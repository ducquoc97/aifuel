//! `POST /v1/chat/completions`: run the resolved attempt chain through
//! `execute` and answer in OpenAI Chat Completions shape - SSE
//! `chat.completion.chunk` frames for `stream: true`, one
//! `chat.completion` JSON otherwise.
//!
//! This module owns only the wire format; the attempt chain, failover,
//! and cancellation machinery lives in `execute`.

use super::execute::{self, Feed, Flow, Outcome};
use super::flatten::{ParsedAnswer, flatten_messages, flatten_tools, parse_tool_calls};
use super::types::ChatRequest;
use super::{Gateway, cors_headers, logs, read_body, respond, respond_error};
use aifuel_app::MonitoringFacade;
use aifuel_core::{StatusCollector, TokenUsage};
use serde_json::{Value, json};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

const SSE_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nAccess-Control-Allow-Origin: *\r\n\r\n";

/// Handle one `POST /v1/chat/completions` request end to end.
pub(crate) fn completions<C: StatusCollector>(
    mut request: tiny_http::Request,
    gateway: &Gateway,
    facade: &MonitoringFacade<C>,
    runtime: &tokio::runtime::Runtime,
) {
    let body = match read_body(&mut request) {
        Ok(body) => body,
        Err(message) => {
            respond_error(request, 400, message, "invalid_request_error");
            return;
        }
    };
    let chat: ChatRequest = match serde_json::from_slice(&body) {
        Ok(chat) => chat,
        Err(error) => {
            respond_error(
                request,
                400,
                &format!("invalid chat completion request: {error}"),
                "invalid_request_error",
            );
            return;
        }
    };
    let model = chat.model.as_deref().unwrap_or_default().trim();
    if model.is_empty() {
        respond_error(request, 400, "model is required", "invalid_request_error");
        return;
    }
    if let Err(reason) = super::keys::require_permits(&request, model) {
        respond_error(request, 403, &reason, "permission_error");
        return;
    }
    if chat.messages.is_empty() {
        respond_error(
            request,
            400,
            "messages must not be empty",
            "invalid_request_error",
        );
        return;
    }
    // The providers.json optimizer plan applies to this request: `rtk`
    // compresses tool-role content at flatten, and `execute::run` loads
    // the same plan so wire integrations receive the caveman instruction.
    let optimize = super::request_plan();
    let transcript = flatten_messages(&chat.messages, &optimize);
    if transcript.trim().is_empty() {
        respond_error(
            request,
            400,
            "messages contain no usable text; media-only content is not supported",
            "invalid_request_error",
        );
        return;
    }
    // Tool declarations become a prompt preamble - the run contract is
    // prompt-only, so tool calling is emulated: declare the tools, then
    // parse the fenced call block back out of the answer.
    let tools_preamble = flatten_tools(&chat.tool_functions(), chat.tool_choice().as_ref());
    let parse_tools = !tools_preamble.is_empty();
    let prompt = if parse_tools {
        format!("{tools_preamble}\n\n{transcript}")
    } else {
        transcript
    };
    let attempts =
        match execute::resolve_attempts(gateway, model, &|| Gateway::status(facade, runtime)) {
            Ok(attempts) => attempts,
            Err((status, message)) => {
                respond_error(request, status, &message, "invalid_request_error");
                return;
            }
        };
    serve(
        request,
        gateway,
        &chat,
        model,
        &prompt,
        attempts,
        parse_tools,
    );
}

/// Feed the chain's deltas into `chat.completion.chunk` frames, or buffer
/// them for the one-shot JSON when `stream` is false. `parse_tools` marks
/// a request that declared tools: its answer may carry the fenced
/// `aifuel_tool_calls` block, so deltas are buffered (never streamed
/// mid-run) and the parsed result goes out as the tail chunks - a partial
/// block must not reach the client as text.
fn serve(
    request: tiny_http::Request,
    gateway: &Gateway,
    chat: &ChatRequest,
    model_echo: &str,
    prompt: &str,
    attempts: Vec<execute::Attempt>,
    parse_tools: bool,
) {
    let completion_id = format!(
        "chatcmpl-{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let include_usage = chat
        .stream_options
        .as_ref()
        .and_then(|options| options.include_usage)
        .unwrap_or(false);
    let stream = chat.stream;

    let mut slot = Some(request);
    let mut writer: Option<Box<dyn Write + Send>> = None;
    let mut collected = String::new();
    let outcome = execute::run(gateway, attempts, prompt, &mut |feed| match feed {
        Feed::Delta(delta) => {
            collected.push_str(delta);
            if !stream || parse_tools {
                return Flow::Continue;
            }
            if writer.is_none()
                && commit(&mut slot, &mut writer, &completion_id, created, model_echo).is_err()
            {
                return Flow::Stop;
            }
            let frame = chunk_frame(
                &completion_id,
                created,
                model_echo,
                json!({"content": delta}),
                None,
            );
            if write_all(
                writer.as_mut().expect("commit stores the writer"),
                frame.as_bytes(),
            )
            .is_ok()
            {
                Flow::Continue
            } else {
                Flow::Stop
            }
        }
        Feed::Keepalive => {
            if let Some(writer) = writer.as_mut()
                && writer
                    .write_all(b": keep-alive\n\n")
                    .and_then(|()| writer.flush())
                    .is_err()
            {
                return Flow::Stop;
            }
            Flow::Continue
        }
    });

    match outcome {
        Outcome::Success { text, result } => {
            logs::record(logs::Entry {
                ts_unix: unix_secs(),
                model: model_echo.to_owned(),
                integration: Some(result.integration_id.to_string()),
                status: 200,
                stream,
                usage: result.usage.as_ref().map(|usage| {
                    json!({
                        "prompt_tokens": usage.input_tokens,
                        "completion_tokens": usage.output_tokens,
                        "total_tokens": usage.input_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0),
                    })
                }),
                error: None,
            });
            // Only a request that declared tools can yield the fenced
            // call block; other answers pass through unparsed.
            let parsed = if parse_tools {
                parse_tool_calls(&text)
            } else {
                None
            };
            if let Some(writer) = writer.as_mut() {
                // Deltas already streamed: close out the committed
                // response. Reaching here means parse_tools was off, so
                // the finish is always a plain stop.
                let _ = write_terminal(
                    writer,
                    "stop",
                    result.usage.as_ref(),
                    include_usage,
                    &completion_id,
                    created,
                    model_echo,
                )
                .and_then(|()| write_all(writer, b"data: [DONE]\n\n"));
            } else if stream {
                // The answer arrived whole (a non-streaming adapter, or a
                // tool request buffering for the call block): commit now
                // and replay it as one chunk sequence.
                if commit(&mut slot, &mut writer, &completion_id, created, model_echo).is_err() {
                    return;
                }
                let writer = writer.as_mut().expect("commit stores the writer");
                let finish_reason = match &parsed {
                    Some(answer) => {
                        if !answer.content.is_empty() {
                            let _ = write_all(
                                writer,
                                chunk_frame(
                                    &completion_id,
                                    created,
                                    model_echo,
                                    json!({"content": answer.content}),
                                    None,
                                )
                                .as_bytes(),
                            );
                        }
                        let _ = write_all(
                            writer,
                            chunk_frame(
                                &completion_id,
                                created,
                                model_echo,
                                json!({"tool_calls": answer.calls_json(true)}),
                                None,
                            )
                            .as_bytes(),
                        );
                        "tool_calls"
                    }
                    None => {
                        if !text.is_empty() {
                            let _ = write_all(
                                writer,
                                chunk_frame(
                                    &completion_id,
                                    created,
                                    model_echo,
                                    json!({"content": text}),
                                    None,
                                )
                                .as_bytes(),
                            );
                        }
                        "stop"
                    }
                };
                let _ = write_terminal(
                    writer,
                    finish_reason,
                    result.usage.as_ref(),
                    include_usage,
                    &completion_id,
                    created,
                    model_echo,
                )
                .and_then(|()| write_all(writer, b"data: [DONE]\n\n"));
            } else {
                let request = slot.take().expect("an uncommitted request is present");
                respond(
                    request,
                    200,
                    serde_json::to_vec(&completion_body(
                        &completion_id,
                        created,
                        model_echo,
                        &text,
                        parsed.as_ref(),
                        result.usage.as_ref(),
                    ))
                    .expect("a completion body serializes"),
                    Some("application/json"),
                    cors_headers(),
                );
            }
        }
        Outcome::Failed {
            committed: true,
            status,
            message,
        } => {
            logs::record(logs::Entry {
                ts_unix: unix_secs(),
                model: model_echo.to_owned(),
                integration: None,
                status,
                stream,
                usage: None,
                error: Some(message.clone()),
            });
            if let Some(writer) = writer.as_mut() {
                let _ = write_error_frame(writer, &message);
            } else if let Some(request) = slot.take() {
                // Deltas reached the sink but were buffered for tool
                // parsing and never committed, so the request is still
                // answerable with a normal error response.
                respond_error(request, status, &message, "server_error");
            }
        }
        Outcome::Failed {
            status, message, ..
        } => {
            logs::record(logs::Entry {
                ts_unix: unix_secs(),
                model: model_echo.to_owned(),
                integration: None,
                status,
                stream,
                usage: None,
                error: Some(message.clone()),
            });
            if let Some(request) = slot.take() {
                respond_error(request, status, &message, "server_error");
            }
        }
        Outcome::Aborted => {
            logs::record(logs::Entry {
                ts_unix: unix_secs(),
                model: model_echo.to_owned(),
                integration: None,
                status: 499,
                stream,
                usage: None,
                error: Some("client disconnected".to_owned()),
            });
        }
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Write the SSE head and the assistant role chunk - the point of no
/// return after which the attempt owns the stream.
fn commit(
    slot: &mut Option<tiny_http::Request>,
    writer: &mut Option<Box<dyn Write + Send>>,
    completion_id: &str,
    created: u64,
    model_echo: &str,
) -> Result<(), ()> {
    let request = slot.take().ok_or(())?;
    let mut w = request.into_writer();
    write_all(&mut w, SSE_HEAD.as_bytes())?;
    let role = chunk_frame(
        completion_id,
        created,
        model_echo,
        json!({"role": "assistant", "content": ""}),
        None,
    );
    write_all(&mut w, role.as_bytes())?;
    *writer = Some(w);
    Ok(())
}

/// The terminal chunk carrying `finish_reason` (`"stop"` or
/// `"tool_calls"`) plus the spec-shaped usage chunk when the caller
/// asked for it.
fn write_terminal(
    writer: &mut Box<dyn Write + Send>,
    finish_reason: &str,
    usage: Option<&TokenUsage>,
    include_usage: bool,
    completion_id: &str,
    created: u64,
    model_echo: &str,
) -> Result<(), ()> {
    let finish = chunk_frame(
        completion_id,
        created,
        model_echo,
        json!({}),
        Some(finish_reason),
    );
    write_all(writer, finish.as_bytes())?;
    if include_usage {
        let usage_frame = usage_chunk(completion_id, created, model_echo, usage);
        write_all(writer, usage_frame.as_bytes())?;
    }
    Ok(())
}

/// An SSE error frame for a failure after the stream committed, then the
/// `[DONE]` sentinel clients expect.
fn write_error_frame(writer: &mut Box<dyn Write + Send>, message: &str) -> Result<(), ()> {
    let frame = format!(
        "data: {}\n\n",
        json!({"error": {"message": message, "type": "server_error", "param": null, "code": null}})
    );
    write_all(writer, frame.as_bytes())?;
    write_all(writer, b"data: [DONE]\n\n")
}

fn write_all(writer: &mut (impl Write + ?Sized), bytes: &[u8]) -> Result<(), ()> {
    writer
        .write_all(bytes)
        .and_then(|()| writer.flush())
        .map_err(|_| ())
}

/// One `chat.completion.chunk` SSE frame.
fn chunk_frame(
    completion_id: &str,
    created: u64,
    model_echo: &str,
    delta: Value,
    finish_reason: Option<&str>,
) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": completion_id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model_echo,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
        })
    )
}

/// The final `{"choices": [], "usage": ...}` chunk shape
/// `stream_options.include_usage` callers wait on. `usage` is null when the
/// provider reported none - missing counts stay unknown, never zero.
fn usage_chunk(
    completion_id: &str,
    created: u64,
    model_echo: &str,
    usage: Option<&TokenUsage>,
) -> String {
    let usage = usage.map(|usage| {
        json!({
            "prompt_tokens": usage.input_tokens,
            "completion_tokens": usage.output_tokens,
            "total_tokens": usage.input_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0),
        })
    });
    format!(
        "data: {}\n\n",
        json!({
            "id": completion_id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model_echo,
            "choices": [],
            "usage": usage,
        })
    )
}

/// The non-streamed `chat.completion` body. A parsed tool-call block
/// becomes `message.tool_calls` with `finish_reason "tool_calls"`, and
/// `content` is the text outside the block - null when the model left
/// nothing else. Anything unparseable stays a plain stop answer.
fn completion_body(
    completion_id: &str,
    created: u64,
    model_echo: &str,
    answer: &str,
    parsed: Option<&ParsedAnswer>,
    usage: Option<&TokenUsage>,
) -> Value {
    let (message, finish_reason) = match parsed {
        Some(answer) => (
            json!({
                "role": "assistant",
                "content": if answer.content.is_empty() {
                    Value::Null
                } else {
                    json!(answer.content)
                },
                "tool_calls": answer.calls_json(false),
            }),
            "tool_calls",
        ),
        None => (json!({"role": "assistant", "content": answer}), "stop"),
    };
    json!({
        "id": completion_id,
        "object": "chat.completion",
        "created": created,
        "model": model_echo,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
        }],
        "usage": usage.map(|usage| json!({
            "prompt_tokens": usage.input_tokens,
            "completion_tokens": usage.output_tokens,
            "total_tokens": usage.input_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0),
        })),
    })
}

impl ParsedAnswer {
    /// The OpenAI `tool_calls` array. Streamed deltas key each entry by
    /// `index`; the non-streamed message form omits it.
    fn calls_json(&self, indexed: bool) -> Vec<Value> {
        self.calls
            .iter()
            .enumerate()
            .map(|(index, call)| {
                let mut value = json!({
                    "id": format!("call_{index}"),
                    "type": "function",
                    "function": {"name": call.name, "arguments": call.arguments},
                });
                if indexed {
                    value["index"] = json!(index);
                }
                value
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::flatten::ParsedCall;

    /// The SSE frames must match the shape OpenAI SDKs and litellm parse:
    /// `data:` prefix, `chat.completion.chunk` object, `choices[].delta`,
    /// and a `choices: []` usage chunk before `[DONE]` - Cline historically
    /// crashed on usage chunks carrying malformed choices, and several
    /// clients key usage accounting off this exact shape.
    #[test]
    fn chunk_frames_are_openai_shaped() {
        let delta = chunk_frame("chatcmpl-1", 10, "auto", json!({"content": "hi"}), None);
        let frame: Value = serde_json::from_str(delta.trim_start_matches("data: ").trim())
            .expect("a chunk frame is json");
        assert_eq!(frame["object"], "chat.completion.chunk");
        assert_eq!(frame["choices"][0]["delta"]["content"], "hi");
        assert!(delta.ends_with("\n\n"));

        let usage = usage_chunk(
            "chatcmpl-1",
            10,
            "auto",
            Some(&TokenUsage {
                input_tokens: Some(3),
                output_tokens: Some(4),
            }),
        );
        let frame: Value = serde_json::from_str(usage.trim_start_matches("data: ").trim())
            .expect("a usage frame is json");
        assert_eq!(frame["choices"], json!([]));
        assert_eq!(frame["usage"]["total_tokens"], 7);
    }

    #[test]
    fn usage_chunk_reports_unknown_as_null() {
        // A provider that reported nothing must not fabricate zeroes.
        let frame = usage_chunk("chatcmpl-1", 10, "auto", None);
        let parsed: Value =
            serde_json::from_str(frame.trim_start_matches("data: ").trim()).unwrap();
        assert_eq!(parsed["usage"], Value::Null);
    }

    #[test]
    fn completion_body_is_openai_shaped() {
        let body = completion_body("chatcmpl-1", 10, "auto", "done", None, None);
        assert_eq!(body["object"], "chat.completion");
        assert_eq!(body["choices"][0]["message"]["content"], "done");
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
        assert_eq!(body["usage"], Value::Null);
    }

    #[test]
    fn fenced_block_parses_into_calls() {
        // The whole emulation hinges on this round-trip: the preamble
        // tells the model one fenced block, and a compliant answer must
        // come back as calls plus whatever text surrounded it.
        let answer = "checking first\n\n```aifuel_tool_calls\n{\"calls\":[{\"name\":\"lookup\",\"arguments\":{\"q\":\"rust\"}}]}\n```\n";
        let parsed = parse_tool_calls(answer).expect("a well-formed block parses");
        assert_eq!(parsed.content, "checking first");
        assert_eq!(parsed.calls.len(), 1);
        assert_eq!(parsed.calls[0].name, "lookup");
        assert_eq!(parsed.calls[0].arguments, r#"{"q":"rust"}"#);
    }

    #[test]
    fn block_only_answer_leaves_no_content() {
        let answer =
            "```aifuel_tool_calls\n{\"calls\":[{\"name\":\"lookup\",\"arguments\":{}}]}\n```";
        let parsed = parse_tool_calls(answer).expect("a bare block parses");
        assert!(parsed.content.is_empty());
        assert_eq!(parsed.calls[0].arguments, "{}");
    }

    #[test]
    fn malformed_blocks_fall_back_to_plain_content() {
        // A model that half-follows the contract - truncated fence, bad
        // JSON, empty calls - must not lose its answer: the gateway
        // passes the text through rather than emitting a broken call.
        for answer in [
            "```aifuel_tool_calls\n{\"calls\":[{\"name\":\"x\"}]",
            "```aifuel_tool_calls\nnot json\n```",
            "```aifuel_tool_calls\n{\"calls\":[]}\n```",
            "```aifuel_tool_calls\n{\"calls\":[{\"arguments\":{}}]}\n```",
            "plain text, no fence",
        ] {
            assert!(parse_tool_calls(answer).is_none(), "{answer:?}");
        }
    }

    #[test]
    fn completion_body_carries_tool_calls() {
        // Roo/Cline decide "call a tool" off finish_reason and
        // tool_calls alone; content must be the leftover text or null,
        // never the raw fenced block.
        let answer = "```aifuel_tool_calls\n{\"calls\":[{\"name\":\"lookup\",\"arguments\":{\"q\":\"rust\"}}]}\n```";
        let parsed = parse_tool_calls(answer).expect("a well-formed block parses");
        let body = completion_body("chatcmpl-1", 10, "auto", answer, Some(&parsed), None);
        let choice = &body["choices"][0];
        assert_eq!(choice["finish_reason"], "tool_calls");
        assert_eq!(choice["message"]["content"], Value::Null);
        let call = &choice["message"]["tool_calls"][0];
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "lookup");
        // arguments is a JSON string on the wire, not an object.
        assert_eq!(call["function"]["arguments"], r#"{"q":"rust"}"#);
        assert!(
            call["id"].as_str().expect("an id").starts_with("call_"),
            "call ids keep the call_ prefix clients pattern-match"
        );
    }

    #[test]
    fn streamed_calls_carry_index() {
        // Delta tool_calls are index-keyed so clients can accumulate
        // parallel calls; the message form must not carry the key.
        let parsed = ParsedAnswer {
            content: String::new(),
            calls: vec![
                ParsedCall {
                    name: "a".to_owned(),
                    arguments: "{}".to_owned(),
                },
                ParsedCall {
                    name: "b".to_owned(),
                    arguments: "{}".to_owned(),
                },
            ],
        };
        let delta = parsed.calls_json(true);
        assert_eq!(delta[0]["index"], 0);
        assert_eq!(delta[1]["index"], 1);
        assert_eq!(delta[1]["id"], "call_1");
        let message = parsed.calls_json(false);
        assert!(message[0].get("index").is_none());
    }
}
