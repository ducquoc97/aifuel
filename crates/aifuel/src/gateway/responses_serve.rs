//! The wire half of `super::responses`: the shared `execute` chain drives
//! one attempt and this module shapes the answer into a Response JSON
//! object or the `response.*` SSE event stream. Codex CLI dispatches on
//! the `type` field inside each `data:` frame; every frame also carries an
//! `event:` line naming that type for generic SSE clients. There is no
//! `[DONE]` sentinel - the Responses stream ends at its terminal event
//! (`response.completed` or `response.failed`).
//!
//! Tool calling round-trips through the prompt-level emulation `chat`
//! established: a request that declared `type: "function"` tools buffered
//! its deltas, and a recovered `aifuel_tool_calls` block goes out as
//! `function_call` output items (one `output_item.added` /
//! `function_call_arguments.*` / `output_item.done` sequence per call),
//! so a partial block never reaches the client as text.

use super::unix_secs;
use crate::gateway::execute::{self, Feed, Flow, Outcome};
use crate::gateway::flatten::{ParsedAnswer, ParsedCall, parse_tool_calls};
use crate::gateway::{Gateway, cors_headers, logs, respond, respond_error};
use aifuel_core::TokenUsage;
use serde_json::{Value, json};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

const SSE_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nAccess-Control-Allow-Origin: *\r\n\r\n";

/// Feed the chain's deltas into `response.output_text.delta` events, or
/// buffer them for the one-shot Response object when `stream` is false.
/// `parse_tools` marks a request that declared tools: its answer may
/// carry the fenced `aifuel_tool_calls` block, so deltas are buffered
/// (never streamed mid-run) and the parsed result goes out as output
/// items once the answer is whole.
#[allow(clippy::too_many_arguments)]
pub(super) fn serve(
    request: tiny_http::Request,
    gateway: &Gateway,
    model: &str,
    prompt: &str,
    stream: bool,
    attempts: Vec<execute::Attempt>,
    parse_tools: bool,
) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let response_id = format!("resp_{nanos:x}");
    let item_id = format!("msg_{:x}", nanos as u64);
    let created = unix_secs();
    // `sequence_number` orders the events like OpenAI's own stream.
    let mut seq = 0u64;

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
                && commit(
                    &mut slot,
                    &mut writer,
                    &response_id,
                    &item_id,
                    created,
                    model,
                    &mut seq,
                )
                .is_err()
            {
                return Flow::Stop;
            }
            let frame = sse_frame(&delta_event(&response_id, &item_id, 0, delta, &mut seq));
            match write_all(
                writer.as_mut().expect("commit stores the writer"),
                frame.as_bytes(),
            ) {
                Ok(()) => Flow::Continue,
                Err(()) => Flow::Stop,
            }
        }
        Feed::Keepalive => {
            // Only an already-committed stream carries the keepalive
            // comment; before commit a tick is silence.
            if let Some(writer) = writer.as_mut()
                && write_all(writer, b": keep-alive\n\n").is_err()
            {
                return Flow::Stop;
            }
            Flow::Continue
        }
    });

    match outcome {
        Outcome::Success { text, result } => {
            let usage = usage_value(prompt, &text, result.usage.as_ref());
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
                // the output is the plain text answer.
                let _ = write_events(
                    writer,
                    &message_done_events(&response_id, &item_id, 0, &text, &mut seq),
                )
                .and_then(|()| {
                    write_events(
                        writer,
                        &[completed_event(
                            &response_id,
                            created,
                            model,
                            output_items(&item_id, &text, None),
                            &usage,
                            &mut seq,
                        )],
                    )
                });
            } else if stream {
                // The answer arrived whole (a non-streaming adapter, or a
                // tool request buffering for the call block): commit now
                // and replay it as the full event sequence.
                if commit_head(
                    &mut slot,
                    &mut writer,
                    &response_id,
                    created,
                    model,
                    &mut seq,
                )
                .is_err()
                {
                    log(
                        model,
                        None,
                        499,
                        stream,
                        None,
                        Some("the client went away before the stream committed".to_owned()),
                    );
                    return;
                }
                let writer = writer.as_mut().expect("commit_head stores the writer");
                let _ = write_answer(
                    writer,
                    &response_id,
                    &item_id,
                    created,
                    model,
                    &text,
                    parsed.as_ref(),
                    &usage,
                    &mut seq,
                );
            } else {
                let request = slot.take().expect("an uncommitted request is present");
                let body = response_object(
                    &response_id,
                    created,
                    model,
                    "completed",
                    json!(output_items(&item_id, &text, parsed.as_ref())),
                    Some(usage.clone()),
                    None,
                );
                respond(
                    request,
                    200,
                    serde_json::to_vec(&body).expect("a response body serializes"),
                    Some("application/json"),
                    cors_headers(),
                );
            }
            log(
                model,
                Some(result.integration_id.to_string()),
                200,
                stream,
                Some(usage),
                None,
            );
        }
        Outcome::Failed {
            status, message, ..
        } => {
            let mut http_status = status;
            if let Some(writer) = writer.as_mut() {
                // The stream committed: failure surfaces as
                // `response.failed`, the stream's terminal event - no
                // `[DONE]` follows it.
                let frame = sse_frame(&failed_event(
                    &response_id,
                    created,
                    model,
                    &message,
                    &mut seq,
                ));
                let _ = write_all(writer, frame.as_bytes());
                http_status = 200;
            } else if let Some(request) = slot.take() {
                // Either the run never committed, or it buffered deltas
                // for tool parsing and the request is still answerable
                // with a normal error response.
                respond_error(request, status, &message, "server_error");
            }
            log(model, None, http_status, stream, None, Some(message));
        }
        Outcome::Aborted => {
            // The sink only votes `Stop` after the stream committed, so
            // the client saw a 200 it walked away from.
            log(
                model,
                None,
                499,
                stream,
                None,
                Some("the client went away; the run was cancelled".to_owned()),
            );
        }
    }
}

/// Write the SSE head, `response.created`, and the message item's opening
/// events - the point of no return after which the attempt owns the
/// stream. Used by the live-delta path; the buffered path commits with
/// [`commit_head`] and emits per-item sequences itself.
fn commit(
    slot: &mut Option<tiny_http::Request>,
    writer: &mut Option<Box<dyn Write + Send>>,
    response_id: &str,
    item_id: &str,
    created: u64,
    model: &str,
    seq: &mut u64,
) -> Result<(), ()> {
    commit_head(slot, writer, response_id, created, model, seq)?;
    let w = writer.as_mut().expect("commit_head stores the writer");
    for event in message_open_events(response_id, item_id, 0, seq) {
        write_all(w, sse_frame(&event).as_bytes())?;
    }
    Ok(())
}

/// Write the SSE head and `response.created` only - the buffered path's
/// commit, before its replay decides which output items exist.
fn commit_head(
    slot: &mut Option<tiny_http::Request>,
    writer: &mut Option<Box<dyn Write + Send>>,
    response_id: &str,
    created: u64,
    model: &str,
    seq: &mut u64,
) -> Result<(), ()> {
    let request = slot.take().ok_or(())?;
    let mut w = request.into_writer();
    write_all(&mut w, SSE_HEAD.as_bytes())?;
    write_all(
        &mut w,
        sse_frame(&created_event(response_id, created, model, seq)).as_bytes(),
    )?;
    *writer = Some(w);
    Ok(())
}

/// Replay a buffered answer: plain text as one message item sequence, or
/// a parsed tool-call answer as its message part plus one `function_call`
/// item sequence per call, then the terminal `response.completed`.
#[allow(clippy::too_many_arguments)]
fn write_answer(
    writer: &mut Box<dyn Write + Send>,
    response_id: &str,
    item_id: &str,
    created: u64,
    model: &str,
    text: &str,
    parsed: Option<&ParsedAnswer>,
    usage: &Value,
    seq: &mut u64,
) -> Result<(), ()> {
    let (content, calls): (&str, &[ParsedCall]) = match parsed {
        Some(answer) => (answer.content.as_str(), &answer.calls),
        None => (text, &[]),
    };
    let mut output_index = 0usize;
    // A message item is present whenever the answer is plain text or the
    // parsed answer left real text outside its call block.
    if parsed.is_none() || !content.is_empty() {
        write_message_sequence(writer, response_id, item_id, output_index, content, seq)?;
        output_index += 1;
    }
    for (ordinal, call) in calls.iter().enumerate() {
        write_events(
            writer,
            &call_events(response_id, ordinal, output_index, call, seq),
        )?;
        output_index += 1;
    }
    write_events(
        writer,
        &[completed_event(
            response_id,
            created,
            model,
            output_items(item_id, text, parsed),
            usage,
            seq,
        )],
    )
}

/// One message item's full sequence: `output_item.added`,
/// `content_part.added`, the whole answer as one `output_text.delta`,
/// then the `done` trio.
fn write_message_sequence(
    writer: &mut Box<dyn Write + Send>,
    response_id: &str,
    item_id: &str,
    output_index: usize,
    text: &str,
    seq: &mut u64,
) -> Result<(), ()> {
    let mut events = message_open_events(response_id, item_id, output_index, seq);
    if !text.is_empty() {
        events.push(delta_event(response_id, item_id, output_index, text, seq));
    }
    events.extend(message_done_events(
        response_id,
        item_id,
        output_index,
        text,
        seq,
    ));
    write_events(writer, &events)
}

fn write_events(writer: &mut (impl Write + ?Sized), events: &[Value]) -> Result<(), ()> {
    for event in events {
        write_all(writer, sse_frame(event).as_bytes())?;
    }
    Ok(())
}

/// One `/v1` request-log line; every terminal outcome records once.
fn log(
    model: &str,
    integration: Option<String>,
    status: u16,
    stream: bool,
    usage: Option<Value>,
    error: Option<String>,
) {
    logs::record(logs::Entry {
        ts_unix: unix_secs(),
        model: model.to_owned(),
        integration,
        status,
        stream,
        usage,
        error,
    });
}

fn write_all(writer: &mut (impl Write + ?Sized), bytes: &[u8]) -> Result<(), ()> {
    writer
        .write_all(bytes)
        .and_then(|()| writer.flush())
        .map_err(|_| ())
}

/// One SSE frame: `event:` names `data.type`, matching the Responses API
/// convention (clients differ on which of the two they dispatch on).
fn sse_frame(data: &Value) -> String {
    let event = data
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message");
    format!("event: {event}\ndata: {data}\n\n")
}

/// The `response.created` event: the full Response object `in_progress`,
/// before any output exists.
fn created_event(response_id: &str, created: u64, model: &str, seq: &mut u64) -> Value {
    json!({
        "type": "response.created",
        "sequence_number": take_seq(seq),
        "response": response_object(
            response_id, created, model, "in_progress", json!([]), None, None,
        ),
    })
}

/// A message item's opening events: `response.output_item.added` with the
/// `in_progress` item, then `response.content_part.added` with the empty
/// `output_text` part deltas will fill.
fn message_open_events(
    response_id: &str,
    item_id: &str,
    output_index: usize,
    seq: &mut u64,
) -> Vec<Value> {
    vec![
        json!({
            "type": "response.output_item.added",
            "sequence_number": take_seq(seq),
            "response_id": response_id,
            "output_index": output_index,
            "item": message_item(item_id, "in_progress", ""),
        }),
        json!({
            "type": "response.content_part.added",
            "sequence_number": take_seq(seq),
            "response_id": response_id,
            "item_id": item_id,
            "output_index": output_index,
            "content_index": 0,
            "part": {"type": "output_text", "text": "", "annotations": []},
        }),
    ]
}

/// One `response.output_text.delta` event carrying an answer delta.
fn delta_event(
    response_id: &str,
    item_id: &str,
    output_index: usize,
    delta: &str,
    seq: &mut u64,
) -> Value {
    json!({
        "type": "response.output_text.delta",
        "sequence_number": take_seq(seq),
        "response_id": response_id,
        "item_id": item_id,
        "output_index": output_index,
        "content_index": 0,
        "delta": delta,
    })
}

/// A message item's closing events: `response.output_text.done`,
/// `response.content_part.done`, `response.output_item.done`.
fn message_done_events(
    response_id: &str,
    item_id: &str,
    output_index: usize,
    text: &str,
    seq: &mut u64,
) -> Vec<Value> {
    vec![
        json!({
            "type": "response.output_text.done",
            "sequence_number": take_seq(seq),
            "response_id": response_id,
            "item_id": item_id,
            "output_index": output_index,
            "content_index": 0,
            "text": text,
        }),
        json!({
            "type": "response.content_part.done",
            "sequence_number": take_seq(seq),
            "response_id": response_id,
            "item_id": item_id,
            "output_index": output_index,
            "content_index": 0,
            "part": {"type": "output_text", "text": text, "annotations": []},
        }),
        json!({
            "type": "response.output_item.done",
            "sequence_number": take_seq(seq),
            "response_id": response_id,
            "output_index": output_index,
            "item": message_item(item_id, "completed", text),
        }),
    ]
}

/// One `function_call` item's sequence: `output_item.added` (`in_progress`,
/// empty arguments), `function_call_arguments.delta` with the whole
/// argument string, `function_call_arguments.done`, `output_item.done`.
fn call_events(
    response_id: &str,
    ordinal: usize,
    output_index: usize,
    call: &ParsedCall,
    seq: &mut u64,
) -> Vec<Value> {
    let call_item_id = format!("fc_{ordinal}");
    vec![
        json!({
            "type": "response.output_item.added",
            "sequence_number": take_seq(seq),
            "response_id": response_id,
            "output_index": output_index,
            "item": call_item(ordinal, call, "in_progress", ""),
        }),
        json!({
            "type": "response.function_call_arguments.delta",
            "sequence_number": take_seq(seq),
            "response_id": response_id,
            "item_id": call_item_id,
            "output_index": output_index,
            "delta": call.arguments,
        }),
        json!({
            "type": "response.function_call_arguments.done",
            "sequence_number": take_seq(seq),
            "response_id": response_id,
            "item_id": call_item_id,
            "output_index": output_index,
            "arguments": call.arguments,
        }),
        json!({
            "type": "response.output_item.done",
            "sequence_number": take_seq(seq),
            "response_id": response_id,
            "output_index": output_index,
            "item": call_item(ordinal, call, "completed", &call.arguments),
        }),
    ]
}

/// The terminal `response.completed` event: the final object with its
/// output items and usage.
fn completed_event(
    response_id: &str,
    created: u64,
    model: &str,
    output: Vec<Value>,
    usage: &Value,
    seq: &mut u64,
) -> Value {
    json!({
        "type": "response.completed",
        "sequence_number": take_seq(seq),
        "response": response_object(
            response_id, created, model, "completed", json!(output),
            Some(usage.clone()), None,
        ),
    })
}

/// The terminal `response.failed` event: the object goes `failed` with
/// its `error` populated - Codex reads `response.error.{code,message}`.
fn failed_event(
    response_id: &str,
    created: u64,
    model: &str,
    message: &str,
    seq: &mut u64,
) -> Value {
    json!({
        "type": "response.failed",
        "sequence_number": take_seq(seq),
        "response": response_object(
            response_id, created, model, "failed", json!([]), None,
            Some(json!({"code": "server_error", "message": message})),
        ),
    })
}

/// The `output` array of a completed response: the message item for a
/// plain answer or the parsed answer's leftover text, plus one
/// `function_call` item per recovered call.
fn output_items(item_id: &str, text: &str, parsed: Option<&ParsedAnswer>) -> Vec<Value> {
    match parsed {
        Some(answer) => {
            let mut items = Vec::new();
            if !answer.content.is_empty() {
                items.push(message_item(item_id, "completed", &answer.content));
            }
            items.extend(
                answer
                    .calls
                    .iter()
                    .enumerate()
                    .map(|(ordinal, call)| call_item(ordinal, call, "completed", &call.arguments)),
            );
            items
        }
        None => vec![message_item(item_id, "completed", text)],
    }
}

/// The Response object shape Codex and the OpenAI SDK parse. `usage` is
/// `null` until `status` is `completed`; `error` only on `failed`.
fn response_object(
    id: &str,
    created: u64,
    model: &str,
    status: &str,
    output: Value,
    usage: Option<Value>,
    error: Option<Value>,
) -> Value {
    json!({
        "id": id,
        "object": "response",
        "created_at": created,
        "status": status,
        "model": model,
        "output": output,
        "usage": usage,
        "error": error,
        "incomplete_details": null,
        "parallel_tool_calls": true,
        "tools": [],
        "metadata": {},
    })
}

/// The single `output` item a plain answer carries: one assistant message
/// with one `output_text` part. An `in_progress` item reports empty
/// `content`, matching OpenAI's streamed shape.
fn message_item(item_id: &str, status: &str, text: &str) -> Value {
    let content = if status == "in_progress" {
        json!([])
    } else {
        json!([{"type": "output_text", "text": text, "annotations": []}])
    };
    json!({
        "id": item_id,
        "type": "message",
        "status": status,
        "role": "assistant",
        "content": content,
    })
}

/// One `function_call` output item. `call_id`/`id` derive from the call's
/// ordinal so the client can echo them back in `function_call_output`
/// next turn; an `in_progress` item carries empty `arguments`.
fn call_item(ordinal: usize, call: &ParsedCall, status: &str, arguments: &str) -> Value {
    json!({
        "id": format!("fc_{ordinal}"),
        "type": "function_call",
        "status": status,
        "call_id": format!("call_{ordinal}"),
        "name": call.name,
        "arguments": arguments,
    })
}

/// The Responses `usage` block: provider counts where the run reported
/// them, else a `ceil(chars/4)` estimate - the execution contract does
/// not always carry token accounting, and clients still wait on a
/// completed usage shape.
fn usage_value(prompt: &str, answer: &str, reported: Option<&TokenUsage>) -> Value {
    let estimate = |text: &str| (text.chars().count() as u64).div_ceil(4);
    let input = reported
        .and_then(|usage| usage.input_tokens)
        .unwrap_or_else(|| estimate(prompt));
    let output = reported
        .and_then(|usage| usage.output_tokens)
        .unwrap_or_else(|| estimate(answer));
    json!({
        "input_tokens": input,
        "output_tokens": output,
        "total_tokens": input + output,
    })
}

fn take_seq(seq: &mut u64) -> u64 {
    let next = *seq;
    *seq += 1;
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Codex CLI dispatches on `data.type`, parses `response.completed`
    /// for usage, and collects output items from
    /// `response.output_item.done`; the OpenAI SDK waits on the same
    /// lifecycle order. A reorder or a missing event would strand clients
    /// on a terminal `response.completed` that never arrives the way they
    /// expect.
    #[test]
    fn stream_events_follow_the_responses_lifecycle() {
        let usage = json!({"input_tokens": 1, "output_tokens": 1, "total_tokens": 2});
        let mut seq = 0;
        let mut events = vec![created_event("resp_1", 10, "auto", &mut seq)];
        events.extend(message_open_events("resp_1", "msg_1", 0, &mut seq));
        events.push(delta_event("resp_1", "msg_1", 0, "hi", &mut seq));
        events.extend(message_done_events("resp_1", "msg_1", 0, "hi", &mut seq));
        events.push(completed_event(
            "resp_1",
            10,
            "auto",
            output_items("msg_1", "hi", None),
            &usage,
            &mut seq,
        ));
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "response.created",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        let numbers: Vec<u64> = events
            .iter()
            .map(|event| event["sequence_number"].as_u64().unwrap())
            .collect();
        assert!(numbers.windows(2).all(|pair| pair[0] < pair[1]));

        // Each frame's `event:` line names `data.type`, and the terminal
        // event carries the completed object with usage - not a `[DONE]`.
        let frame = sse_frame(&events[0]);
        assert!(frame.starts_with("event: response.created\ndata: "));
        assert!(frame.ends_with("\n\n"));
        let last = events.last().unwrap();
        assert_eq!(last["response"]["status"], "completed");
        assert_eq!(last["response"]["usage"]["total_tokens"], 2);
    }

    #[test]
    fn response_object_is_responses_shaped() {
        let object = response_object(
            "resp_1",
            10,
            "auto",
            "completed",
            json!([message_item("msg_1", "completed", "done")]),
            Some(json!({"input_tokens": 2, "output_tokens": 3, "total_tokens": 5})),
            None,
        );
        assert_eq!(object["object"], "response");
        assert_eq!(object["id"], "resp_1");
        assert_eq!(object["status"], "completed");
        assert_eq!(object["output"][0]["type"], "message");
        assert_eq!(object["output"][0]["status"], "completed");
        assert_eq!(object["output"][0]["role"], "assistant");
        assert_eq!(object["output"][0]["content"][0]["type"], "output_text");
        assert_eq!(object["output"][0]["content"][0]["text"], "done");
        assert_eq!(object["usage"]["total_tokens"], 5);
        assert_eq!(object["error"], Value::Null);
        assert_eq!(object["incomplete_details"], Value::Null);
    }

    /// A recovered call block must surface as `function_call` output
    /// items whose `call_id`/`name`/`arguments` Codex echoes back in
    /// `function_call_output` next turn - that echo is the whole reason
    /// the emulation exists, so the item shape is asserted field by field.
    #[test]
    fn parsed_calls_become_function_call_output_items() {
        let answer = "```aifuel_tool_calls\n{\"calls\":[{\"name\":\"shell\",\"arguments\":{\"cmd\":\"ls\"}}]}\n```";
        let parsed = parse_tool_calls(answer).expect("a fenced block parses");
        assert!(parsed.content.is_empty());
        let items = output_items("msg_1", answer, Some(&parsed));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[0]["status"], "completed");
        assert_eq!(items[0]["call_id"], "call_0");
        assert_eq!(items[0]["name"], "shell");
        assert_eq!(items[0]["arguments"], "{\"cmd\":\"ls\"}");
    }

    #[test]
    fn text_around_the_block_stays_a_message_item() {
        let answer =
            "checking\n```aifuel_tool_calls\n{\"calls\":[{\"name\":\"x\"}]}\n```\ndone thinking";
        let parsed = parse_tool_calls(answer).expect("a fenced block parses");
        let items = output_items("msg_1", answer, Some(&parsed));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["content"][0]["text"], "checking\ndone thinking");
        assert_eq!(items[1]["type"], "function_call");
    }

    #[test]
    fn call_events_emit_the_arguments_lifecycle() {
        let parsed = parse_tool_calls(
            "```aifuel_tool_calls\n{\"calls\":[{\"name\":\"shell\",\"arguments\":{\"cmd\":\"ls\"}}]}\n```",
        )
        .unwrap();
        let mut seq = 0;
        let events = call_events("resp_1", 0, 1, &parsed.calls[0], &mut seq);
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done",
            ]
        );
        assert_eq!(events[0]["item"]["type"], "function_call");
        assert_eq!(events[1]["delta"], "{\"cmd\":\"ls\"}");
        assert_eq!(events[3]["item"]["arguments"], "{\"cmd\":\"ls\"}");
    }

    #[test]
    fn usage_falls_back_to_a_chars_over_four_estimate() {
        // Providers that report no token accounting still owe clients a
        // completed usage shape; the estimate is documented, not zero.
        let usage = usage_value("12345678", "1234", None);
        assert_eq!(usage["input_tokens"], 2);
        assert_eq!(usage["output_tokens"], 1);
        assert_eq!(usage["total_tokens"], 3);

        // Partial provider counts fill only the missing half.
        let real = TokenUsage {
            input_tokens: Some(7),
            output_tokens: None,
        };
        let usage = usage_value("12345678", "1234", Some(&real));
        assert_eq!(usage["input_tokens"], 7);
        assert_eq!(usage["output_tokens"], 1);
    }

    #[test]
    fn failed_event_populates_the_error() {
        // Codex reads `response.error.{code,message}` off the failed
        // object; a bare status would surface as an opaque stream error.
        let mut seq = 0;
        let event = failed_event("resp_1", 10, "auto", "boom", &mut seq);
        assert_eq!(event["type"], "response.failed");
        assert_eq!(event["response"]["status"], "failed");
        assert_eq!(event["response"]["error"]["code"], "server_error");
        assert_eq!(event["response"]["error"]["message"], "boom");
    }
}
