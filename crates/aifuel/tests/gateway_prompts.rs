#[allow(dead_code)]
#[path = "support/gateway.rs"]
mod gateway_support;
#[allow(dead_code)]
mod support;

use gateway_support::*;
use serde_json::json;
use std::time::Duration;
use support::TestDirectory;

#[test]
fn local_gateway_lists_gets_and_completes_namespaced_prompts() {
    let temporary = TestDirectory::new("mcp-gateway-prompts-local");
    let server = compile_prompt_mcp_server(temporary.path());
    let config_root = temporary.path().join("config");
    let request_log = temporary.path().join("prompt-request.json");
    let completion_log = temporary.path().join("prompt-completion.json");
    write_catalog(
        &config_root,
        json!({
            "servers": {"docs": {
                "transport":"stdio",
                "command":server,
                "env": {
                    "MCP_PROMPT_REQUEST_LOG":{"value":request_log},
                    "MCP_PROMPT_COMPLETION_LOG":{"value":completion_log}
                }
            }},
            "defaults":["docs"]
        }),
    );
    let (mut gateway, responses) = start_gateway(&config_root);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);

    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"prompts/list","params":{}}),
    );
    let listed = response_with_id(&responses, 2);
    assert_eq!(listed["result"]["prompts"][0]["name"], "docs__summarize");
    assert_eq!(
        listed["result"]["prompts"][0]["arguments"][0]["name"],
        "topic"
    );

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"prompts/get",
            "params":{"name":"docs__summarize","arguments":{"topic":"rust"}}
        }),
    );
    let prompt = response_with_id(&responses, 3);
    assert_eq!(prompt["result"]["description"], "fixture prompt");
    assert_eq!(prompt["result"]["messages"][0]["role"], "user");
    assert_eq!(prompt["result"]["messages"][1]["content"]["type"], "image");
    assert!(
        prompt["result"]["messages"][2]["content"]["resource"]["uri"]
            .as_str()
            .is_some_and(|uri| uri.starts_with("aifuel-resource+646f6373+"))
    );
    assert_eq!(
        prompt["result"]["messages"][3]["content"]["uri"],
        "https://example.test/direct"
    );
    let prompt_request = wait_for_file_content(&request_log, "prompts/get", Duration::from_secs(2));
    assert!(prompt_request.contains("topic"));

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":4,
            "method":"completion/complete",
            "params":{
                "ref":{"type":"ref/prompt","name":"docs__summarize"},
                "argument":{"name":"topic","value":"ru"}
            }
        }),
    );
    let completion = response_with_id(&responses, 4);
    assert_eq!(
        completion["result"]["completion"]["values"],
        json!(["alpha", "beta"])
    );

    let resource_uri = "aifuel-resource+646f6373+66696c65:///guide.md";
    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":5,
            "method":"completion/complete",
            "params":{
                "ref":{"type":"ref/resource","uri":resource_uri},
                "argument":{"name":"path","value":"gu"}
            }
        }),
    );
    assert_eq!(
        response_with_id(&responses, 5)["result"]["completion"]["values"],
        json!(["alpha", "beta"])
    );
    let logged = wait_for_file_content(
        &completion_log,
        "completion/complete",
        Duration::from_secs(2),
    );
    assert!(logged.contains("file:///guide.md"));

    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(8)).success());
}

#[test]
fn prompt_list_updates_are_forwarded_and_known_prompts_are_retained_on_outage() {
    let temporary = TestDirectory::new("mcp-gateway-prompts-updates");
    let server = compile_prompt_mcp_server(temporary.path());
    let config_root = temporary.path().join("config");
    write_catalog(
        &config_root,
        json!({
            "servers": {"docs": {
                "transport":"stdio",
                "command":server,
                "env": {"MCP_PROMPT_CHANGE_LIST":{"value":"1"}}
            }},
            "defaults":["docs"]
        }),
    );
    let (mut gateway, responses) = start_gateway(&config_root);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"prompts/list","params":{}}),
    );
    assert_eq!(
        response_with_id(&responses, 2)["result"]["prompts"][0]["name"],
        "docs__summarize"
    );
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":3,"method":"prompts/get","params":{"name":"docs__summarize"}}),
    );
    let response = response_with_id(&responses, 3);
    assert!(response["result"].is_object());
    let changed = next_message_timeout(&responses, Duration::from_secs(4));
    assert_eq!(changed["method"], "notifications/prompts/list_changed");
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":4,"method":"prompts/list","params":{}}),
    );
    let updated = response_with_id(&responses, 4);
    assert_eq!(
        updated["result"]["prompts"][0]["name"],
        "docs__summarize-updated"
    );

    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(8)).success());
}

#[test]
fn completion_is_explicitly_rejected_when_upstream_does_not_advertise_it() {
    let temporary = TestDirectory::new("mcp-gateway-prompts-unsupported");
    let server = compile_local_mcp_server(temporary.path());
    let config_root = temporary.path().join("config");
    write_catalog(
        &config_root,
        json!({"servers":{"docs":{"transport":"stdio","command":server}},"defaults":["docs"]}),
    );
    let (mut gateway, responses) = start_gateway(&config_root);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);
    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"completion/complete",
            "params":{
                "ref":{"type":"ref/resource","uri":"aifuel-resource+646f6373+66696c65:///guide.md"},
                "argument":{"name":"path","value":""}
            }
        }),
    );
    let response = response_with_id(&responses, 2);
    assert_eq!(response["error"]["code"], -32601);
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("does not support"))
    );
    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(8)).success());
}
