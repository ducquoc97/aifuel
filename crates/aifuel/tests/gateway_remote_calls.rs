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
use serde_json::json;
use std::time::Duration;
use streamable_http::StreamableHttpFixture;
use support::TestDirectory;

#[test]
fn remote_gateway_runs_concurrent_tool_calls_and_correlates_responses() {
    let temporary = TestDirectory::new("mcp-gateway-remote-concurrency");
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
    initialize_remote(&fixture, "concurrent-session");
    respond_tools_list(&fixture, "concurrent-session");
    let listed = response_with_id(&responses, 2);
    assert_eq!(listed["result"]["tools"][0]["name"], "docs__echo");

    for (id, message) in [(3, "first"), (4, "second")] {
        send_message(
            &mut stdin,
            json!({
                "jsonrpc":"2.0",
                "id":id,
                "method":"tools/call",
                "params":{"name":"docs__echo","arguments":{"message":message}}
            }),
        );
    }
    let call_a = next_remote_post(&fixture, "tools/call", Duration::from_secs(5));
    let call_b = next_remote_post(&fixture, "tools/call", Duration::from_secs(5));
    assert_eq!(call_a.header("mcp-session-id"), Some("concurrent-session"));
    assert_eq!(call_b.header("mcp-session-id"), Some("concurrent-session"));
    let body_a = call_a.json();
    let body_b = call_b.json();
    let (first, first_body, second, second_body) =
        if body_a["params"]["arguments"]["message"] == "first" {
            (call_a, body_a, call_b, body_b)
        } else {
            (call_b, body_b, call_a, body_a)
        };
    let first_id = first_body["id"].clone();
    let second_id = second_body["id"].clone();
    assert_eq!(first_body["params"]["arguments"]["message"], "first");
    assert_eq!(second_body["params"]["arguments"]["message"], "second");

    second.respond_json(
        200,
        Vec::new(),
        json!({
            "jsonrpc":"2.0",
            "id":second_id,
            "result":{"content":[{"type":"text","text":"second"}],"isError":false}
        }),
    );
    first.respond_json(
        200,
        Vec::new(),
        json!({
            "jsonrpc":"2.0",
            "id":first_id,
            "result":{"content":[{"type":"text","text":"first"}],"isError":false}
        }),
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut received = Vec::new();
    let mut first_response = None;
    let mut second_response = None;
    while first_response.is_none() || second_response.is_none() {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let line = responses.recv_timeout(remaining).unwrap_or_else(|error| {
            panic!("gateway did not return both concurrent calls: {error}; received={received:?}")
        });
        let message: serde_json::Value = serde_json::from_str(&line).unwrap();
        received.push(message.clone());
        match message["id"].as_u64() {
            Some(3) => first_response = Some(message),
            Some(4) => second_response = Some(message),
            _ => {}
        }
    }
    let first_response = first_response.unwrap();
    let second_response = second_response.unwrap();
    assert_eq!(second_response["result"]["content"][0]["text"], "second");
    assert_eq!(first_response["result"]["content"][0]["text"], "first");

    finish_gateway(&mut gateway, stdin, &fixture, "concurrent-session");
}

#[test]
fn remote_gateway_cancels_inflight_call_and_discards_late_response() {
    let temporary = TestDirectory::new("mcp-gateway-remote-cancel");
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
    initialize_remote(&fixture, "cancel-session");
    respond_tools_list(&fixture, "cancel-session");
    let listed = response_with_id(&responses, 2);
    assert_eq!(listed["result"]["tools"][0]["name"], "docs__echo");

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{"name":"docs__echo","arguments":{"message":"cancel me"}}
        }),
    );
    let in_flight = next_remote_post(&fixture, "tools/call", Duration::from_secs(5));
    let upstream_id = in_flight.json()["id"].clone();
    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "method":"notifications/cancelled",
            "params":{"requestId":3,"reason":"test cancellation"}
        }),
    );
    let cancellation =
        next_remote_post(&fixture, "notifications/cancelled", Duration::from_secs(5));
    assert_eq!(cancellation.json()["params"]["requestId"], upstream_id);
    cancellation.respond(202, None, Vec::new(), Vec::new());

    let cancelled = response_with_id_timeout(&responses, 3, Duration::from_secs(5));
    assert_eq!(cancelled["error"]["code"], -32800);
    let late_id = upstream_id;
    in_flight.respond_json(
        200,
        Vec::new(),
        json!({
            "jsonrpc":"2.0",
            "id":late_id,
            "result":{"content":[{"type":"text","text":"late result"}],"isError":false}
        }),
    );
    send_message(&mut stdin, json!({"jsonrpc":"2.0","id":4,"method":"ping"}));
    assert_eq!(response_with_id(&responses, 4)["result"], json!({}));
    assert!(matches!(
        responses.recv_timeout(Duration::from_millis(200)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));

    finish_gateway(&mut gateway, stdin, &fixture, "cancel-session");
}
