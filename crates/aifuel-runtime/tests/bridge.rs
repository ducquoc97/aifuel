//! End-to-end tests for the stdio JSON-RPC bridge: `serve_stdio` over
//! in-memory pipes, driving the same bytes a Host Application sends -
//! command/response round-trips, `agent.event` notifications, malformed
//! input error responses, and the EOF shutdown path.

mod support;

use aifuel_core::{SessionId, SessionStatus};
use aifuel_runtime::{AgentRuntime, bridge::serve_stdio};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use support::{FAKE_INTEGRATION, FakeAdapter, FakeScript, fake_runtime, test_dir};

/// A connected bridge client: a request pipe plus a parsed frame stream
/// carrying responses and `agent.event` notifications in arrival order.
struct Bridge {
    requests: std::io::PipeWriter,
    lines: Receiver<Value>,
    server: Option<JoinHandle<Result<(), String>>>,
}

impl Bridge {
    /// Spawn `serve_stdio` over anonymous pipes. The response pipe drains
    /// on a reader thread so the bridge never blocks on a full pipe.
    fn start(runtime: Arc<AgentRuntime>) -> Self {
        let (request_read, requests) = std::io::pipe().expect("request pipe");
        let (response_read, response_write) = std::io::pipe().expect("response pipe");
        let server = thread::spawn(move || {
            serve_stdio(&runtime, BufReader::new(request_read), response_write)
        });
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(response_read).lines() {
                let line = line.expect("the bridge writes UTF-8 lines");
                let frame = serde_json::from_str(&line)
                    .unwrap_or_else(|error| panic!("each frame is valid JSON: {error}: {line}"));
                if sender.send(frame).is_err() {
                    return;
                }
            }
        });
        Self {
            requests,
            lines,
            server: Some(server),
        }
    }

    /// Write one JSON-RPC request frame.
    fn send(&mut self, request: &Value) {
        self.send_raw(&serde_json::to_string(request).expect("request serializes"));
    }

    /// Write one raw line, for the malformed-input paths.
    fn send_raw(&mut self, line: &str) {
        self.requests
            .write_all(line.as_bytes())
            .and_then(|()| self.requests.write_all(b"\n"))
            .and_then(|()| self.requests.flush())
            .expect("the request writes");
    }

    /// The next frame satisfying `predicate`, failing loudly when none
    /// arrives so a protocol regression cannot hang the suite.
    fn next(&self, predicate: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..600 {
            let frame = self
                .lines
                .recv_timeout(Duration::from_secs(15))
                .expect("the next frame arrives");
            if predicate(&frame) {
                return frame;
            }
        }
        panic!("the expected frame never arrived");
    }

    /// Every frame until `stop` matches, in arrival order - responses and
    /// notifications interleave the way the client sees them.
    fn collect_until(&self, stop: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut frames = Vec::new();
        for _ in 0..600 {
            let frame = self
                .lines
                .recv_timeout(Duration::from_secs(15))
                .expect("the next frame arrives");
            let done = stop(&frame);
            frames.push(frame);
            if done {
                return frames;
            }
        }
        panic!("the expected frame never arrived");
    }

    /// Send a request and return its response, skipping any notifications
    /// that land in between.
    fn call(&mut self, request: &Value) -> Value {
        let id = request["id"].clone();
        self.send(request);
        self.next(|frame| frame.get("id") == Some(&id))
    }

    /// Drop stdin: the bridge sees EOF, shuts the runtime down, and the
    /// serve thread returns its result.
    fn close(self) -> Result<(), String> {
        let Self {
            requests, server, ..
        } = self;
        drop(requests);
        server
            .expect("the serve thread runs")
            .join()
            .expect("the serve thread joins")
    }
}

/// `session.create` naming the fake integration.
fn create_request(dir: &Path, id: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "v": 1,
        "id": id,
        "method": "session.create",
        "params": {
            "cwd": dir,
            "selection": {"integration_id": FAKE_INTEGRATION, "model": "fake-a"},
            "access": "workspace-write",
        },
    })
}

/// Whether the frame is an `agent.event` notification carrying `kind`.
fn is_event(frame: &Value, kind: &str) -> bool {
    frame["method"] == "agent.event" && frame["params"]["type"] == kind
}

/// Commands answer one response carrying the serialized CommandOutcome:
/// the contract receipt plus the listing payload beside it. The request
/// id - string or number - becomes the receipt's `command_id`.
#[test]
fn commands_round_trip_receipt_and_payload() {
    let dir = test_dir("bridge-roundtrip");
    let (_store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![],
    );
    let mut bridge = Bridge::start(Arc::new(runtime));

    // A numeric id is the command_id in its JSON string form.
    let response = bridge.call(&json!({
        "jsonrpc": "2.0", "v": 1, "id": 7, "method": "session.list", "params": {},
    }));
    assert_eq!(response["v"], 1, "the envelope is versioned");
    assert_eq!(response["id"], 7, "the response echoes the request id");
    assert_eq!(response["result"]["receipt"]["command_id"], "7");
    assert_eq!(response["result"]["receipt"]["ok"], true);
    assert_eq!(response["result"]["payload"]["sessions"], json!([]));

    let response = bridge.call(&create_request(&dir, "create-1"));
    let receipt = &response["result"]["receipt"];
    assert_eq!(receipt["ok"], true, "session.create succeeds: {receipt}");
    assert_eq!(receipt["seq"], 1, "session.created is the log's first fact");
    let session_id = receipt["session_id"]
        .as_str()
        .expect("the receipt names the session")
        .to_owned();

    // integrations.list reports the fake integration in its payload.
    let response = bridge.call(&json!({
        "jsonrpc": "2.0", "id": "list-1", "method": "integrations.list",
    }));
    let integrations = response["result"]["payload"]["integrations"]
        .as_array()
        .expect("integrations payload");
    assert_eq!(integrations.len(), 1);
    assert_eq!(integrations[0]["integration_id"], FAKE_INTEGRATION);

    // A params object restating type and command_id - a host serializing
    // the whole AgentCommand as params - validates against the envelope.
    let response = bridge.call(&json!({
        "jsonrpc": "2.0", "id": "sub-1", "method": "session.subscribe",
        "params": {
            "type": "session.subscribe",
            "command_id": "sub-1",
            "session_id": session_id,
            "last_seen_seq": 0,
        },
    }));
    let receipt = &response["result"]["receipt"];
    assert_eq!(receipt["ok"], true, "session.subscribe succeeds: {receipt}");
    assert_eq!(receipt["snapshot"]["status"], "idle");
    assert_eq!(receipt["snapshot"]["selection"]["model"], "fake-a");

    bridge.close().expect("EOF shuts down cleanly");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The replayed log and live run events reach the client as
/// `agent.event` notifications in Session Event Log order, interleaved
/// with command responses on the one protocol channel.
#[test]
fn run_events_arrive_as_agent_event_notifications() {
    let dir = test_dir("bridge-events");
    let (_store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![FakeScript::Complete {
            deltas: vec!["he".to_owned(), "llo".to_owned()],
            cursor: None,
        }],
    );
    let mut bridge = Bridge::start(Arc::new(runtime));

    let response = bridge.call(&create_request(&dir, "create-1"));
    let session_id = response["result"]["receipt"]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    bridge.send(&json!({
        "jsonrpc": "2.0", "id": "sub-1", "method": "session.subscribe",
        "params": {"session_id": session_id, "last_seen_seq": 0},
    }));
    bridge.send(&json!({
        "jsonrpc": "2.0", "id": "run-1", "method": "run.start",
        "params": {"session_id": session_id, "input": {"text": "go"}},
    }));

    let frames = bridge.collect_until(|frame| is_event(frame, "run.completed"));
    // Both command responses landed, notifications interleaved between them.
    let response = |id: &str| {
        frames
            .iter()
            .find(|frame| frame.get("id") == Some(&json!(id)))
            .unwrap_or_else(|| panic!("a response for {id}"))
    };
    assert_eq!(response("sub-1")["result"]["receipt"]["ok"], true);
    assert_eq!(response("run-1")["result"]["receipt"]["ok"], true);

    // The notification stream carries the subscribe replay first
    // (session.created, the idle prelude) then the run's causal order.
    let kinds: Vec<&str> = frames
        .iter()
        .filter(|frame| frame["method"] == "agent.event")
        .map(|frame| {
            assert_eq!(frame["v"], 1, "the notification envelope is versioned");
            frame["params"]["type"].as_str().expect("event type tag")
        })
        .collect();
    assert_eq!(&kinds[..2], ["session.created", "session.status"]);
    assert_eq!(kinds[2], "run.started");
    assert!(
        kinds.contains(&"session.status"),
        "the run's status facts arrive"
    );

    // The scripted deltas stream in order, then the terminal fact.
    let deltas: Vec<&str> = frames
        .iter()
        .filter(|frame| is_event(frame, "message.delta"))
        .map(|frame| frame["params"]["text"].as_str().expect("delta text"))
        .collect();
    assert_eq!(deltas, ["he", "llo"]);
    let completed = frames
        .iter()
        .find(|frame| is_event(frame, "run.completed"))
        .expect("the run completes");
    assert_eq!(completed["params"]["outcome"], "success");
    assert_eq!(
        completed["params"]["session_id"], session_id,
        "the event envelope names its session"
    );
    // Every fact carries the log's monotonic sequence.
    let seqs: Vec<u64> = frames
        .iter()
        .filter(|frame| frame["method"] == "agent.event")
        .filter_map(|frame| frame["params"]["seq"].as_u64())
        .collect();
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "seq increases across the notification stream: {seqs:?}"
    );

    bridge.close().expect("EOF shuts down cleanly");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Malformed input answers a JSON-RPC error response instead of a panic:
/// unparseable lines, a bad envelope, a bad id, an unknown method, wrong
/// params, and a restated `command_id` that disagrees with the request id.
#[test]
fn malformed_frames_answer_error_responses() {
    let dir = test_dir("bridge-errors");
    let (_store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![],
    );
    let mut bridge = Bridge::start(Arc::new(runtime));

    bridge.send_raw("this is not json");
    let response = bridge.next(|frame| frame.get("error").is_some());
    assert_eq!(response["id"], Value::Null);
    assert_eq!(response["error"]["code"], -32700);

    // The envelope must declare JSON-RPC 2.0.
    bridge.send(&json!({"id": 1, "method": "session.list"}));
    let response = bridge.next(|frame| frame.get("error").is_some());
    assert_eq!(response["error"]["code"], -32600);

    // A null or structured id cannot carry a command identity.
    bridge.send(&json!({"jsonrpc": "2.0", "id": null, "method": "session.list"}));
    let response = bridge.next(|frame| frame.get("error").is_some());
    assert_eq!(response["error"]["code"], -32600);

    bridge.send(&json!({"jsonrpc": "2.0", "id": "x", "method": "no.such.method"}));
    let response = bridge.next(|frame| frame.get("error").is_some());
    assert_eq!(response["id"], "x", "the error echoes the request id");
    assert_eq!(response["error"]["code"], -32601);

    // Missing required fields and disagreement with the envelope are
    // invalid params, not panics.
    bridge.send(&json!({
        "jsonrpc": "2.0", "id": "p1", "method": "session.subscribe",
        "params": {"session_id": "s-1"},
    }));
    let response = bridge.next(|frame| frame.get("error").is_some());
    assert_eq!(response["error"]["code"], -32602);
    bridge.send(&json!({
        "jsonrpc": "2.0", "id": "p2", "method": "session.list",
        "params": {"command_id": "not-p2"},
    }));
    let response = bridge.next(|frame| frame.get("error").is_some());
    assert_eq!(response["error"]["code"], -32602);

    // An inbound notification is not answered: the only frame a later
    // request produces is its own response.
    bridge.send(&json!({"jsonrpc": "2.0", "method": "session.list"}));
    let frames = {
        bridge.send(&json!({"jsonrpc": "2.0", "id": "after", "method": "session.list"}));
        bridge.collect_until(|frame| frame.get("id") == Some(&json!("after")))
    };
    assert_eq!(
        frames.len(),
        1,
        "the notification produced no frame: {frames:?}"
    );

    // The bridge still serves commands after every malformed frame.
    assert_eq!(frames[0]["result"]["receipt"]["ok"], true);
    bridge.close().expect("EOF shuts down cleanly");
    let _ = std::fs::remove_dir_all(&dir);
}

/// EOF on stdin is the orderly end: the serve loop returns, the runtime's
/// graceful shutdown records in-flight sessions as `interrupted` in the
/// Session Event Log, and the process exit path stays clean.
#[test]
fn eof_shuts_down_orderly_with_an_in_flight_run() {
    let dir = test_dir("bridge-eof");
    let (store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![FakeScript::Block { cursor: None }],
    );
    let mut bridge = Bridge::start(Arc::new(runtime));

    let response = bridge.call(&create_request(&dir, "create-1"));
    let session_id = response["result"]["receipt"]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    bridge.call(&json!({
        "jsonrpc": "2.0", "id": "sub-1", "method": "session.subscribe",
        "params": {"session_id": session_id, "last_seen_seq": 0},
    }));
    bridge.send(&json!({
        "jsonrpc": "2.0", "id": "run-1", "method": "run.start",
        "params": {"session_id": session_id, "input": {"text": "go"}},
    }));
    // The blocking script is in flight before stdin closes.
    bridge
        .next(|frame| is_event(frame, "session.status") && frame["params"]["status"] == "working");

    bridge.close().expect("the bridge exits cleanly on EOF");
    let session = store
        .agent_session(&SessionId::new(session_id))
        .expect("session reads")
        .expect("session exists");
    assert_eq!(
        session.status,
        SessionStatus::Interrupted,
        "shutdown records the in-flight session interrupted"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
