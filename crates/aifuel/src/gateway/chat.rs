//! `POST /v1/chat/completions`: run the resolved attempt chain through
//! `execute` and answer in OpenAI Chat Completions shape - SSE
//! `chat.completion.chunk` frames for `stream: true`, one
//! `chat.completion` JSON otherwise.
//!
//! This module owns only the wire format; the attempt chain, failover,
//! and cancellation machinery lives in `execute`.

use super::execute::{self, Feed, Flow, Outcome};
use super::flatten::flatten_messages;
use super::types::ChatRequest;
use super::{Gateway, cors_headers, read_body, respond, respond_error};
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
    if chat.messages.is_empty() {
        respond_error(
            request,
            400,
            "messages must not be empty",
            "invalid_request_error",
        );
        return;
    }
    let prompt = flatten_messages(&chat.messages);
    if prompt.trim().is_empty() {
        respond_error(
            request,
            400,
            "messages contain no usable text; media-only content is not supported",
            "invalid_request_error",
        );
        return;
    }
    let attempts =
        match execute::resolve_attempts(gateway, model, &|| Gateway::status(facade, runtime)) {
            Ok(attempts) => attempts,
            Err((status, message)) => {
                respond_error(request, status, &message, "invalid_request_error");
                return;
            }
        };
    serve(request, gateway, &chat, model, &prompt, attempts);
}

/// Feed the chain's deltas into `chat.completion.chunk` frames, or buffer
/// them for the one-shot JSON when `stream` is false.
fn serve(
    request: tiny_http::Request,
    gateway: &Gateway,
    chat: &ChatRequest,
    model_echo: &str,
    prompt: &str,
    attempts: Vec<execute::Attempt>,
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
            if !stream {
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
            if let Some(writer) = writer.as_mut() {
                // Deltas already streamed: close out the committed response.
                let _ = write_terminal(
                    writer,
                    result.usage.as_ref(),
                    include_usage,
                    &completion_id,
                    created,
                    model_echo,
                )
                .and_then(|()| write_all(writer, b"data: [DONE]\n\n"));
            } else if stream {
                // The answer arrived whole (a non-streaming adapter): commit
                // now and replay it as one chunk sequence.
                if commit(&mut slot, &mut writer, &completion_id, created, model_echo).is_err() {
                    return;
                }
                let writer = writer.as_mut().expect("commit stores the writer");
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
                let _ = write_terminal(
                    writer,
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
            message,
            ..
        } => {
            if let Some(writer) = writer.as_mut() {
                let _ = write_error_frame(writer, &message);
            }
        }
        Outcome::Failed {
            status, message, ..
        } => {
            if let Some(request) = slot.take() {
                respond_error(request, status, &message, "server_error");
            }
        }
        Outcome::Aborted => {}
    }
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

/// The terminal chunk (`finish_reason: "stop"`) plus the spec-shaped usage
/// chunk when the caller asked for it.
fn write_terminal(
    writer: &mut Box<dyn Write + Send>,
    usage: Option<&TokenUsage>,
    include_usage: bool,
    completion_id: &str,
    created: u64,
    model_echo: &str,
) -> Result<(), ()> {
    let stop = chunk_frame(completion_id, created, model_echo, json!({}), Some("stop"));
    write_all(writer, stop.as_bytes())?;
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

/// The non-streamed `chat.completion` body.
fn completion_body(
    completion_id: &str,
    created: u64,
    model_echo: &str,
    answer: &str,
    usage: Option<&TokenUsage>,
) -> Value {
    json!({
        "id": completion_id,
        "object": "chat.completion",
        "created": created,
        "model": model_echo,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": answer},
            "finish_reason": "stop",
        }],
        "usage": usage.map(|usage| json!({
            "prompt_tokens": usage.input_tokens,
            "completion_tokens": usage.output_tokens,
            "total_tokens": usage.input_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0),
        })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let body = completion_body("chatcmpl-1", 10, "auto", "done", None);
        assert_eq!(body["object"], "chat.completion");
        assert_eq!(body["choices"][0]["message"]["content"], "done");
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
        assert_eq!(body["usage"], Value::Null);
    }
}
