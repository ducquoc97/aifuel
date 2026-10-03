//! Anthropic-compatible `POST /v1/messages` and
//! `POST /v1/messages/count_tokens`: run the resolved attempt chain
//! through `execute` and answer in Messages shape - `event:`-named SSE
//! frames for `stream: true`, one `message` JSON otherwise.
//! `count_tokens` answers the documented estimate with no provider round
//! trip.
//!
//! Anthropic `system` + `messages[]` map onto the `flatten` transcript
//! convention: `system` leads as instruction, `tool_use` becomes an
//! assistant `tool_calls` turn and `tool_result` a `tool` turn so calls
//! keep their handles, and media/unknown blocks leave the
//! `[<type> part omitted]` marker. Declared `tools`, `tool_choice`,
//! `thinking`, `metadata`, `stop_sequences`, and `max_tokens` are
//! tolerated and ignored - the run contract is prompt-only, so client
//! tool definitions do not round-trip yet, like `responses`.
//!
//! Token accounting: the Anthropic `usage` shape requires ints, never
//! null. Counts the provider reported pass through; the rest answer the
//! documented estimate - ceil(chars / 4), see `estimate_tokens` - rather
//! than a zero that would read as "free".
//!
//! This module owns the request mapping and the Anthropic error
//! envelope; the serve/stream half lives in `messages_serve`.

use super::execute;
use super::flatten::flatten_messages;
use super::types::ChatMessage;
use super::{Gateway, cors_headers, logs, read_body, respond};
use aifuel_app::MonitoringFacade;
use aifuel_core::StatusCollector;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

// A plain `mod messages_serve;` inside this non-`mod.rs` file would
// resolve under `gateway/messages/`; `#[path]` keeps the serve half a
// sibling beside this file.
#[path = "messages_serve.rs"]
mod messages_serve;

/// `POST /v1/messages` request subset the gateway consumes. `max_tokens`
/// is declared because the Messages contract requires it, then ignored
/// like every sampling knob; `tools`, `tool_choice`, `thinking`,
/// `metadata`, `stop_sequences` and friends are tolerated the way
/// `ChatRequest` tolerates extras.
#[derive(Debug, Deserialize)]
struct MessagesRequest {
    /// The routing selector; see the `crate::gateway` module docs for
    /// the addressing convention. Required by the Messages contract.
    model: Option<String>,
    /// Leading instruction: a string or an array of `text` blocks.
    #[serde(default)]
    system: Option<Value>,
    #[serde(default)]
    messages: Vec<AnthropicMessage>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    #[allow(dead_code)]
    max_tokens: Option<Value>,
}

/// One inbound message: `role` is `user` or `assistant`, and `content`
/// is a string or a block array - kept raw like `ChatMessage.content`
/// because `push_blocks` reads the block types itself.
#[derive(Debug, Deserialize)]
struct AnthropicMessage {
    role: String,
    #[serde(default)]
    content: Value,
}

/// Handle one `/v1/messages*` request end to end, like
/// `chat::completions`.
pub(crate) fn handle<C: StatusCollector>(
    mut request: tiny_http::Request,
    gateway: &Gateway,
    facade: &MonitoringFacade<C>,
    runtime: &tokio::runtime::Runtime,
) {
    let count_tokens = request
        .url()
        .split('?')
        .next()
        .is_some_and(|path| path.ends_with("/count_tokens"));
    let body = match read_body(&mut request) {
        Ok(body) => body,
        Err(message) => return fail(request, "", false, 400, message),
    };
    let parsed: MessagesRequest = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(error) => {
            return fail(
                request,
                "",
                false,
                400,
                &format!("invalid messages request: {error}"),
            );
        }
    };
    let model = parsed
        .model
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_owned();
    if model.is_empty() {
        return fail(request, "", parsed.stream, 400, "model is required");
    }
    if let Err(reason) = super::keys::require_permits(&request, &model) {
        return fail(request, &model, parsed.stream, 403, &reason);
    }
    if parsed.messages.is_empty() {
        return fail(
            request,
            &model,
            parsed.stream,
            400,
            "messages must not be empty",
        );
    }
    let optimize = super::request_plan();
    let prompt = flatten_messages(
        &to_chat_messages(parsed.system.as_ref(), &parsed.messages),
        &optimize,
    );
    if count_tokens {
        // The documented estimate doubles as the token-count answer: no
        // provider round trip, matching the endpoint's contract.
        let usage = json!({"input_tokens": estimate_tokens(&prompt)});
        record(&model, false, 200, None, Some(usage.clone()), None);
        return respond(
            request,
            200,
            serde_json::to_vec(&usage).expect("a token count serializes"),
            Some("application/json"),
            cors_headers(),
        );
    }
    if prompt.trim().is_empty() {
        return fail(
            request,
            &model,
            parsed.stream,
            400,
            "messages contain no usable text; media-only content is not supported",
        );
    }
    let attempts = match execute::resolve_attempts(gateway, &model, &|| {
        Gateway::status(facade, runtime)
    }) {
        Ok(attempts) => attempts,
        Err((status, message)) => return fail(request, &model, parsed.stream, status, &message),
    };
    messages_serve::serve(request, gateway, &model, &prompt, parsed.stream, attempts);
}

/// Map Anthropic `system` + `messages[]` onto the transcript
/// `flatten_messages` reads: `system` leads as instruction, block-array
/// content walks [`push_blocks`], and everything else passes through so
/// a lone user message still flattens verbatim.
fn to_chat_messages(system: Option<&Value>, messages: &[AnthropicMessage]) -> Vec<ChatMessage> {
    let mut transcript = Vec::new();
    if let Some(system) = system {
        transcript.push(chat_message("system", system.clone()));
    }
    for message in messages {
        match &message.content {
            Value::Array(blocks) => push_blocks(&mut transcript, &message.role, blocks),
            content => transcript.push(chat_message(&message.role, content.clone())),
        }
    }
    transcript
}

/// Walk one block-array `content` preserving order: text-ish parts
/// accumulate into a multipart message of the original role; each run of
/// `tool_use` blocks becomes an assistant `tool_calls` turn; each
/// `tool_result` becomes a `tool` turn keyed on its `tool_use_id`. The
/// flushes keep interleaved order, so `[text, tool_use, text]` lands as
/// three turns, not two merged ones.
fn push_blocks(transcript: &mut Vec<ChatMessage>, role: &str, blocks: &[Value]) {
    let mut parts: Vec<Value> = Vec::new();
    let mut calls: Vec<Value> = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                flush_parts(transcript, role, &mut parts);
                calls.push(tool_call(block));
            }
            Some("tool_result") => {
                flush_parts(transcript, role, &mut parts);
                flush_calls(transcript, &mut calls);
                let mut turn =
                    chat_message("tool", block.get("content").cloned().unwrap_or(Value::Null));
                turn.tool_call_id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                transcript.push(turn);
            }
            _ => {
                // Any non-tool block ends a run of calls so later text
                // cannot merge ahead of it.
                flush_calls(transcript, &mut calls);
                parts.push(block.clone());
            }
        }
    }
    flush_parts(transcript, role, &mut parts);
    flush_calls(transcript, &mut calls);
}

/// Emit accumulated content parts as one multipart message, if any.
fn flush_parts(transcript: &mut Vec<ChatMessage>, role: &str, parts: &mut Vec<Value>) {
    if !parts.is_empty() {
        transcript.push(chat_message(role, Value::Array(std::mem::take(parts))));
    }
}

/// Emit accumulated `tool_use` blocks as one assistant `tool_calls`
/// turn. A `tool_use` is assistant output by definition in the Messages
/// contract, so the turn is pinned to `assistant` whichever envelope it
/// arrived in.
fn flush_calls(transcript: &mut Vec<ChatMessage>, calls: &mut Vec<Value>) {
    if !calls.is_empty() {
        let mut turn = chat_message("assistant", Value::Null);
        turn.tool_calls = Some(Value::Array(std::mem::take(calls)));
        transcript.push(turn);
    }
}

/// One `tool_use` block as the OpenAI `tool_calls` entry `flatten`
/// renders on the `Tool calls:` line - id, name, and input stay visible
/// so the transcript keeps its handles, like `responses` maps Codex
/// calls.
fn tool_call(block: &Value) -> Value {
    json!({
        "id": block.get("id"),
        "type": "function",
        "function": {
            "name": block.get("name"),
            "arguments": block.get("input"),
        },
    })
}

fn chat_message(role: &str, content: Value) -> ChatMessage {
    ChatMessage {
        role: role.to_owned(),
        content,
        name: None,
        tool_calls: None,
        tool_call_id: None,
    }
}

/// Answer a pre-commit failure with the Anthropic error envelope and log
/// it: a rejected request is terminal like any run outcome.
fn fail(request: tiny_http::Request, model: &str, stream: bool, status: u16, message: &str) {
    record(model, stream, status, None, None, Some(message.to_owned()));
    respond(
        request,
        status,
        serde_json::to_vec(&error_body(status, message)).expect("an error envelope serializes"),
        Some("application/json"),
        cors_headers(),
    );
}

/// The Anthropic error envelope: `{"type":"error","error":{...}}`. SDKs
/// branch on `error.type`, so an OpenAI-shaped body would read as a
/// transport failure rather than the request's real verdict.
fn error_body(status: u16, message: &str) -> Value {
    json!({
        "type": "error",
        "error": {"type": error_type(status), "message": message},
    })
}

/// The Anthropic error `type` matching an HTTP status.
fn error_type(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        _ => "api_error",
    }
}

/// The documented token estimate: ceil(chars / 4), the standard
/// characters-per-token heuristic. Chars, not bytes, so non-ASCII text
/// does not inflate the count. It serves `count_tokens` and every usage
/// field a provider left unreported.
fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

/// Record the terminal outcome in the request log.
fn record(
    model_echo: &str,
    stream: bool,
    status: u16,
    integration: Option<String>,
    usage: Option<Value>,
    error: Option<String>,
) {
    logs::record(logs::Entry {
        ts_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        model: model_echo.to_owned(),
        integration,
        status,
        stream,
        usage,
        error,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The request must decode the Messages contract - `max_tokens`
    /// required upstream, `system` as string or blocks - and tolerate the
    /// broad parameter sets hosted apps send: strict decoding would 400
    /// requests a provider could have served.
    #[test]
    fn request_decodes_the_contract_and_tolerates_extras() {
        let parsed: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-stub-4",
            "max_tokens": 1024,
            "system": [{"type": "text", "text": "be terse"}],
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "lookup", "input_schema": {}}],
            "tool_choice": {"type": "auto"},
            "metadata": {"user_id": "u"},
            "thinking": {"type": "enabled", "budget_tokens": 512},
            "stop_sequences": ["\n"],
            "stream": true,
        }))
        .expect("the request parses");
        assert!(parsed.stream);
        let flat = flatten_messages(
            &to_chat_messages(parsed.system.as_ref(), &parsed.messages),
            &Default::default(),
        );
        assert_eq!(flat, "be terse\n\nUser: hi");
    }

    /// A lone user message maps to its text verbatim - the single-turn
    /// case stays byte-exact like every other `/v1` surface.
    #[test]
    fn a_lone_user_message_passes_through_verbatim() {
        let parsed: MessagesRequest = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Explain ownership"}],
        }))
        .unwrap();
        assert_eq!(
            flatten_messages(
                &to_chat_messages(parsed.system.as_ref(), &parsed.messages),
                &Default::default()
            ),
            "Explain ownership"
        );
    }

    /// `tool_use`/`tool_result` must stay readable as tool turns with
    /// their ids, not collapse into bare text: a transcript the model
    /// cannot attribute is worse than an honest omission marker.
    #[test]
    fn tool_turns_keep_their_handles() {
        let parsed: MessagesRequest = serde_json::from_value(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "check"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "looking"},
                    {"type": "tool_use", "id": "tu_1", "name": "lookup", "input": {"q": "x"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_1", "content": "found it"},
                ]},
            ],
        }))
        .unwrap();
        let flat = flatten_messages(
            &to_chat_messages(parsed.system.as_ref(), &parsed.messages),
            &Default::default(),
        );
        assert!(flat.contains("Tool calls:"), "{flat}");
        assert!(flat.contains("tu_1") && flat.contains("lookup"), "{flat}");
        assert!(flat.contains("Tool (tu_1): found it"), "{flat}");
    }

    /// `[text, tool_use, text]` in one assistant message must not merge
    /// the texts across the call: the transcript reads in the order it
    /// happened.
    #[test]
    fn tool_calls_split_text_in_order() {
        let parsed: MessagesRequest = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "assistant", "content": [
                {"type": "text", "text": "first"},
                {"type": "tool_use", "id": "tu_1", "name": "lookup", "input": {}},
                {"type": "text", "text": "second"},
            ]}],
        }))
        .unwrap();
        let flat = flatten_messages(
            &to_chat_messages(None, &parsed.messages),
            &Default::default(),
        );
        let (first, calls, second) = (
            flat.find("first").expect("first text"),
            flat.find("Tool calls:").expect("a calls block"),
            flat.find("second").expect("second text"),
        );
        assert!(first < calls && calls < second, "{flat}");
    }

    /// Media blocks the run contract cannot carry leave the visible
    /// omission marker `flatten` defined rather than vanishing.
    #[test]
    fn image_blocks_leave_the_omitted_marker() {
        let parsed: MessagesRequest = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "what is "},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "..."}},
                {"type": "text", "text": "showing?"},
            ]}],
        }))
        .unwrap();
        assert_eq!(
            flatten_messages(
                &to_chat_messages(None, &parsed.messages),
                &Default::default()
            ),
            "what is [image part omitted]showing?"
        );
    }

    /// The estimate is ceil(chars/4) - chars, not bytes, so a CJK-heavy
    /// prompt does not bill at three times its token count.
    #[test]
    fn estimate_is_ceil_chars_over_four() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens("あいうえ"), 1);
    }

    /// The error envelope must be Anthropic's `{"type":"error"}` with the
    /// status-matched `error.type` - an OpenAI-shaped body would read as
    /// a transport failure to Anthropic SDKs.
    #[test]
    fn error_envelope_is_anthropic_shaped() {
        assert_eq!(
            error_body(400, "bad"),
            json!({"type": "error", "error": {"type": "invalid_request_error", "message": "bad"}})
        );
        assert_eq!(
            error_body(401, "x")["error"]["type"],
            "authentication_error"
        );
        assert_eq!(error_body(404, "x")["error"]["type"], "not_found_error");
        assert_eq!(error_body(502, "x")["error"]["type"], "api_error");
    }
}
