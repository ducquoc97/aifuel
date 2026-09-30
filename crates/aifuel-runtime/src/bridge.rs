//! The stdio JSON-RPC bridge: the `aifuel runtime` embedding surface for
//! Host Applications written in any language.
//!
//! One bridge process hosts one [`AgentRuntime`], matching the
//! `codex app-server` model: each consumer spawns its own process and owns
//! the live sessions it starts, while the persisted run store stays
//! shared. Framing is newline-delimited JSON - each line is one complete
//! JSON-RPC 2.0 message with no `Content-Length` headers, the same
//! convention `aifuel mcp execution` uses. The writer is protocol-owned:
//! diagnostics go to stderr only.
//!
//! ## Wire surface
//!
//! Every outbound message carries `v`, the contract's schema version, so
//! the envelope is versioned JSON the same way the contract types inside
//! it are. Inbound `v` may be absent - a plain JSON-RPC 2.0 request can
//! only mean v1, the one version the bridge serves - but a declared `v`
//! must be `1` exactly: any other version answers `-32600`, never a
//! silent dispatch under a version the frame did not ask for.
//!
//! ```json
//! {"jsonrpc":"2.0","v":1,"id":7,"method":"session.create","params":{"cwd":"/repo","selection":{"integration_id":"claude","model":"opus"},"access":"workspace-write"}}
//! {"jsonrpc":"2.0","v":1,"id":7,"result":{"receipt":{"schema_version":1,"command_id":"7","ok":true,"seq":1,"session_id":"s-1"},"payload":null}}
//! {"jsonrpc":"2.0","v":1,"method":"agent.event","params":{"schema_version":1,"session_id":"s-1","seq":2,"ts":1700000000.0,"type":"session.status","status":"idle"}}
//! {"jsonrpc":"2.0","v":1,"id":null,"error":{"code":-32700,"message":"invalid JSON"}}
//! ```
//!
//! `method` is the [`AgentCommand`] `type` tag verbatim and `params`
//! carries that command's fields. The response `result` is the serialized
//! [`CommandOutcome`](crate::CommandOutcome): `receipt` plus the command's `payload` (`null` for
//! most commands; `{"integrations":[...]}`, `{"models":[...]}`, or
//! `{"sessions":[...]}` for the listing commands).
//!
//! ## `command_id`
//!
//! The JSON-RPC request `id` IS the command's `command_id`, stringified:
//! a string id is used verbatim and a number id renders in its JSON form
//! (`7` becomes `"7"`). One identifier keeps the contract's
//! idempotent-retry rule intact on the wire - a host re-sending an
//! ambiguously delivered command reuses the same id and receives the
//! recorded receipt verbatim. `params` may carry `command_id` or `type`
//! redundantly only when they agree with the request `id` and `method`;
//! a disagreement is `-32602 invalid params`.
//!
//! ## Errors and lifecycle
//!
//! Malformed lines answer a JSON-RPC error response, never a panic:
//! `-32700` parse error, `-32600` invalid request, `-32601` method not
//! found, `-32602` invalid params. One `-32600` also answers a frame
//! that is not a request object at all - a batch array (batches stay
//! unimplemented) or a bare primitive - and an object carrying neither
//! `id` nor `method`. Inbound notifications (a `method` and no `id`)
//! have no answer and are ignored. A frame over 1 MiB is a protocol
//! violation the bridge cannot resynchronize past, so it is fatal. EOF
//! on stdin is the orderly end: the runtime's graceful `shutdown` runs
//! before the bridge exits.

use crate::runtime::AgentRuntime;
use aifuel_core::{AGENT_RUNTIME_SCHEMA_VERSION, AgentCommand, AgentEvent, ConsumerId};
use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// The largest accepted request line, matching the execution MCP bound.
const MAX_FRAME_BYTES: u64 = 1024 * 1024;

/// The consumer id the connected client holds inside the runtime. Every
/// dispatch shares it, so `session.subscribe` replays and live events for
/// every session the client attaches to flow through one channel.
const BRIDGE_CONSUMER: &str = "bridge";

/// The JSON-RPC method pushing [`AgentEvent`]s to the client.
const EVENT_METHOD: &str = "agent.event";

/// The contract schema version stamped on every outbound envelope.
const BRIDGE_VERSION: u32 = AGENT_RUNTIME_SCHEMA_VERSION;

// Standard JSON-RPC 2.0 error codes, the same set `aifuel mcp` reports.
const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;

/// The command `type` tags the bridge serves, one JSON-RPC method each.
/// These spellings are the stable contract surface from `commands.rs`;
/// the unit test asserts every [`AgentCommand`] variant is listed.
const KNOWN_METHODS: &[&str] = &[
    "session.create",
    "session.subscribe",
    "session.list",
    "session.close",
    "run.start",
    "run.cancel",
    "approval.answer",
    "model.select",
    "checkpoint.restore",
    "integrations.list",
    "models.list",
];

/// Serve the runtime over newline-delimited JSON-RPC until `reader` hits
/// EOF, then shut the runtime down gracefully. `writer` takes over the
/// protocol channel for the process; nothing else may write to it.
///
/// Requests dispatch through [`AgentRuntime::dispatch`] under the bridge's
/// single consumer id; the events that consumer is subscribed to are
/// pushed back as `agent.event` notifications on a pump thread, so live
/// runs stream while the request loop blocks on the next line.
pub fn serve_stdio<R, W>(runtime: &AgentRuntime, mut reader: R, writer: W) -> Result<(), String>
where
    R: BufRead,
    W: Write + Send + 'static,
{
    let consumer = ConsumerId::new(BRIDGE_CONSUMER);
    let writer = Arc::new(Mutex::new(writer));
    let done = Arc::new(AtomicBool::new(false));
    // `events` hands the receiver out once; a runtime that already gave it
    // away still serves commands, the client just cannot receive pushes.
    let pump = runtime.events(&consumer).map(|events| {
        let writer = Arc::clone(&writer);
        let done = Arc::clone(&done);
        thread::spawn(move || pump_events(events, writer, done))
    });
    let result = serve_requests(runtime, &consumer, &mut reader, &writer);
    done.store(true, Ordering::Release);
    if let Some(pump) = pump {
        let _ = pump.join();
    }
    runtime.shutdown();
    result
}

/// The request loop: one line is one JSON-RPC message. Returns at EOF;
/// read and write failures are the only errors.
fn serve_requests<R, W>(
    runtime: &AgentRuntime,
    consumer: &ConsumerId,
    reader: &mut R,
    writer: &Mutex<W>,
) -> Result<(), String>
where
    R: BufRead,
    W: Write,
{
    loop {
        let mut frame = Vec::new();
        let count = reader
            .by_ref()
            .take(MAX_FRAME_BYTES + 1)
            .read_until(b'\n', &mut frame)
            .map_err(|error| error.to_string())?;
        if count == 0 {
            return Ok(());
        }
        // The newline never arrived inside the bound; the frame's tail is
        // still unread, so no error response could resynchronize the stream.
        if count as u64 > MAX_FRAME_BYTES {
            return Err("runtime bridge frame exceeds 1 MiB".to_owned());
        }
        if frame.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if let Some(response) = handle_frame(runtime, consumer, &frame) {
            write_frame(writer, &response)?;
        }
    }
}

/// Forward the consumer's live events as `agent.event` notifications until
/// the channel disconnects, the writer breaks, or the serve loop ends.
fn pump_events<W: Write>(
    events: std::sync::mpsc::Receiver<AgentEvent>,
    writer: Arc<Mutex<W>>,
    done: Arc<AtomicBool>,
) {
    while !done.load(Ordering::Acquire) {
        match events.recv_timeout(Duration::from_millis(50)) {
            Ok(event) => {
                let notification = json!({
                    "jsonrpc": "2.0",
                    "v": BRIDGE_VERSION,
                    "method": EVENT_METHOD,
                    "params": event,
                });
                if write_frame(&writer, &notification).is_err() {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Answer one frame: `Some` response to write, `None` when the message is
/// an inbound notification that has no answer.
fn handle_frame(runtime: &AgentRuntime, consumer: &ConsumerId, frame: &[u8]) -> Option<Value> {
    let request: Value = match serde_json::from_slice(frame) {
        Ok(request) => request,
        Err(_) => return Some(error_frame(Value::Null, PARSE_ERROR, "invalid JSON")),
    };
    let Some(object) = request.as_object() else {
        // Batches stay unimplemented and primitives are never requests:
        // one invalid-request error answers the whole frame rather than
        // dropping it silently.
        return Some(error_frame(Value::Null, INVALID_REQUEST, "invalid request"));
    };
    let Some(id) = object.get("id").cloned() else {
        if object.contains_key("method") {
            // A JSON-RPC notification is not answered. The inbound
            // surface is request-only, so notifications are ignored.
            return None;
        }
        // Neither a request nor a notification is not a message the
        // protocol defines; it is an invalid request.
        return Some(error_frame(Value::Null, INVALID_REQUEST, "invalid request"));
    };
    if request["jsonrpc"] != "2.0" {
        return Some(error_frame(Value::Null, INVALID_REQUEST, "invalid request"));
    }
    // The envelope's schema version: absent reads as v1 - a plain
    // JSON-RPC 2.0 request can only mean the one version the bridge
    // serves - while a declared `v` must be exactly it. Any other
    // version is an invalid request, never a silent v1 dispatch. The
    // error still echoes the request id so the host can correlate it.
    match object.get("v") {
        None => {}
        Some(version) if version.as_u64() == Some(u64::from(BRIDGE_VERSION)) => {}
        Some(_) => {
            return Some(error_frame(
                id,
                INVALID_REQUEST,
                "unsupported schema version",
            ));
        }
    }
    // The request id carries the command identity; only string and number
    // ids can carry one (per JSON-RPC and the `command_id` rule above).
    let command_id = match &id {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => {
            return Some(error_frame(
                Value::Null,
                INVALID_REQUEST,
                "request id must be a string or a number",
            ));
        }
    };
    let Some(method) = request["method"].as_str() else {
        return Some(error_frame(id, INVALID_REQUEST, "invalid request"));
    };
    if !KNOWN_METHODS.contains(&method) {
        return Some(error_frame(id, METHOD_NOT_FOUND, "method not found"));
    }
    let params = match request.get("params") {
        None => json!({}),
        Some(params @ Value::Object(_)) => params.clone(),
        Some(_) => {
            return Some(error_frame(id, INVALID_PARAMS, "params must be an object"));
        }
    };
    match command_from_parts(method, &command_id, params) {
        Ok(command) => {
            let outcome = runtime.dispatch(command, consumer);
            Some(json!({
                "jsonrpc": "2.0",
                "v": BRIDGE_VERSION,
                "id": id,
                "result": outcome,
            }))
        }
        Err(message) => Some(error_frame(id, INVALID_PARAMS, &message)),
    }
}

/// Rebuild the [`AgentCommand`] a request names: the method supplies the
/// `type` tag and the id supplies `command_id`; everything else comes from
/// `params`. Fields restating those two must agree with the envelope, so
/// a host that puts a fully serialized command in `params` validates
/// against its own request rather than silently disagreeing.
fn command_from_parts(
    method: &str,
    command_id: &str,
    mut params: Value,
) -> Result<AgentCommand, String> {
    let object = params.as_object_mut().expect("params is an object");
    if let Some(tag) = object.get("type")
        && tag.as_str() != Some(method)
    {
        return Err("params.type disagrees with the request method".to_owned());
    }
    if let Some(stated) = object.get("command_id")
        && stated.as_str() != Some(command_id)
    {
        return Err("params.command_id disagrees with the request id".to_owned());
    }
    object.insert("type".to_owned(), Value::String(method.to_owned()));
    object.insert(
        "command_id".to_owned(),
        Value::String(command_id.to_owned()),
    );
    serde_json::from_value(params).map_err(|error| format!("invalid params: {error}"))
}

/// One JSON-RPC error response frame.
fn error_frame(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "v": BRIDGE_VERSION,
        "id": id,
        "error": {"code": code, "message": message},
    })
}

/// Serialize one frame to the protocol channel as a single line. Responses
/// and notifications share the writer's lock, so one frame never
/// interleaves with another.
fn write_frame<W: Write>(writer: &Mutex<W>, frame: &Value) -> Result<(), String> {
    let mut output = writer
        .lock()
        .map_err(|_| "the protocol writer lock is poisoned".to_owned())?;
    serde_json::to_writer(&mut *output, frame).map_err(|error| error.to_string())?;
    output
        .write_all(b"\n")
        .and_then(|()| output.flush())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aifuel_core::{
        AccessMode, ApprovalDecision, CheckpointId, CommandId, IntegrationId, ModelSelection,
        RequestId, RunId, SessionId, UserInput,
    };
    use std::path::PathBuf;

    fn command_id() -> CommandId {
        CommandId::new("cmd-1")
    }

    /// Every `AgentCommand` variant's `type` tag is a method the bridge
    /// serves: the list above and the contract enum cannot drift apart.
    #[test]
    fn every_command_type_tag_is_a_served_method() {
        let commands = [
            AgentCommand::SessionCreate {
                command_id: command_id(),
                cwd: PathBuf::from("/repo"),
                selection: ModelSelection {
                    integration_id: IntegrationId::new("i"),
                    model: "m".to_owned(),
                    effort: None,
                },
                access: AccessMode::ReadOnly,
            },
            AgentCommand::SessionSubscribe {
                command_id: command_id(),
                session_id: SessionId::new("s"),
                last_seen_seq: 0,
            },
            AgentCommand::SessionList {
                command_id: command_id(),
            },
            AgentCommand::SessionClose {
                command_id: command_id(),
                session_id: SessionId::new("s"),
            },
            AgentCommand::RunStart {
                command_id: command_id(),
                session_id: SessionId::new("s"),
                input: UserInput {
                    text: "hi".to_owned(),
                    attachments: Vec::new(),
                },
            },
            AgentCommand::RunCancel {
                command_id: command_id(),
                session_id: SessionId::new("s"),
                run_id: RunId::new("r"),
            },
            AgentCommand::ApprovalAnswer {
                command_id: command_id(),
                session_id: SessionId::new("s"),
                request_id: RequestId::new("q"),
                decision: ApprovalDecision::OptionId("accept".to_owned()),
            },
            AgentCommand::ModelSelect {
                command_id: command_id(),
                session_id: SessionId::new("s"),
                selection: ModelSelection {
                    integration_id: IntegrationId::new("i"),
                    model: "m".to_owned(),
                    effort: None,
                },
            },
            AgentCommand::CheckpointRestore {
                command_id: command_id(),
                session_id: SessionId::new("s"),
                checkpoint_id: CheckpointId::new("c"),
            },
            AgentCommand::IntegrationsList {
                command_id: command_id(),
            },
            AgentCommand::ModelsList {
                command_id: command_id(),
                integration_id: IntegrationId::new("i"),
            },
        ];
        for command in &commands {
            let value = serde_json::to_value(command).expect("command serializes");
            let tag = value["type"].as_str().expect("type tag serializes");
            assert!(KNOWN_METHODS.contains(&tag), "the bridge must serve {tag}");
        }
        assert_eq!(KNOWN_METHODS.len(), commands.len());
    }

    /// A request round-trips through `command_from_parts` with the id
    /// becoming `command_id` and the method becoming `type`; restated
    /// fields that agree are accepted, disagreements are rejected.
    #[test]
    fn command_parts_validate_the_restatements() {
        let command = command_from_parts("session.list", "9", json!({})).expect("builds");
        assert_eq!(command.command_id().as_str(), "9");

        // A host that serializes the whole command into params validates.
        let mut params = serde_json::to_value(&command).expect("command serializes");
        let command =
            command_from_parts("session.list", "9", params.clone()).expect("agreeing restates");
        assert_eq!(command.command_id().as_str(), "9");

        params["type"] = json!("session.close");
        assert!(command_from_parts("session.list", "9", params).is_err());
        let mut params = serde_json::to_value(&command).expect("command serializes");
        params["command_id"] = json!("other");
        assert!(command_from_parts("session.list", "9", params).is_err());
    }
}
