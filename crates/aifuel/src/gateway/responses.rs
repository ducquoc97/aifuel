//! OpenAI Responses API: `POST /v1/responses` and
//! `POST /v1/responses/compact` - the wire surface a Codex CLI configured
//! with `wire_api = "responses"` speaks.
//!
//! `input` items flatten into the same single-prompt transcript
//! `chat::completions` builds: `instructions` leads as an unlabeled system
//! block, `message` items map by role, `function_call`/`custom_tool_call`
//! and their `*_output` partners keep `call_id`/`name` handles as tool
//! turns, and `reasoning` items drop - the run contract carries one
//! prompt, so provider-side thinking, `store`, `max_output_tokens`, and
//! `reasoning` effort do not round-trip yet. `previous_response_id` and
//! `item_reference` are rejected outright: the gateway stores no
//! responses, so the full input must arrive each turn.
//!
//! Declared `tools` round-trip through the prompt-level emulation the
//! whole `/v1` surface shares: `type: "function"` tools lead the
//! transcript as an `Available tools:` preamble (`flatten_tools`), the
//! model answers with a fenced `aifuel_tool_calls` block, and
//! `responses_serve` recovers it as `function_call` output items - the
//! loop Codex needs, since it executes every tool client-side. Custom
//! (grammar) and hosted tool kinds are not declared: their input
//! contract is not JSON arguments.
//!
//! `POST /v1/responses/compact` serves Codex's context compaction: the
//! flattened input runs once under a summary instruction and answers as a
//! non-streamed Response object. It is a prompt-level approximation of
//! the proprietary compact endpoint - the same transcript a model would
//! summarize, without provider-side compaction state.

use super::execute;
use super::flatten::{flatten_messages, flatten_tools};
use super::types::{ChatMessage, ToolFunction};
use super::{Gateway, logs, read_body, respond_error};
use aifuel_app::MonitoringFacade;
use aifuel_core::StatusCollector;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

// A plain `mod responses_serve;` inside this non-`mod.rs` file would
// resolve under `gateway/responses/`; `#[path]` keeps the serve half a
// flat sibling like the other gateway modules.
#[path = "responses_serve.rs"]
mod responses_serve;

/// The instruction a compact run answers under; the flattened
/// conversation transcript follows it verbatim.
const COMPACT_INSTRUCTION: &str = "Compact this conversation into a dense \
    summary preserving decisions, file paths, and pending tasks.";

/// `POST /v1/responses*` request subset the gateway consumes. Fields
/// beyond these - `tools`, `tool_choice`, `store`, `reasoning`,
/// `max_output_tokens`, `include`, `metadata` and friends - are tolerated
/// and ignored by design, like `ChatRequest`.
#[derive(Debug, Deserialize)]
struct ResponsesRequest {
    /// The routing selector; see the `crate::gateway` module docs for the
    /// addressing convention. Required by the OpenAI contract.
    model: Option<String>,
    /// A bare prompt string, or an array of typed input items
    /// (`message`, `function_call`, `reasoning`, ...).
    #[serde(default)]
    input: Value,
    /// System instructions leading the transcript.
    #[serde(default)]
    instructions: Option<String>,
    /// Declared tools; only `type: "function"` entries join the preamble.
    #[serde(default)]
    tools: Option<Vec<Value>>,
    #[serde(default)]
    tool_choice: Option<Value>,
    #[serde(default)]
    stream: bool,
    /// Continuation handle for a stored response; rejected because the
    /// gateway stores no responses.
    #[serde(default)]
    previous_response_id: Option<String>,
}

/// Handle one `/v1/responses*` request end to end, like
/// `chat::completions`.
pub(crate) fn handle<C: StatusCollector>(
    mut request: tiny_http::Request,
    gateway: &Gateway,
    facade: &MonitoringFacade<C>,
    runtime: &tokio::runtime::Runtime,
) {
    let compact = request.url().split('?').next() == Some("/v1/responses/compact");
    let body = match read_body(&mut request) {
        Ok(body) => body,
        Err(message) => return fail(request, "", false, 400, message),
    };
    let responses: ResponsesRequest = match serde_json::from_slice(&body) {
        Ok(responses) => responses,
        Err(error) => {
            return fail(
                request,
                "",
                false,
                400,
                &format!("invalid responses request: {error}"),
            );
        }
    };
    let model = responses
        .model
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_owned();
    if model.is_empty() {
        return fail(request, "", responses.stream, 400, "model is required");
    }
    if let Err(reason) = super::keys::require_permits(&request, &model) {
        return fail(request, &model, responses.stream, 403, &reason);
    }
    if let Some(previous) = responses.previous_response_id.as_deref() {
        return fail(
            request,
            &model,
            responses.stream,
            400,
            &format!(
                "previous_response_id {previous:?} is not supported: the gateway stores no responses, so send the full input each turn"
            ),
        );
    }
    if compact && responses.stream {
        return fail(
            request,
            &model,
            true,
            400,
            "/v1/responses/compact does not stream; it answers one Response object",
        );
    }
    let mut messages = Vec::new();
    if compact {
        messages.push(chat_message("system", json!(COMPACT_INSTRUCTION)));
    }
    if let Some(instructions) = &responses.instructions {
        messages.push(chat_message("system", json!(instructions)));
    }
    if let Err(message) = flatten_input(&responses.input, &mut messages) {
        return fail(request, &model, responses.stream, 400, &message);
    }
    let transcript = flatten_messages(&messages);
    if transcript.trim().is_empty() {
        return fail(
            request,
            &model,
            responses.stream,
            400,
            "input contains no usable text; media-only content is not supported",
        );
    }
    // Tool declarations become a prompt preamble - the run contract is
    // prompt-only, so tool calling is emulated: declare the tools, then
    // parse the fenced call block back out of the answer.
    let tools_preamble = flatten_tools(
        &tool_functions(responses.tools.as_deref().unwrap_or(&[])),
        normalized_tool_choice(responses.tool_choice.as_ref()).as_ref(),
    );
    let parse_tools = !tools_preamble.is_empty();
    let prompt = if parse_tools {
        format!("{tools_preamble}\n\n{transcript}")
    } else {
        transcript
    };
    let attempts =
        match execute::resolve_attempts(gateway, &model, &|| Gateway::status(facade, runtime)) {
            Ok(attempts) => attempts,
            Err((status, message)) => {
                return fail(request, &model, responses.stream, status, &message);
            }
        };
    responses_serve::serve(
        request,
        gateway,
        &model,
        &prompt,
        responses.stream,
        attempts,
        parse_tools,
    );
}

/// Answer a pre-commit failure with the OpenAI error envelope and log it:
/// a rejected request is terminal like any run outcome.
fn fail(request: tiny_http::Request, model: &str, stream: bool, status: u16, message: &str) {
    logs::record(logs::Entry {
        ts_unix: unix_secs(),
        model: model.to_owned(),
        integration: None,
        status,
        stream,
        usage: None,
        error: Some(message.to_owned()),
    });
    respond_error(request, status, message, "invalid_request_error");
}

/// The `tools` entries the emulation can declare: only `type: "function"`
/// tools carry the JSON-arguments contract the call block speaks, so
/// `custom` (grammar) and hosted tool kinds are skipped rather than
/// misdeclared.
fn tool_functions(tools: &[Value]) -> Vec<ToolFunction> {
    tools
        .iter()
        .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("function"))
        .filter_map(|tool| {
            let name = tool.get("name").and_then(Value::as_str)?;
            if name.is_empty() {
                return None;
            }
            Some(ToolFunction {
                name: name.to_owned(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                parameters: tool.get("parameters").cloned(),
            })
        })
        .collect()
}

/// `tool_choice` in Responses shape: string choices pass through, and the
/// `{"type": "function", "name": X}` object is re-pointed at the
/// `function.name` path `flatten_tools` reads for a forced call.
fn normalized_tool_choice(choice: Option<&Value>) -> Option<Value> {
    let choice = choice?;
    if choice.get("type").and_then(Value::as_str) == Some("function")
        && let Some(name) = choice.get("name")
    {
        return Some(json!({"function": {"name": name.clone()}}));
    }
    Some(choice.clone())
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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

/// Flatten one Responses `input` value into chat messages for
/// `flatten_messages`: a bare string is the lone user message; an array
/// walks its items through [`push_item`]. A missing `input` flattens to
/// nothing and the caller's empty-prompt check rejects it.
fn flatten_input(input: &Value, messages: &mut Vec<ChatMessage>) -> Result<(), String> {
    match input {
        Value::String(text) => messages.push(chat_message("user", json!(text))),
        Value::Array(items) => {
            for item in items {
                push_item(messages, item)?;
            }
        }
        Value::Null => {}
        _ => return Err("input must be a string or an array of input items".to_owned()),
    }
    Ok(())
}

/// Map one `input` item onto the transcript: message items by role, tool
/// items by `call_id` handle, `reasoning` dropped, `item_reference`
/// rejected, and unknown kinds left as a visible marker so context loss
/// is never silent.
fn push_item(messages: &mut Vec<ChatMessage>, item: &Value) -> Result<(), String> {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => messages.push(message_from_item(item)),
        Some("function_call") | Some("custom_tool_call") => {
            messages.push(tool_call_item(item));
        }
        Some("function_call_output") | Some("custom_tool_call_output") => {
            messages.push(tool_output_item(item));
        }
        Some("reasoning") => {
            // Provider-side thinking carries no text the prompt contract
            // can replay; Codex resends it on every turn.
        }
        Some("item_reference") => {
            return Err(
                "item_reference input items are not supported: the gateway stores no responses, so send the full input each turn"
                    .to_owned(),
            );
        }
        Some(kind) => messages.push(chat_message(
            "user",
            json!(format!("[{kind} item omitted]")),
        )),
        // Untyped `{role, content}` entries are message shorthand.
        None if item.get("role").is_some() => messages.push(message_from_item(item)),
        None => messages.push(chat_message(
            "user",
            json!("[malformed input item omitted]"),
        )),
    }
    Ok(())
}

/// One `message` input item - typed (`type: "message"`) or bare
/// (`{role, content}` shorthand) - mapped by role with its content parts
/// translated through [`content_value`].
fn message_from_item(item: &Value) -> ChatMessage {
    let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
    chat_message(role, content_value(item.get("content")))
}

/// One `function_call`/`custom_tool_call` item as an assistant tool turn
/// keeping its `call_id`/`name` handle. `custom_tool_call` carries the
/// payload in `input` where `function_call` uses `arguments`.
fn tool_call_item(item: &Value) -> ChatMessage {
    ChatMessage {
        role: "assistant".to_owned(),
        content: Value::Null,
        name: None,
        tool_call_id: None,
        tool_calls: Some(json!([{
            "id": item.get("call_id"),
            "type": "function",
            "function": {
                "name": item.get("name"),
                "arguments": item.get("arguments").or_else(|| item.get("input")),
            },
        }])),
    }
}

/// One `function_call_output`/`custom_tool_call_output` item as a tool
/// turn keyed by its `call_id`.
fn tool_output_item(item: &Value) -> ChatMessage {
    ChatMessage {
        role: "tool".to_owned(),
        content: content_value(item.get("output")),
        name: None,
        tool_calls: None,
        tool_call_id: item
            .get("call_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

/// Translate a Responses `content`/`output` value into the multipart
/// shape `flatten` reads: text-bearing parts (`input_text`,
/// `output_text`, `refusal`) become `text` parts; everything else passes
/// through so `flatten` leaves its `[<type> part omitted]` marker.
fn content_value(content: Option<&Value>) -> Value {
    match content {
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| match part.get("type").and_then(Value::as_str) {
                Some("input_text") | Some("output_text") => {
                    json!({"type": "text", "text": part.get("text").cloned().unwrap_or_default()})
                }
                Some("refusal") => {
                    json!({"type": "text", "text": part.get("refusal").cloned().unwrap_or_default()})
                }
                _ => part.clone(),
            })
            .collect(),
        Some(value) => value.clone(),
        None => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Codex sends `input` as typed items; a bare string is the lone-prompt
    /// shorthand. Both must land as the user's text, and media parts must
    /// leave the visible marker `flatten` defined rather than vanish.
    #[test]
    fn input_string_and_message_items_flatten_to_prompt_text() {
        let mut messages = Vec::new();
        flatten_input(&json!("hello"), &mut messages).unwrap();
        assert_eq!(flatten_messages(&messages), "hello");

        let mut messages = Vec::new();
        flatten_input(
            &json!([
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "What is "},
                    {"type": "input_image", "image_url": "data:..."},
                    {"type": "input_text", "text": "this?"},
                ]},
                {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "An owl."},
                ]},
            ]),
            &mut messages,
        )
        .unwrap();
        assert_eq!(
            flatten_messages(&messages),
            "User: What is [input_image part omitted]this?\n\nAssistant: An owl."
        );
    }

    #[test]
    fn tool_items_keep_their_handles() {
        // Codex replays calls as function_call/custom_tool_call items; the
        // model must still see which call produced which output.
        let mut messages = Vec::new();
        flatten_input(
            &json!([
                {"type": "function_call", "call_id": "call_1", "name": "shell", "arguments": "{\"cmd\":\"ls\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "a.rs"},
                {"type": "custom_tool_call", "call_id": "call_2", "name": "apply_patch", "input": "*** patch"},
                {"type": "custom_tool_call_output", "call_id": "call_2", "output": "ok"},
            ]),
            &mut messages,
        )
        .unwrap();
        let flat = flatten_messages(&messages);
        assert!(flat.contains("Tool calls:") && flat.contains("call_1") && flat.contains("shell"));
        assert!(flat.contains("Tool (call_1): a.rs"));
        assert!(flat.contains("apply_patch") && flat.contains("Tool (call_2): ok"));
    }

    #[test]
    fn reasoning_is_dropped_and_item_reference_is_rejected() {
        // Stored-response handles cannot be replayed because nothing is
        // stored; reasoning has no replayable text either.
        let mut messages = Vec::new();
        flatten_input(
            &json!([{"type": "reasoning", "summary": []}]),
            &mut messages,
        )
        .unwrap();
        assert!(messages.is_empty());
        assert!(
            flatten_input(
                &json!([{"type": "item_reference", "id": "msg_1"}]),
                &mut messages
            )
            .is_err()
        );
    }

    #[test]
    fn unknown_items_leave_a_visible_marker() {
        let mut messages = Vec::new();
        flatten_input(
            &json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "web_search_call", "status": "completed"},
            ]),
            &mut messages,
        )
        .unwrap();
        assert!(flatten_messages(&messages).contains("[web_search_call item omitted]"));
    }

    #[test]
    fn function_tools_become_the_preamble_and_named_choice_forces_a_call() {
        // The emulation contract only speaks JSON-arguments function
        // tools; custom (grammar) and hosted kinds must not be declared
        // at all, and a named function choice must force the call.
        let raw = json!([
            {"type": "function", "name": "shell", "description": "run a command", "parameters": {"type": "object"}},
            {"type": "custom", "name": "apply_patch", "description": "patch files"},
            {"type": "web_search"},
        ]);
        let tools = tool_functions(raw.as_array().unwrap());
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "shell");

        let choice = normalized_tool_choice(Some(&json!({"type": "function", "name": "shell"})));
        let preamble = flatten_tools(&tools, choice.as_ref());
        assert!(preamble.contains("- shell: run a command"));
        assert!(preamble.contains("must be a call to the tool `shell`"));
    }

    #[test]
    fn codex_requests_parse_with_extra_fields() {
        // Codex sends a broad parameter set; strict decoding would 400
        // requests the chain could have served.
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5", "store": false, "stream": true,
            "instructions": "be terse",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "tools": [{"type": "function", "name": "shell"}],
            "tool_choice": "auto", "parallel_tool_calls": false,
            "reasoning": {"effort": "high", "summary": "auto"},
            "include": [], "prompt_cache_key": "k", "text": {"format": {"type": "text"}},
        }))
        .unwrap();
        assert_eq!(request.model.as_deref(), Some("gpt-5"));
        assert!(request.stream);
    }
}
