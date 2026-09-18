#[allow(dead_code)]
#[path = "support/gateway.rs"]
mod gateway_support;
#[path = "support/remote_gateway.rs"]
#[allow(dead_code)]
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
fn finite_sse_retry_delay_survives_a_failed_resumed_get() {
    let temporary = TestDirectory::new("mcp-gateway-remote-finite-retry");
    let fixture = StreamableHttpFixture::start();
    let config_root = temporary.path().join("config");
    write_remote_catalog(&config_root, &fixture.url(), json!({"operationSeconds":75}));
    let (mut gateway, responses) = start_gateway(&config_root);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    initialize_remote(&fixture, "finite-retry-session");
    respond_tools_list(&fixture, "finite-retry-session");
    let listed = response_with_id(&responses, 2);
    assert_eq!(listed["result"]["tools"][0]["name"], "docs__echo");

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{"name":"docs__echo","arguments":{"message":"preserve retry"}}
        }),
    );
    let call = next_remote_post(&fixture, "tools/call", Duration::from_secs(5));
    let upstream_id = call.json()["id"].clone();
    call.respond(
        200,
        Some("text/event-stream"),
        Vec::new(),
        sse_priming_event("finite-fast", 1),
    );

    let first_resume = next_resume_get(&fixture, "finite-fast", Duration::from_secs(5))
        .expect("finite SSE should resume after its short initial retry");
    let slow_event = sse_priming_event("finite-slow", 60_000).into_bytes();
    first_resume.respond_chunked(
        200,
        Some("text/event-stream"),
        Vec::new(),
        slow_event.chunks(13).map(|chunk| chunk.to_vec()).collect(),
    );

    let retry_started = Instant::now();
    let failed_resume = next_resume_get(&fixture, "finite-slow", Duration::from_secs(65))
        .expect("finite SSE should resume after the 60000 ms retry delay");
    assert!(retry_started.elapsed() >= Duration::from_secs(59));
    failed_resume.respond(500, None, Vec::new(), Vec::new());

    let observe_until = Instant::now() + Duration::from_secs(6);
    let mut resumed_too_soon = false;
    let mut call_replayed = false;
    let mut cancellation_seen = false;
    let mut host_result = None;
    while Instant::now() < observe_until {
        let remaining = observe_until.saturating_duration_since(Instant::now());
        match fixture.next_request(remaining.min(Duration::from_millis(20))) {
            Ok(request) if request.method == "GET" => {
                resumed_too_soon = true;
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
                        let request_id = body["id"].clone();
                        request.respond_json(
                            200,
                            Vec::new(),
                            json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32603,"message":"duplicate call"}}),
                        );
                    }
                    other => panic!("unexpected upstream POST after failed GET: {other}"),
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

    let call_deadline = Instant::now() + Duration::from_secs(15);
    while host_result.is_none() {
        let remaining = call_deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "the absolute operation deadline should finish the call"
        );
        match fixture.next_request(remaining.min(Duration::from_millis(20))) {
            Ok(request) if request.method == "GET" => {
                resumed_too_soon = true;
                request.respond(405, None, Vec::new(), Vec::new());
            }
            Ok(request) if request.method == "POST" => {
                let body = request.json();
                assert_eq!(body["method"], "notifications/cancelled");
                assert_eq!(body["params"]["requestId"], upstream_id);
                cancellation_seen = true;
                request.respond(202, None, Vec::new(), Vec::new());
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
    let timeout_message = host_result.unwrap()["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    finish_gateway(&mut gateway, stdin, &fixture, "finite-retry-session");

    assert!(
        !resumed_too_soon,
        "a failed resumed GET must retain retry:60000"
    );
    assert!(!call_replayed, "the failed tool POST must not be replayed");
    assert!(
        cancellation_seen,
        "the timed out operation should be cancelled upstream"
    );
    assert!(timeout_message.contains("timed out"), "{timeout_message}");
    assert!(timeout_message.contains("unknown") || timeout_message.contains("outcome"));
}

#[test]
fn shared_event_stream_retry_delay_survives_a_failed_resumed_get() {
    let temporary = TestDirectory::new("mcp-gateway-remote-shared-retry");
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
    initialize_remote(&fixture, "shared-retry-session");

    let mut tools_listed = false;
    let mut initial_get_seen = false;
    let mut short_resume_seen = false;
    let initialization_deadline = Instant::now() + Duration::from_secs(10);
    while !tools_listed || !initial_get_seen || !short_resume_seen {
        let remaining = initialization_deadline.saturating_duration_since(Instant::now());
        let request = fixture
            .next_request(remaining)
            .expect("fixture should receive the list request and initial event stream");
        match request.method.as_str() {
            "GET" => match request.header("last-event-id") {
                None => {
                    assert!(!initial_get_seen);
                    initial_get_seen = true;
                    let event = sse_priming_event("shared-fast", 1).into_bytes();
                    request.respond_chunked(
                        200,
                        Some("text/event-stream"),
                        Vec::new(),
                        event.chunks(9).map(|chunk| chunk.to_vec()).collect(),
                    );
                }
                Some("shared-fast") => {
                    assert!(!short_resume_seen);
                    short_resume_seen = true;
                    let event = sse_priming_event("shared-slow", 60_000).into_bytes();
                    request.respond_chunked(
                        200,
                        Some("text/event-stream"),
                        Vec::new(),
                        event.chunks(11).map(|chunk| chunk.to_vec()).collect(),
                    );
                }
                Some(other) => panic!("unexpected initial event ID {other}"),
            },
            "POST" => {
                assert_eq!(request.json()["method"], "tools/list");
                respond_tools_list_request(request, "shared-retry-session");
                tools_listed = true;
            }
            method => panic!("unexpected upstream method {method}"),
        }
    }
    let listed = response_with_id(&responses, 2);
    assert_eq!(listed["result"]["tools"][0]["name"], "docs__echo");

    let retry_started = Instant::now();
    let failed_resume = next_resume_get(&fixture, "shared-slow", Duration::from_secs(65))
        .expect("shared SSE should resume after the 60000 ms retry delay");
    assert!(retry_started.elapsed() >= Duration::from_secs(59));
    failed_resume.respond(500, None, Vec::new(), Vec::new());

    let observe_until = Instant::now() + Duration::from_secs(6);
    let mut resumed_too_soon = false;
    match fixture.next_request(observe_until.saturating_duration_since(Instant::now())) {
        Ok(request) if request.method == "GET" => {
            resumed_too_soon = true;
            request.respond(405, None, Vec::new(), Vec::new());
        }
        Ok(request) => panic!("unexpected upstream request {}", request.method),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        Err(error) => panic!("remote fixture stopped unexpectedly: {error}"),
    }
    finish_gateway(&mut gateway, stdin, &fixture, "shared-retry-session");
    assert!(
        !resumed_too_soon,
        "the shared event stream must retain retry:60000 after a failed GET"
    );
}
