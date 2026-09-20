#[allow(dead_code)]
#[path = "support/gateway.rs"]
mod gateway_support;
#[allow(dead_code)]
#[path = "support/remote_gateway.rs"]
mod remote_gateway_support;
#[allow(dead_code)]
#[path = "support/streamable_http.rs"]
mod streamable_http;
#[allow(dead_code)]
mod support;

use gateway_support::*;
use remote_gateway_support::*;
use serde_json::json;
use std::fs;
use std::time::Duration;
use streamable_http::StreamableHttpFixture;
use support::TestDirectory;

#[test]
fn local_gateway_lists_reads_and_subscribes_to_server_scoped_resources() {
    let temporary = TestDirectory::new("mcp-gateway-resources-local");
    let server = compile_resource_mcp_server(temporary.path());
    let log = temporary.path().join("resource-requests.log");
    let config_root = temporary.path().join("config");
    write_catalog(
        &config_root,
        json!({
            "servers": {
                "docs": {
                    "transport": "stdio",
                    "command": server,
                    "env": {"MCP_RESOURCE_LOG": {"value": log}}
                }
            },
            "defaults": [],
            "agents": {"codex": {"servers": ["docs"]}}
        }),
    );
    let (mut gateway, responses) = start_gateway(&config_root);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);

    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"resources/list","params":{}}),
    );
    let listed = response_with_id(&responses, 2);
    assert_eq!(
        listed["result"]["resources"][0]["uri"],
        "aifuel-resource+646f6373+66696c65:///docs/guide.md"
    );
    assert_eq!(
        listed["result"]["resources"][1]["uri"],
        "aifuel-resource+646f6373+6874747073://[::1]/direct"
    );

    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":3,"method":"resources/templates/list","params":{}}),
    );
    let templates = response_with_id(&responses, 3);
    let template_uris = templates["result"]["resourceTemplates"]
        .as_array()
        .expect("resource templates should be an array")
        .iter()
        .map(|template| template["uriTemplate"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert!(template_uris.contains(&"aifuel-resource+646f6373+66696c65:///docs/{path}"));
    assert!(
        template_uris.contains(&"aifuel-resource+646f6373+637573746f6d2b736368656d65:///{+path}")
    );

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":4,
            "method":"resources/read",
            "params":{"uri":"aifuel-resource+646f6373+66696c65:///docs/guide.md"}
        }),
    );
    let read = response_with_id(&responses, 4);
    assert_eq!(read["result"]["contents"][0]["text"], "fixture resource");
    assert_eq!(
        read["result"]["contents"][0]["uri"],
        "aifuel-resource+646f6373+66696c65:///docs/guide.md"
    );

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":5,
            "method":"resources/subscribe",
            "params":{"uri":"aifuel-resource+646f6373+66696c65:///docs/guide.md"}
        }),
    );
    assert_eq!(response_with_id(&responses, 5)["result"], json!({}));
    let notification = next_message_timeout(&responses, Duration::from_secs(5));
    assert_eq!(notification["method"], "notifications/resources/updated");
    assert_eq!(
        notification["params"]["uri"],
        "aifuel-resource+646f6373+66696c65:///docs/guide.md"
    );

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":7,
            "method":"tools/call",
            "params":{"name":"docs__echo","arguments":{}}
        }),
    );
    let tool_result = response_with_id(&responses, 7);
    assert_eq!(
        tool_result["result"]["content"][0]["uri"],
        "https://example.com/direct"
    );
    assert_eq!(
        tool_result["result"]["content"][1]["uri"],
        "aifuel-resource+646f6373+66696c65:///private"
    );

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":6,
            "method":"resources/unsubscribe",
            "params":{"uri":"aifuel-resource+646f6373+66696c65:///docs/guide.md"}
        }),
    );
    assert_eq!(response_with_id(&responses, 6)["result"], json!({}));
    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(10)).success());

    let log = fs::read_to_string(log).expect("resource fixture should log requests");
    assert!(log.contains("read:file:///docs/guide.md"));
    assert!(log.contains("subscribe:file:///docs/guide.md"));
    assert!(log.contains("unsubscribe:file:///docs/guide.md"));
}

#[test]
fn remote_gateway_lists_and_reads_server_scoped_resources() {
    let temporary = TestDirectory::new("mcp-gateway-resources-remote");
    let fixture = StreamableHttpFixture::start();
    let config_root = temporary.path().join("config");
    write_remote_catalog(&config_root, &fixture.url(), json!({}));
    let (mut gateway, responses) = start_gateway(&config_root);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"resources/list","params":{}}),
    );

    initialize_remote(&fixture, "remote-resource-session");
    let list = next_remote_post(&fixture, "resources/list", Duration::from_secs(5));
    let list_id = list.json()["id"].clone();
    list.respond_json(
        200,
        Vec::new(),
        json!({
            "jsonrpc":"2.0",
            "id":list_id,
            "result":{"resources":[
                {"uri":"https://[::1]/guide","name":"guide","mimeType":"text/plain"}
            ]}
        }),
    );
    let templates = next_remote_post(&fixture, "resources/templates/list", Duration::from_secs(5));
    let templates_id = templates.json()["id"].clone();
    templates.respond_json(
        200,
        Vec::new(),
        json!({
            "jsonrpc":"2.0",
            "id":templates_id,
            "result":{"resourceTemplates":[
                {"uriTemplate":"custom+scheme:///{+path}","name":"guide-template"}
            ]}
        }),
    );
    let listed = response_with_id(&responses, 2);
    assert_eq!(
        listed["result"]["resources"][0]["uri"],
        "aifuel-resource+646f6373+6874747073://[::1]/guide"
    );

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"resources/read",
            "params":{"uri":"aifuel-resource+646f6373+6874747073://[::1]/guide"}
        }),
    );
    let read = next_remote_post(&fixture, "resources/read", Duration::from_secs(5));
    assert_eq!(read.json()["params"]["uri"], "https://[::1]/guide");
    let read_id = read.json()["id"].clone();
    read.respond_json(
        200,
        Vec::new(),
        json!({
            "jsonrpc":"2.0",
            "id":read_id,
            "result":{"contents":[{"uri":"https://[::1]/guide","text":"remote fixture"}]}
        }),
    );
    let read_response = response_with_id(&responses, 3);
    assert_eq!(
        read_response["result"]["contents"][0]["text"],
        "remote fixture"
    );
    assert_eq!(
        read_response["result"]["contents"][0]["uri"],
        "aifuel-resource+646f6373+6874747073://[::1]/guide"
    );

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":4,
            "method":"resources/subscribe",
            "params":{"uri":"aifuel-resource+646f6373+6874747073://[::1]/guide"}
        }),
    );
    let subscribe = next_remote_post(&fixture, "resources/subscribe", Duration::from_secs(5));
    let subscribe_id = subscribe.json()["id"].clone();
    subscribe.respond_json(
        200,
        Vec::new(),
        json!({"jsonrpc":"2.0","id":subscribe_id,"result":{}}),
    );
    assert_eq!(response_with_id(&responses, 4)["result"], json!({}));

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":5,
            "method":"resources/unsubscribe",
            "params":{"uri":"aifuel-resource+646f6373+6874747073://[::1]/guide"}
        }),
    );
    let unsubscribe = next_remote_post(&fixture, "resources/unsubscribe", Duration::from_secs(5));
    let unsubscribe_id = unsubscribe.json()["id"].clone();
    unsubscribe.respond_json(
        200,
        Vec::new(),
        json!({"jsonrpc":"2.0","id":unsubscribe_id,"result":{}}),
    );
    assert_eq!(response_with_id(&responses, 5)["result"], json!({}));

    finish_gateway(&mut gateway, stdin, &fixture, "remote-resource-session");
}
