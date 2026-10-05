//! Legacy OpenAI text `POST /v1/completions`: the pre-chat API shape -
//! one `prompt`, one `text` answer. The prompt goes to the run verbatim:
//! legacy completions carry no system/role message structure, so there is
//! no transcript to flatten the way `chat` does.
//!
//! This module owns only the wire format; the attempt chain, failover,
//! and cancellation machinery lives in `execute` - see `chat` for the
//! slot/writer/commit pattern both endpoints share.

use super::execute::{self, Feed, Flow, Outcome};
use super::{Gateway, cors_headers, logs, read_body, respond, respond_error};
use aifuel_app::MonitoringFacade;
use aifuel_core::{StatusCollector, TokenUsage};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

const SSE_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nAccess-Control-Allow-Origin: *\r\n\r\n";

/// `POST /v1/completions` request subset the gateway consumes. Per the
/// `types` convention every other field - `suffix`, `max_tokens`,
/// `temperature`, `top_p`, `stop`, `echo`, penalties - is tolerated and
/// ignored: the run contract carries a prompt, not sampling knobs.
#[derive(Debug, Deserialize)]
struct CompletionRequest {
    /// The routing selector; see the module docs on `crate::gateway` for
    /// the addressing convention. Required by the OpenAI contract.
    model: Option<String>,
    /// Required by the legacy contract: one string, or the array form
    /// collapsed into one prompt.
    prompt: Option<Prompt>,
    #[serde(default)]
    stream: bool,
    /// Requested model-specific effort - the `reasoning_effort` spelling
    /// the other `/v1` surfaces share.
    #[serde(default)]
    reasoning_effort: Option<String>,
}

/// The two prompt shapes the legacy API accepts here: a bare string, or
/// an array of parts.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Prompt {
    Text(String),
    Parts(Vec<String>),
}

impl Prompt {
    /// Collapse to the single transcript the run contract carries: array
    /// parts join on a blank line, the legacy batch separator.
    fn into_text(self) -> String {
        match self {
            Self::Text(text) => text,
            Self::Parts(parts) => parts.join("\n\n"),
        }
    }
}

/// Handle one `/v1/completions` request end to end.
pub(crate) fn handle<C: StatusCollector>(
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
    let completion: CompletionRequest = match serde_json::from_slice(&body) {
        Ok(completion) => completion,
        Err(error) => {
            respond_error(
                request,
                400,
                &format!("invalid completion request: {error}"),
                "invalid_request_error",
            );
            return;
        }
    };
    let model = completion.model.as_deref().unwrap_or_default().trim();
    if model.is_empty() {
        respond_error(request, 400, "model is required", "invalid_request_error");
        return;
    }
    if let Err(reason) = super::keys::require_permits(&request, model) {
        respond_error(request, 403, &reason, "permission_error");
        return;
    }
    let Some(prompt) = completion.prompt.map(Prompt::into_text) else {
        respond_error(request, 400, "prompt is required", "invalid_request_error");
        return;
    };
    if prompt.trim().is_empty() {
        respond_error(
            request,
            400,
            "prompt must not be empty",
            "invalid_request_error",
        );
        return;
    }
    let attempts = match execute::resolve_attempts(
        gateway,
        model,
        completion.reasoning_effort.as_deref(),
        &|| Gateway::status(facade, runtime),
    ) {
        Ok(attempts) => attempts,
        Err((status, message)) => {
            respond_error(request, status, &message, "invalid_request_error");
            return;
        }
    };
    serve(
        request,
        gateway,
        model,
        &prompt,
        completion.stream,
        attempts,
    );
}

/// Feed the chain's deltas into `text_completion` frames, or buffer them
/// for the one-shot JSON when `stream` is false.
fn serve(
    request: tiny_http::Request,
    gateway: &Gateway,
    model_echo: &str,
    prompt: &str,
    stream: bool,
    attempts: Vec<execute::Attempt>,
) {
    let completion_id = format!(
        "cmpl-{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut slot = Some(request);
    let mut writer: Option<Box<dyn Write + Send>> = None;
    let outcome = execute::run(gateway, attempts, prompt, &mut |feed| match feed {
        Feed::Delta(delta) => {
            if !stream {
                return Flow::Continue;
            }
            if writer.is_none() && commit(&mut slot, &mut writer).is_err() {
                return Flow::Stop;
            }
            let frame = chunk_frame(&completion_id, created, model_echo, delta, None);
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
                let _ = write_all(
                    writer,
                    chunk_frame(&completion_id, created, model_echo, "", Some("stop")).as_bytes(),
                )
                .and_then(|()| write_all(writer, b"data: [DONE]\n\n"));
            } else if stream {
                // The answer arrived whole (a non-streaming adapter): commit
                // now and replay it as one chunk sequence. A failed commit
                // means the client is already gone - the record below still
                // logs the successful run.
                if commit(&mut slot, &mut writer).is_ok() {
                    let writer = writer.as_mut().expect("commit stores the writer");
                    if !text.is_empty() {
                        let _ = write_all(
                            writer,
                            chunk_frame(&completion_id, created, model_echo, &text, None)
                                .as_bytes(),
                        );
                    }
                    let _ = write_all(
                        writer,
                        chunk_frame(&completion_id, created, model_echo, "", Some("stop"))
                            .as_bytes(),
                    )
                    .and_then(|()| write_all(writer, b"data: [DONE]\n\n"));
                }
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
            record(
                model_echo,
                stream,
                200,
                Some(result.integration_id.as_str().to_owned()),
                result.usage.as_ref(),
                None,
            );
        }
        Outcome::Failed {
            committed,
            status,
            message,
        } => {
            if committed {
                if let Some(writer) = writer.as_mut() {
                    let _ = write_error_frame(writer, &message);
                }
            } else if let Some(request) = slot.take() {
                respond_error(request, status, &message, "server_error");
            }
            record(model_echo, stream, status, None, None, Some(message));
        }
        Outcome::Aborted => {
            // The client is gone; 499 is the conventional "client closed
            // request" marker in request logs.
            record(
                model_echo,
                stream,
                499,
                None,
                None,
                Some("the client disconnected mid-run".to_owned()),
            );
        }
    }
}

/// Write the SSE head - the point of no return after which the attempt
/// owns the stream. Legacy completions have no role preamble the way
/// `chat` does; the first frame is already content.
fn commit(
    slot: &mut Option<tiny_http::Request>,
    writer: &mut Option<Box<dyn Write + Send>>,
) -> Result<(), ()> {
    let request = slot.take().ok_or(())?;
    let mut w = request.into_writer();
    write_all(&mut w, SSE_HEAD.as_bytes())?;
    *writer = Some(w);
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

/// One `text_completion` SSE frame.
fn chunk_frame(
    completion_id: &str,
    created: u64,
    model_echo: &str,
    text: &str,
    finish_reason: Option<&str>,
) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": completion_id,
            "object": "text_completion",
            "created": created,
            "model": model_echo,
            "choices": [{"text": text, "index": 0, "finish_reason": finish_reason}],
        })
    )
}

/// The non-streamed `text_completion` body.
fn completion_body(
    completion_id: &str,
    created: u64,
    model_echo: &str,
    answer: &str,
    usage: Option<&TokenUsage>,
) -> Value {
    json!({
        "id": completion_id,
        "object": "text_completion",
        "created": created,
        "model": model_echo,
        "choices": [{
            "text": answer,
            "index": 0,
            "finish_reason": "stop",
            "logprobs": null,
        }],
        "usage": usage.map(usage_json),
    })
}

/// The OpenAI token block shared by the response body and the request
/// log. Missing counts stay unknown, never zero - `total_tokens` only
/// sums what the provider reported.
fn usage_json(usage: &TokenUsage) -> Value {
    json!({
        "prompt_tokens": usage.input_tokens,
        "completion_tokens": usage.output_tokens,
        "total_tokens": usage.input_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0),
    })
}

/// Record the terminal outcome in the request log.
fn record(
    model_echo: &str,
    stream: bool,
    status: u16,
    integration: Option<String>,
    usage: Option<&TokenUsage>,
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
        usage: usage.map(usage_json),
        error,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The request must decode what legacy clients actually send: a bare
    /// prompt string, the array form joined on the batch separator, and a
    /// broad parameter set the gateway ignores - strict decoding would
    /// 400 requests a provider could have served.
    #[test]
    fn request_body_decodes_both_prompt_shapes_and_tolerates_the_rest() {
        let one: CompletionRequest =
            serde_json::from_str(r#"{"model": "auto", "prompt": "hi"}"#).expect("a string prompt");
        assert_eq!(one.prompt.map(Prompt::into_text).as_deref(), Some("hi"));

        let many: CompletionRequest =
            serde_json::from_str(r#"{"model": "auto", "prompt": ["a", "b"], "stream": true}"#)
                .expect("an array prompt");
        assert_eq!(
            many.prompt.map(Prompt::into_text).as_deref(),
            Some("a\n\nb")
        );
        assert!(many.stream);

        let broad: CompletionRequest = serde_json::from_str(
            r#"{"model": "auto", "prompt": "x", "suffix": "s", "max_tokens": 5,
                "temperature": 0.7, "top_p": 0.9, "stop": ["\n"], "echo": true,
                "n": 2, "best_of": 3, "extra_body": {"k": 1}}"#,
        )
        .expect("unknown fields are tolerated");
        assert!(broad.prompt.is_some());
    }

    /// A missing, null, or mistyped prompt must fail decode or the
    /// required-field check - the run contract needs a transcript.
    #[test]
    fn a_prompt_must_be_present_and_textual() {
        let missing: CompletionRequest =
            serde_json::from_str(r#"{"model": "auto"}"#).expect("prompt is optional in decode");
        assert!(missing.prompt.is_none());
        let null: CompletionRequest =
            serde_json::from_str(r#"{"model": "auto", "prompt": null}"#).expect("null decodes");
        assert!(null.prompt.is_none());
        // Token arrays and other types are not served here.
        assert!(serde_json::from_str::<CompletionRequest>(r#"{"prompt": [1, 2]}"#).is_err());
        assert!(serde_json::from_str::<CompletionRequest>(r#"{"prompt": 5}"#).is_err());
    }

    /// The SSE frames must match the shape legacy completions clients
    /// parse: `data:` prefix, `text_completion` object, `choices[].text`,
    /// and a null `finish_reason` until the terminal stop chunk.
    #[test]
    fn chunk_frames_are_legacy_completion_shaped() {
        let delta = chunk_frame("cmpl-1", 10, "auto", "hi", None);
        let frame: Value = serde_json::from_str(delta.trim_start_matches("data: ").trim())
            .expect("a chunk frame is json");
        assert_eq!(frame["object"], "text_completion");
        assert_eq!(frame["choices"][0]["text"], "hi");
        assert_eq!(frame["choices"][0]["finish_reason"], Value::Null);
        assert!(delta.ends_with("\n\n"));

        let stop = chunk_frame("cmpl-1", 10, "auto", "", Some("stop"));
        let frame: Value = serde_json::from_str(stop.trim_start_matches("data: ").trim())
            .expect("a stop frame is json");
        assert_eq!(frame["choices"][0]["finish_reason"], "stop");
    }

    /// The one-shot body must carry `logprobs` (null - the gateway never
    /// computes them) and real usage or null: clients key accounting off
    /// this exact shape, and a missing count must never report as zero.
    #[test]
    fn completion_body_is_legacy_shaped() {
        let body = completion_body(
            "cmpl-1",
            10,
            "auto",
            "done",
            Some(&TokenUsage {
                input_tokens: Some(3),
                output_tokens: Some(4),
            }),
        );
        assert_eq!(body["object"], "text_completion");
        assert_eq!(body["choices"][0]["text"], "done");
        assert_eq!(body["choices"][0]["index"], 0);
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
        assert_eq!(body["choices"][0]["logprobs"], Value::Null);
        assert_eq!(body["usage"]["prompt_tokens"], 3);
        assert_eq!(body["usage"]["total_tokens"], 7);

        let unmeasured = completion_body("cmpl-1", 10, "auto", "done", None);
        assert_eq!(unmeasured["usage"], Value::Null);
    }
}
