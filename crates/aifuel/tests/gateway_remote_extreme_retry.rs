#[allow(dead_code)]
#[path = "support/gateway.rs"]
mod gateway_support;
#[path = "support/remote_gateway.rs"]
#[allow(dead_code)]
mod remote_gateway_support;
#[path = "support/streamable_http.rs"]
#[allow(dead_code)]
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
fn finite_sse_extreme_retry_times_out_without_panic_or_reconnect() {
    let temporary = TestDirectory::new("mcp-gateway-remote-finite-extreme-retry");
    let fixture = StreamableHttpFixture::start();
    let config_root = temporary.path().join("config");
    write_remote_catalog(&config_root, &fixture.url(), json!({"operationSeconds":2}));
    let (mut gateway, responses) = start_gateway(&config_root);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    initialize_remote(&fixture, "finite-extreme-session");
    respond_tools_list(&fixture, "finite-extreme-session");
    let listed = response_with_id(&responses, 2);
    assert_eq!(listed["result"]["tools"][0]["name"], "docs__echo");

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{"name":"docs__echo","arguments":{"message":"extreme retry"}}
        }),
    );
    let call = next_remote_post(&fixture, "tools/call", Duration::from_secs(5));
    let upstream_id = call.json()["id"].clone();
    call.respond(
        200,
        Some("text/event-stream"),
        Vec::new(),
        sse_priming_event("finite-extreme-cursor", u64::MAX),
    );

    let result_deadline = Instant::now() + Duration::from_secs(5);
    let mut resumed_early = false;
    let mut call_replayed = false;
    let mut cancellation_seen = false;
    let mut host_result = None;
    while host_result.is_none() {
        let remaining = result_deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "the operation should time out without panicking"
        );
        match fixture.next_request(remaining.min(Duration::from_millis(20))) {
            Ok(request) if request.method == "GET" => {
                resumed_early = true;
                request.respond(405, None, Vec::new(), Vec::new());
            }
            Ok(request) if request.method == "POST" => {
                let body = request.json();
                match body["method"].as_str().unwrap_or_default() {
                    "notifications/cancelled" => {
                        assert_eq!(body["params"]["requestId"], upstream_id);
                        cancellation_seen = true;
                        request.respond(202, None, Vec::new(), Vec::new());
                    }
                    "tools/call" => {
                        call_replayed = true;
                        request.respond(500, None, Vec::new(), Vec::new());
                    }
                    other => panic!("unexpected upstream POST: {other}"),
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => panic!("remote fixture stopped unexpectedly: {error}"),
            Ok(request) => panic!("unexpected upstream request {}", request.method),
        }
        if let Ok(line) = responses.recv_timeout(Duration::from_millis(20)) {
            let message: Value =
                serde_json::from_str(&line).expect("gateway output should be JSON");
            if message["id"] == 3 {
                host_result = Some(message);
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
    let timeout_text = host_result.unwrap()["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    send_message(&mut stdin, json!({"jsonrpc":"2.0","id":4,"method":"ping"}));
    assert_eq!(response_with_id(&responses, 4)["result"], json!({}));
    let stderr =
        finish_gateway_with_stderr(&mut gateway, stdin, &fixture, "finite-extreme-session");

    assert!(
        !resumed_early,
        "the server retry value must not cause early reconnect"
    );
    assert!(!call_replayed, "the tool POST must not be replayed");
    assert!(
        cancellation_seen,
        "the timed out operation should be cancelled upstream"
    );
    assert!(timeout_text.contains("timed out"), "{timeout_text}");
    assert!(!stderr.contains("panicked at"), "{stderr}");
}

#[test]
fn shared_sse_extreme_retry_waits_without_panic_or_reconnect() {
    let temporary = TestDirectory::new("mcp-gateway-remote-shared-extreme-retry");
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
    initialize_remote(&fixture, "shared-extreme-session");

    let mut tools_listed = false;
    let mut common_get_seen = false;
    let setup_deadline = Instant::now() + Duration::from_secs(10);
    while !tools_listed || !common_get_seen {
        let remaining = setup_deadline.saturating_duration_since(Instant::now());
        let request = fixture
            .next_request(remaining)
            .expect("fixture should receive tools/list and the shared event stream");
        match request.method.as_str() {
            "GET" => {
                assert!(!common_get_seen);
                assert_eq!(
                    request.header("mcp-session-id"),
                    Some("shared-extreme-session")
                );
                common_get_seen = true;
                request.respond(
                    200,
                    Some("text/event-stream"),
                    Vec::new(),
                    sse_priming_event("shared-extreme-cursor", u64::MAX),
                );
            }
            "POST" => {
                assert_eq!(request.json()["method"], "tools/list");
                respond_tools_list_request(request, "shared-extreme-session");
                tools_listed = true;
            }
            method => panic!("unexpected upstream method {method}"),
        }
    }
    let listed = response_with_id(&responses, 2);
    assert_eq!(listed["result"]["tools"][0]["name"], "docs__echo");

    let early_get = fixture.next_request(Duration::from_secs(1));
    let resumed_early = match early_get {
        Ok(request) if request.method == "GET" => {
            request.respond(405, None, Vec::new(), Vec::new());
            true
        }
        Ok(request) => panic!("unexpected upstream request {}", request.method),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
        Err(error) => panic!("remote fixture stopped unexpectedly: {error}"),
    };
    send_message(&mut stdin, json!({"jsonrpc":"2.0","id":3,"method":"ping"}));
    assert_eq!(response_with_id(&responses, 3)["result"], json!({}));
    let stderr =
        finish_gateway_with_stderr(&mut gateway, stdin, &fixture, "shared-extreme-session");

    assert!(
        !resumed_early,
        "the server retry value must not cause early reconnect"
    );
    assert!(!stderr.contains("panicked at"), "{stderr}");
}
