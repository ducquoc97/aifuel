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
use std::io::Read;
use std::time::{Duration, Instant};
use streamable_http::StreamableHttpFixture;
use support::TestDirectory;

#[test]
fn remote_gateway_negotiates_2025_and_calls_a_selected_server_tool() {
    let temporary = TestDirectory::new("mcp-gateway-remote");
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

    initialize_remote(&fixture, "remote-session");
    respond_tools_list(&fixture, "remote-session");
    let listed = response_with_id(&responses, 2);
    assert_eq!(listed["result"]["tools"][0]["name"], "docs__echo");
    assert_eq!(
        listed["result"]["tools"][0]["annotations"]["readOnlyHint"],
        false
    );

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{"name":"docs__echo","arguments":{"message":"hello remote"}}
        }),
    );
    let call = next_remote_post(&fixture, "tools/call", Duration::from_secs(5));
    assert_eq!(call.header("mcp-session-id"), Some("remote-session"));
    assert_eq!(call.header("mcp-protocol-version"), Some("2025-11-25"));
    let call_body = call.json();
    assert_eq!(call_body["params"]["arguments"]["message"], "hello remote");
    let call_id = call_body["id"].clone();
    call.respond_json(
        200,
        Vec::new(),
        json!({
            "jsonrpc":"2.0",
            "id":call_id,
            "result":{"content":[{"type":"text","text":"remote-fixture-result"}],"isError":false}
        }),
    );
    let called = response_with_id(&responses, 3);
    assert_eq!(called["result"]["isError"], false);
    assert_eq!(
        called["result"]["content"][0]["text"],
        "remote-fixture-result"
    );

    finish_gateway(&mut gateway, stdin, &fixture, "remote-session");
}

#[test]
fn remote_sse_response_resumes_with_get_and_deduplicates_redelivery() {
    let temporary = TestDirectory::new("mcp-gateway-remote-sse");
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
    initialize_remote(&fixture, "remote-sse-session");
    respond_tools_list(&fixture, "remote-sse-session");
    let listed = response_with_id(&responses, 2);
    assert_eq!(listed["result"]["tools"][0]["name"], "docs__echo");

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{
                "name":"docs__echo",
                "arguments":{"message":"resume me"},
                "_meta":{"progressToken":"host-progress"}
            }
        }),
    );
    let call = next_remote_post(&fixture, "tools/call", Duration::from_secs(5));
    let call_body = call.json();
    let upstream_id = call_body["id"].clone();
    let progress_token = call_body["params"]["_meta"]["progressToken"].clone();
    assert!(
        !progress_token.is_null(),
        "gateway should forward a progress token"
    );
    let progress = json!({
        "jsonrpc":"2.0",
        "method":"notifications/progress",
        "params":{"progressToken":progress_token,"progress":1,"total":2}
    });
    call.respond(
        200,
        Some("text/event-stream"),
        Vec::new(),
        sse_event("event-1", Some(1), &progress),
    );

    let resumed = match next_resume_get(&fixture, "event-1", Duration::from_secs(5)) {
        Ok(request) => request,
        Err(error) => {
            let host_messages: Vec<_> = responses.try_iter().collect();
            drop(stdin);
            let exit_deadline = Instant::now() + Duration::from_secs(5);
            let mut exited = false;
            while Instant::now() < exit_deadline {
                if gateway
                    .try_wait()
                    .expect("gateway status should be readable")
                    .is_some()
                {
                    exited = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if !exited {
                let _ = gateway.kill();
                let _ = gateway.wait();
            }
            let mut stderr = String::new();
            if let Some(mut pipe) = gateway.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            panic!("missing resumed GET: {error}; host={host_messages:?}; stderr={stderr}");
        }
    };
    assert_eq!(resumed.header("mcp-session-id"), Some("remote-sse-session"));
    assert_eq!(resumed.header("mcp-protocol-version"), Some("2025-11-25"));
    let tool_result = json!({
        "jsonrpc":"2.0",
        "id":upstream_id,
        "result":{"content":[{"type":"text","text":"resumed-result"}],"isError":false}
    });
    resumed.respond(
        200,
        Some("text/event-stream"),
        Vec::new(),
        format!(
            "{}{}",
            sse_event("event-1", None, &progress),
            sse_event("event-2", None, &tool_result)
        ),
    );

    let host_deadline = Instant::now() + Duration::from_secs(5);
    let mut progress_count = 0;
    let mut call_response = None;
    let mut host_messages = Vec::new();
    while call_response.is_none() {
        let remaining = host_deadline.saturating_duration_since(Instant::now());
        let line = match responses.recv_timeout(remaining) {
            Ok(line) => line,
            Err(error) => {
                drop(stdin);
                let _ = gateway.kill();
                let _ = gateway.wait();
                let mut stderr = String::new();
                if let Some(mut pipe) = gateway.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                panic!(
                    "host did not receive the remote result: {error}; messages={host_messages:?}; stderr={stderr}"
                );
            }
        };
        let message: Value = serde_json::from_str(&line).expect("gateway output should be JSON");
        host_messages.push(message.clone());
        if message["method"] == "notifications/progress" {
            progress_count += 1;
            assert_eq!(message["params"]["progressToken"], "host-progress");
        }
        if message["id"] == 3 {
            call_response = Some(message);
        }
    }
    assert_eq!(
        progress_count, 1,
        "redelivered event ID should not forward twice"
    );
    assert_eq!(
        call_response.unwrap()["result"]["content"][0]["text"],
        "resumed-result"
    );

    finish_gateway(&mut gateway, stdin, &fixture, "remote-sse-session");
}
