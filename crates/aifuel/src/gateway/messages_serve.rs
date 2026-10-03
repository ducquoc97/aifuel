//! The serve half of `messages`: feed the resolved attempt chain's
//! deltas through `execute::run` and answer in Anthropic Messages shape -
//! `event:`-named SSE frames for `stream: true`, one `message` JSON
//! otherwise. `messages` owns the request mapping and the error
//! envelope; see `chat::serve` for the slot/writer/commit pattern this
//! shares with every `/v1` surface.

use super::{error_body, estimate_tokens, fail, record};
use crate::gateway::execute::{self, Feed, Flow, Outcome};
use crate::gateway::{Gateway, cors_headers, respond};
use aifuel_core::TokenUsage;
use serde_json::{Value, json};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

const SSE_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nAccess-Control-Allow-Origin: *\r\n\r\n";
/// Keepalive and terminator frames shared by the stream paths.
const PING_FRAME: &[u8] = b"event: ping\ndata: {\"type\":\"ping\"}\n\n";
const MESSAGE_STOP: &[u8] = b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

/// Feed the chain's deltas into the Anthropic event sequence
/// (`message_start`, `content_block_*`, `message_delta`, `message_stop`),
/// or buffer them for the one-shot `message` JSON when `stream` is false.
pub(super) fn serve(
    request: tiny_http::Request,
    gateway: &Gateway,
    model_echo: &str,
    prompt: &str,
    stream: bool,
    attempts: Vec<execute::Attempt>,
) {
    let message_id = format!(
        "msg_{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let mut slot = Some(request);
    let mut writer: Option<Box<dyn Write + Send>> = None;
    let outcome = execute::run(gateway, attempts, prompt, &mut |feed| match feed {
        Feed::Delta(delta) => {
            if !stream {
                return Flow::Continue;
            }
            if writer.is_none()
                && commit(&mut slot, &mut writer, &message_id, model_echo, prompt).is_err()
            {
                return Flow::Stop;
            }
            if write_all(
                writer.as_mut().expect("commit stores the writer"),
                delta_frame(delta).as_bytes(),
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
                && write_all(writer, PING_FRAME).is_err()
            {
                return Flow::Stop;
            }
            Flow::Continue
        }
    });

    match outcome {
        Outcome::Success { text, result } => {
            let usage = usage_json(result.usage.as_ref(), prompt, &text);
            let integration = Some(result.integration_id.as_str().to_owned());
            if let Some(writer) = writer.as_mut() {
                // Deltas already streamed: close the block and the message.
                let _ = write_terminal(writer, &usage);
            } else if stream {
                // The answer arrived whole (a non-streaming adapter):
                // commit now and replay it as one event sequence.
                if commit(&mut slot, &mut writer, &message_id, model_echo, prompt).is_err() {
                    record(
                        model_echo,
                        true,
                        499,
                        integration,
                        None,
                        Some("the stream could not be committed".to_owned()),
                    );
                    return;
                }
                let writer = writer.as_mut().expect("commit stores the writer");
                if !text.is_empty() {
                    let _ = write_all(writer, delta_frame(&text).as_bytes());
                }
                let _ = write_terminal(writer, &usage);
            } else {
                let request = slot.take().expect("an uncommitted request is present");
                respond(
                    request,
                    200,
                    serde_json::to_vec(&message_body(&message_id, model_echo, &text, &usage))
                        .expect("a message body serializes"),
                    Some("application/json"),
                    cors_headers(),
                );
            }
            record(model_echo, stream, 200, integration, Some(usage), None);
        }
        Outcome::Failed {
            status, message, ..
        } => {
            if let Some(writer) = writer.as_mut() {
                // The stream committed: report the failure as an `error`
                // event and close it out with `message_stop`.
                let _ = write_all(
                    writer,
                    sse_event("error", error_body(status, &message)).as_bytes(),
                )
                .and_then(|()| write_all(writer, MESSAGE_STOP));
                record(model_echo, stream, status, None, None, Some(message));
            } else if let Some(request) = slot.take() {
                // Nothing reached the wire (a buffered non-stream answer
                // only counts as committed to the chain): still answerable
                // with a normal error response, logged by `fail`.
                fail(request, model_echo, stream, status, &message);
            } else {
                record(model_echo, stream, status, None, None, Some(message));
            }
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

/// Write the SSE head plus `message_start` and `content_block_start` -
/// the point of no return after which the attempt owns the stream.
fn commit(
    slot: &mut Option<tiny_http::Request>,
    writer: &mut Option<Box<dyn Write + Send>>,
    message_id: &str,
    model_echo: &str,
    prompt: &str,
) -> Result<(), ()> {
    let request = slot.take().ok_or(())?;
    let mut w = request.into_writer();
    write_all(&mut w, SSE_HEAD.as_bytes())?;
    write_all(
        &mut w,
        sse_event(
            "message_start",
            message_start(message_id, model_echo, prompt),
        )
        .as_bytes(),
    )?;
    write_all(
        &mut w,
        sse_event(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""},
            }),
        )
        .as_bytes(),
    )?;
    *writer = Some(w);
    Ok(())
}

/// The `message_start` skeleton: empty content, `stop_reason` pending,
/// and the input estimate as usage - provider accounting only exists
/// once the run ends; `message_delta` reports the final output count.
fn message_start(message_id: &str, model_echo: &str, prompt: &str) -> Value {
    json!({
        "type": "message_start",
        "message": {
            "id": message_id,
            "type": "message",
            "role": "assistant",
            "content": [],
            "model": model_echo,
            "stop_reason": null,
            "stop_sequence": null,
            "usage": {"input_tokens": estimate_tokens(prompt), "output_tokens": 0},
        },
    })
}

/// Close the content block, send `message_delta` with the end-turn stop
/// reason and the final output count, then `message_stop`.
fn write_terminal(writer: &mut (impl Write + ?Sized), usage: &Value) -> Result<(), ()> {
    write_all(
        writer,
        sse_event(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        )
        .as_bytes(),
    )?;
    write_all(
        writer,
        sse_event(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": {"output_tokens": usage["output_tokens"]},
            }),
        )
        .as_bytes(),
    )?;
    write_all(writer, MESSAGE_STOP)
}

/// One `content_block_delta` carrying a `text_delta`.
fn delta_frame(text: &str) -> String {
    sse_event(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": text},
        }),
    )
}

/// One `event:`/`data:` pair; Anthropic names each event after its
/// payload `type`.
fn sse_event(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn write_all(writer: &mut (impl Write + ?Sized), bytes: &[u8]) -> Result<(), ()> {
    writer
        .write_all(bytes)
        .and_then(|()| writer.flush())
        .map_err(|_| ())
}

/// The non-streamed `message` body.
fn message_body(message_id: &str, model_echo: &str, answer: &str, usage: &Value) -> Value {
    json!({
        "id": message_id,
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": answer}],
        "model": model_echo,
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": usage,
    })
}

/// `{"input_tokens": N, "output_tokens": N}` for one finished run. The
/// Anthropic usage shape requires ints and admits no null, so a count
/// the provider never reported falls back to the documented estimate
/// rather than a zero reading as "free".
fn usage_json(reported: Option<&TokenUsage>, prompt: &str, answer: &str) -> Value {
    json!({
        "input_tokens": reported
            .and_then(|usage| usage.input_tokens)
            .unwrap_or_else(|| estimate_tokens(prompt)),
        "output_tokens": reported
            .and_then(|usage| usage.output_tokens)
            .unwrap_or_else(|| estimate_tokens(answer)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse one `event:`/`data:` frame back into (name, payload) - SDKs
    /// route on the event name, so tests assert on both halves.
    fn parse(frame: &str) -> (String, Value) {
        let mut lines = frame.trim_end().lines();
        let event = lines.next().expect("an event line").to_owned();
        let data: Value = serde_json::from_str(
            lines
                .next()
                .expect("a data line")
                .trim_start_matches("data: "),
        )
        .expect("the payload is json");
        (event.trim_start_matches("event: ").to_owned(), data)
    }

    /// The event sequence the Anthropic SDK consumes: named `event:`
    /// matching the payload `type`, the skeleton's empty content, text
    /// riding `delta.text`, stop reason on `message_delta`, and a
    /// `message_stop` terminator - clients key the whole turn off these
    /// exact names.
    #[test]
    fn stream_frames_are_anthropic_shaped() {
        let (event, data) = parse(&sse_event(
            "message_start",
            message_start("msg_1", "claude-stub-4", "hey"),
        ));
        assert_eq!(event, "message_start");
        assert_eq!(data["type"], "message_start");
        assert_eq!(data["message"]["id"], "msg_1");
        assert_eq!(data["message"]["role"], "assistant");
        assert_eq!(data["message"]["content"], json!([]));
        // "hey" is 3 chars -> ceil(3/4) = 1; output starts at zero.
        assert_eq!(
            data["message"]["usage"],
            json!({"input_tokens": 1, "output_tokens": 0})
        );

        let (event, data) = parse(&delta_frame("Hello"));
        assert_eq!(event, "content_block_delta");
        assert_eq!(data["index"], 0);
        assert_eq!(
            data["delta"],
            json!({"type": "text_delta", "text": "Hello"})
        );

        let mut buf = Vec::new();
        write_terminal(&mut buf, &json!({"input_tokens": 1, "output_tokens": 15})).unwrap();
        let text = String::from_utf8(buf).unwrap();
        let frames: Vec<&str> = text
            .split("\n\n")
            .filter(|frame| !frame.is_empty())
            .collect();
        assert_eq!(frames.len(), 3);
        let (event, data) = parse(frames[0]);
        assert_eq!(
            (event.as_str(), data["type"].as_str().unwrap()),
            ("content_block_stop", "content_block_stop")
        );
        let (event, data) = parse(frames[1]);
        assert_eq!(event, "message_delta");
        assert_eq!(data["delta"]["stop_reason"], "end_turn");
        assert_eq!(data["usage"]["output_tokens"], 15);
        let (event, data) = parse(frames[2]);
        assert_eq!(
            (event.as_str(), data["type"].as_str().unwrap()),
            ("message_stop", "message_stop")
        );
    }

    /// The one-shot body must carry the assistant text block, end-turn
    /// stop reason, and int usage - clients key content and accounting
    /// off this exact shape.
    #[test]
    fn message_body_is_anthropic_shaped() {
        let body = message_body(
            "msg_1",
            "claude-stub-4",
            "done",
            &json!({"input_tokens": 3, "output_tokens": 1}),
        );
        assert_eq!(body["type"], "message");
        assert_eq!(body["role"], "assistant");
        assert_eq!(body["content"], json!([{"type": "text", "text": "done"}]));
        assert_eq!(body["model"], "claude-stub-4");
        assert_eq!(body["stop_reason"], "end_turn");
        assert_eq!(body["stop_sequence"], Value::Null);
        assert_eq!(body["usage"]["input_tokens"], 3);
    }

    /// The Anthropic usage shape admits no null, so reported counts pass
    /// through per field while unreported ones estimate - a zero or a
    /// dropped field would misread as free or missing work.
    #[test]
    fn usage_prefers_provider_counts_and_estimates_the_rest() {
        let usage = usage_json(
            Some(&TokenUsage {
                input_tokens: Some(25),
                output_tokens: None,
            }),
            "12345678", // 8 chars -> 2
            "1234",     // 4 chars -> 1
        );
        assert_eq!(usage, json!({"input_tokens": 25, "output_tokens": 1}));

        let usage = usage_json(None, "12345678", "123456789");
        assert_eq!(usage, json!({"input_tokens": 2, "output_tokens": 3}));
    }
}
