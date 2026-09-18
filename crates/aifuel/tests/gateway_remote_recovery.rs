#[allow(dead_code)]
#[path = "support/gateway.rs"]
mod gateway_support;
#[path = "support/remote_gateway.rs"]
mod remote_gateway_support;
#[path = "support/streamable_http.rs"]
mod streamable_http;
#[allow(dead_code)]
mod support;

use gateway_support::*;
use remote_gateway_support::*;
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use streamable_http::StreamableHttpFixture;
use support::TestDirectory;

#[test]
fn remote_session_expiry_during_get_resume_reinitializes_without_replaying_tool_call() {
    let temporary = TestDirectory::new("mcp-gateway-remote-expired-session");
    let fixture = StreamableHttpFixture::start();
    let config_root = temporary.path().join("config");
    write_remote_catalog(&config_root, &fixture.url(), json!({}));
    let (mut gateway, responses) = start_gateway(&config_root);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    initialize_remote(&fixture, "expired-session");
    respond_tools_list(&fixture, "expired-session");
    let listed = response_with_id(&responses, 2);
    assert_eq!(listed["result"]["tools"][0]["name"], "docs__echo");

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{"name":"docs__echo","arguments":{"message":"do not replay"}}
        }),
    );
    let call = next_remote_post(&fixture, "tools/call", Duration::from_secs(5));
    let original_call_id = call.json()["id"].clone();
    call.respond(
        200,
        Some("text/event-stream"),
        Vec::new(),
        sse_priming_event("expired-event", 1),
    );
    let resumed = next_resume_get(&fixture, "expired-event", Duration::from_secs(5))
        .expect("remote fixture should receive the resumed GET");
    assert_eq!(resumed.header("mcp-session-id"), Some("expired-session"));
    resumed.respond(404, None, Vec::new(), Vec::new());

    let reinitialize = next_remote_post(&fixture, "initialize", Duration::from_secs(5));
    assert_eq!(reinitialize.header("mcp-session-id"), None);
    assert_eq!(reinitialize.header("mcp-protocol-version"), None);
    let reinitialize_id = reinitialize.json()["id"].clone();
    assert_ne!(reinitialize_id, original_call_id);
    reinitialize.respond_json(
        200,
        vec![("MCP-Session-Id".to_owned(), "fresh-session".to_owned())],
        initialize_result(reinitialize_id),
    );
    let initialized = next_remote_post(
        &fixture,
        "notifications/initialized",
        Duration::from_secs(5),
    );
    assert_eq!(initialized.header("mcp-session-id"), Some("fresh-session"));
    assert_eq!(
        initialized.header("mcp-protocol-version"),
        Some("2025-11-25")
    );
    initialized.respond(202, None, Vec::new(), Vec::new());

    let failed_call = response_with_id_timeout(&responses, 3, Duration::from_secs(5));
    assert_eq!(failed_call["error"]["code"], -32603);
    assert!(
        failed_call["error"]["message"]
            .as_str()
            .unwrap()
            .contains("session expired")
    );
    assert!(matches!(
        fixture.next_request(Duration::from_millis(200)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":4,
            "method":"tools/call",
            "params":{"name":"docs__echo","arguments":{"message":"new session"}}
        }),
    );
    let next_call = next_remote_post(&fixture, "tools/call", Duration::from_secs(5));
    assert_eq!(next_call.header("mcp-session-id"), Some("fresh-session"));
    let next_call_id = next_call.json()["id"].clone();
    next_call.respond_json(
        200,
        Vec::new(),
        json!({
            "jsonrpc":"2.0",
            "id":next_call_id,
            "result":{"content":[{"type":"text","text":"new-session-result"}],"isError":false}
        }),
    );
    let result = response_with_id(&responses, 4);
    assert_eq!(result["result"]["content"][0]["text"], "new-session-result");

    finish_gateway(&mut gateway, stdin, &fixture, "fresh-session");
}

#[test]
fn remote_sse_respects_sixty_second_retry_and_absolute_operation_deadline() {
    let temporary = TestDirectory::new("mcp-gateway-remote-retry");
    let fixture = StreamableHttpFixture::start();
    let config_root = temporary.path().join("config");
    write_remote_catalog(&config_root, &fixture.url(), json!({"operationSeconds":35}));
    let (mut gateway, responses) = start_gateway(&config_root);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    initialize_remote(&fixture, "retry-session");
    respond_tools_list(&fixture, "retry-session");
    let listed = response_with_id(&responses, 2);
    assert_eq!(listed["result"]["tools"][0]["name"], "docs__echo");

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{"name":"docs__echo","arguments":{"message":"wait for retry"}}
        }),
    );
    let call = next_remote_post(&fixture, "tools/call", Duration::from_secs(5));
    let upstream_id = call.json()["id"].clone();
    call.respond(
        200,
        Some("text/event-stream"),
        Vec::new(),
        sse_priming_event("slow-event", 60_000),
    );

    let deadline = Instant::now() + Duration::from_secs(40);
    let mut cancellation_seen = false;
    let mut host_result = None;
    while host_result.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "tool call should honor its absolute deadline"
        );
        match fixture.next_request(remaining.min(Duration::from_millis(20))) {
            Ok(request) if request.method == "GET" => {
                panic!("the remote response stream must not resume before retry: 60000ms")
            }
            Ok(request) if request.method == "POST" => {
                let body = request.json();
                match body["method"].as_str().unwrap_or_default() {
                    "notifications/cancelled" => {
                        assert_eq!(body["params"]["requestId"], upstream_id);
                        cancellation_seen = true;
                        request.respond(202, None, Vec::new(), Vec::new());
                    }
                    "tools/call" => panic!("the gateway must never replay a tool POST"),
                    other => panic!("unexpected remote request during retry wait: {other}"),
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => panic!("remote fixture stopped unexpectedly: {error}"),
            Ok(request) => panic!("unexpected HTTP method {}", request.method),
        }

        if let Ok(line) = responses.recv_timeout(Duration::from_millis(20)) {
            let message: Value =
                serde_json::from_str(&line).expect("gateway output should be JSON");
            if message["id"] == 3 {
                host_result = Some(message);
            } else {
                panic!("unexpected gateway response while waiting: {message}");
            }
        }
    }
    if !cancellation_seen && let Ok(request) = fixture.next_request(Duration::from_secs(1)) {
        let body = request.json();
        assert_eq!(body["method"], "notifications/cancelled");
        assert_eq!(body["params"]["requestId"], upstream_id);
        request.respond(202, None, Vec::new(), Vec::new());
        cancellation_seen = true;
    }
    assert!(
        cancellation_seen,
        "the gateway should send upstream cancellation"
    );
    let host_result = host_result.unwrap();
    let timeout_message = host_result["result"]["content"][0]["text"]
        .as_str()
        .or_else(|| host_result["error"]["message"].as_str())
        .unwrap_or_default();
    assert!(timeout_message.contains("timed out"), "{host_result}");
    assert!(timeout_message.contains("unknown") || timeout_message.contains("outcome"));

    finish_gateway(&mut gateway, stdin, &fixture, "retry-session");
}
