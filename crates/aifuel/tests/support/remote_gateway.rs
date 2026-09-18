use super::streamable_http::{PendingRequest, StreamableHttpFixture};
use crate::gateway_support::{wait_for_exit, write_catalog};
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;
use std::process::{Child, ChildStdin};
use std::time::{Duration, Instant};

pub fn write_remote_catalog(config_root: &Path, url: &str, limits: Value) {
    write_catalog(
        config_root,
        json!({
            "servers": {
                "docs": {
                    "transport":"streamable-http",
                    "url":url,
                    "limits":limits
                }
            },
            "defaults":[],
            "agents":{"codex":{"servers":["docs"]}}
        }),
    );
}

pub fn initialize_remote(fixture: &StreamableHttpFixture, session_id: &str) {
    let initialize = next_remote_post(fixture, "initialize", Duration::from_secs(10));
    assert_eq!(initialize.path, "/mcp");
    assert!(initialize.header("accept").is_some_and(
        |value| value.contains("application/json") && value.contains("text/event-stream")
    ));
    assert_eq!(initialize.json()["params"]["protocolVersion"], "2025-11-25");
    assert_eq!(initialize.header("mcp-protocol-version"), None);
    assert_eq!(initialize.header("mcp-session-id"), None);
    let request_id = initialize.json()["id"].clone();
    initialize.respond_json(
        200,
        vec![("MCP-Session-Id".to_owned(), session_id.to_owned())],
        initialize_result(request_id),
    );

    let initialized = next_remote_post(
        fixture,
        "notifications/initialized",
        Duration::from_secs(10),
    );
    assert_eq!(initialized.header("mcp-session-id"), Some(session_id));
    assert_eq!(
        initialized.header("mcp-protocol-version"),
        Some("2025-11-25")
    );
    initialized.respond(202, None, Vec::new(), Vec::new());
}

pub fn initialize_result(request_id: Value) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id":request_id,
        "result":{
            "protocolVersion":"2025-11-25",
            "capabilities":{"tools":{"listChanged":true}},
            "serverInfo":{"name":"remote-fixture","version":"1"}
        }
    })
}

pub fn respond_tools_list(fixture: &StreamableHttpFixture, session_id: &str) {
    let request = next_remote_post(fixture, "tools/list", Duration::from_secs(10));
    respond_tools_list_request(request, session_id);
}

pub fn respond_tools_list_request(request: PendingRequest, session_id: &str) {
    assert_eq!(request.header("mcp-session-id"), Some(session_id));
    assert_eq!(request.header("mcp-protocol-version"), Some("2025-11-25"));
    let request_id = request.json()["id"].clone();
    request.respond_json(
        200,
        Vec::new(),
        json!({
            "jsonrpc":"2.0",
            "id":request_id,
            "result":{
                "tools":[{
                    "name":"echo",
                    "description":"Returns the supplied message",
                    "inputSchema":{
                        "type":"object",
                        "properties":{"message":{"type":"string"}},
                        "required":["message"]
                    },
                    "annotations":{"readOnlyHint":false}
                }]
            }
        }),
    );
}

pub fn next_remote_post(
    fixture: &StreamableHttpFixture,
    method: &str,
    timeout: Duration,
) -> PendingRequest {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let request = fixture
            .next_request(remaining)
            .expect("remote fixture should receive the expected request");
        assert_eq!(request.path, "/mcp");
        if request.method == "GET" {
            assert_eq!(request.header("accept"), Some("text/event-stream"));
            assert_eq!(request.header("last-event-id"), None);
            request.respond(405, None, Vec::new(), Vec::new());
            continue;
        }
        assert_eq!(request.method, "POST");
        assert_eq!(request.json()["method"], method);
        return request;
    }
}

pub fn next_resume_get(
    fixture: &StreamableHttpFixture,
    event_id: &str,
    timeout: Duration,
) -> Result<PendingRequest, &'static str> {
    let deadline = Instant::now() + timeout;
    loop {
        let request = fixture
            .next_request(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| "timeout")?;
        if request.method == "GET" && request.header("last-event-id").is_none() {
            request.respond(405, None, Vec::new(), Vec::new());
            continue;
        }
        assert_eq!(request.method, "GET");
        assert_eq!(request.header("last-event-id"), Some(event_id));
        assert_eq!(request.path, "/mcp");
        assert_eq!(request.header("accept"), Some("text/event-stream"));
        return Ok(request);
    }
}

pub fn assert_no_upstream_post(fixture: &StreamableHttpFixture, duration: Duration) {
    let deadline = Instant::now() + duration;
    loop {
        match fixture.next_request(deadline.saturating_duration_since(Instant::now())) {
            Ok(request) if request.method == "GET" => {
                assert_eq!(request.header("accept"), Some("text/event-stream"));
                request.respond(405, None, Vec::new(), Vec::new());
            }
            Ok(request) if request.method == "POST" => {
                panic!(
                    "unexpected upstream POST during no-replay window: {}",
                    request.json()["method"]
                );
            }
            Ok(request) => panic!("unexpected upstream request {}", request.method),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return,
            Err(error) => panic!("remote fixture stopped unexpectedly: {error}"),
        }
    }
}

#[allow(dead_code)]
pub fn sse_event(event_id: &str, retry_ms: Option<u64>, message: &Value) -> String {
    let mut event = format!("id: {event_id}\n");
    if let Some(retry_ms) = retry_ms {
        event.push_str(&format!("retry: {retry_ms}\n"));
    }
    event.push_str(&format!(
        "data: {}\n\n",
        serde_json::to_string(message).unwrap()
    ));
    event
}

pub fn sse_priming_event(event_id: &str, retry_ms: u64) -> String {
    format!("id: {event_id}\nretry: {retry_ms}\ndata:\n\n")
}

pub fn finish_gateway(
    gateway: &mut Child,
    stdin: ChildStdin,
    fixture: &StreamableHttpFixture,
    session_id: &str,
) {
    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(5);
    let delete = loop {
        let request = fixture
            .next_request(deadline.saturating_duration_since(Instant::now()))
            .expect("gateway shutdown should terminate its upstream HTTP session");
        if request.method == "GET" {
            assert_eq!(request.header("accept"), Some("text/event-stream"));
            request.respond(405, None, Vec::new(), Vec::new());
            continue;
        }
        break request;
    };
    assert_eq!(delete.method, "DELETE");
    assert_eq!(delete.header("mcp-session-id"), Some(session_id));
    assert_eq!(delete.header("mcp-protocol-version"), Some("2025-11-25"));
    delete.respond(200, None, Vec::new(), Vec::new());
    assert!(wait_for_exit(gateway, Duration::from_secs(5)).success());
}

#[allow(dead_code)]
pub fn finish_gateway_with_stderr(
    gateway: &mut Child,
    stdin: ChildStdin,
    fixture: &StreamableHttpFixture,
    session_id: &str,
) -> String {
    finish_gateway(gateway, stdin, fixture, session_id);
    let mut stderr = String::new();
    if let Some(mut pipe) = gateway.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    stderr
}
