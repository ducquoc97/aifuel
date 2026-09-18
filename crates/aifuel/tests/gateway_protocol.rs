#[allow(dead_code)]
#[path = "support/gateway.rs"]
mod gateway_support;
#[allow(dead_code)]
mod support;

use gateway_support::*;
use serde_json::json;
use std::fs;
use std::time::{Duration, Instant};
use support::TestDirectory;

#[test]
fn gateway_preserves_upstream_tool_errors_and_json_rpc_errors() {
    for (fixture_flag, expected) in [
        (
            "MCP_FIXTURE_TOOL_ERROR",
            json!({"isError":true,"text":"fixture-error"}),
        ),
        (
            "MCP_FIXTURE_RPC_ERROR",
            json!({"code":-32042,"message":"upstream denied"}),
        ),
    ] {
        let temporary = TestDirectory::new("mcp-gateway-error-semantics");
        let server = compile_local_mcp_server(temporary.path());
        let config_root = temporary.path().join("config");
        write_catalog(
            &config_root,
            json!({
                "servers": {
                    "docs": {
                        "transport":"stdio",
                        "command":server,
                        "env":{(fixture_flag):{"value":"1"}}
                    }
                },
                "defaults":[],
                "agents":{"codex":{"servers":["docs"]}}
            }),
        );
        let (mut gateway, responses) = start_gateway(&config_root);
        let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
        initialize_host(&mut stdin, &responses);
        send_message(
            &mut stdin,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        );
        let _ = response_with_id(&responses, 2);
        send_message(
            &mut stdin,
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"docs__echo","arguments":{"message":"failure"}}}),
        );
        let response = response_with_id(&responses, 3);

        if fixture_flag == "MCP_FIXTURE_TOOL_ERROR" {
            assert_eq!(response["result"]["isError"], expected["isError"]);
            assert_eq!(response["result"]["content"][0]["text"], expected["text"]);
        } else {
            assert_eq!(response["error"]["code"], expected["code"]);
            assert_eq!(response["error"]["message"], expected["message"]);
        }
        drop(stdin);
        assert!(wait_for_exit(&mut gateway, Duration::from_secs(5)).success());
    }
}

#[test]
fn gateway_reports_missing_local_executables_without_returning_empty_tools() {
    let temporary = TestDirectory::new("mcp-gateway-missing-command");
    let config_root = temporary.path().join("config");
    write_catalog(
        &config_root,
        json!({
            "servers":{"docs":{"transport":"stdio","command":temporary.path().join("missing-server")}},
            "defaults":[],
            "agents":{"codex":{"servers":["docs"]}}
        }),
    );
    let (mut gateway, responses) = start_gateway(&config_root);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    let response = response_with_id(&responses, 2);

    assert_eq!(response["error"]["code"], -32603);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("could not start")
    );
    assert!(response["result"]["tools"].is_null());
    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(5)).success());
}

#[test]
fn gateway_excludes_tools_that_require_unsupported_task_execution() {
    let temporary = TestDirectory::new("mcp-gateway-required-task");
    let server = compile_local_mcp_server(temporary.path());
    let config_root = temporary.path().join("config");
    write_catalog(
        &config_root,
        json!({
            "servers": {
                "docs": {
                    "transport":"stdio",
                    "command":server,
                    "env":{"MCP_FIXTURE_TASK_REQUIRED":{"value":"1"}}
                }
            },
            "defaults":[],
            "agents":{"codex":{"servers":["docs"]}}
        }),
    );
    let (mut gateway, responses) = start_gateway(&config_root);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    let listed = response_with_id(&responses, 2);

    assert_eq!(listed["result"]["tools"], json!([]));
    assert!(listed["result"]["nextCursor"].is_null());
    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(5)).success());
}

#[test]
fn gateway_maps_progress_and_cancels_without_replaying_a_late_tool_result() {
    let temporary = TestDirectory::new("mcp-gateway-cancel");
    let server = compile_local_mcp_server(temporary.path());
    let config_root = temporary.path().join("config");
    let server_log = temporary.path().join("server-request.json");
    let cancellation_log = temporary.path().join("server-cancellation.json");
    let late_response_log = temporary.path().join("server-late-response.txt");
    write_catalog(
        &config_root,
        json!({
            "servers": {
                "docs": {
                    "transport": "stdio",
                    "command": server,
                    "env": {
                        "MCP_FIXTURE_LOG": {"value":server_log},
                        "MCP_FIXTURE_PROGRESS": {"value":"1"},
                        "MCP_FIXTURE_DELAY_MS": {"value":"600"},
                        "MCP_FIXTURE_CANCEL_LOG": {"value":cancellation_log},
                        "MCP_FIXTURE_LATE_RESPONSE_LOG": {"value":late_response_log}
                    }
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
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    let _ = response_with_id(&responses, 2);
    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{
                "name":"docs__echo",
                "arguments":{"message":"cancel me"},
                "_meta":{"progressToken":"host-progress"}
            }
        }),
    );

    let progress = next_message(&responses);
    assert_eq!(
        progress["method"],
        "notifications/progress",
        "first host message: {progress}; upstream request: {}",
        fs::read_to_string(&server_log).unwrap_or_else(|_| "<not recorded>".to_owned())
    );
    assert_eq!(progress["params"]["progressToken"], "host-progress");
    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "method":"notifications/cancelled",
            "params":{"requestId":3,"reason":"test cancellation"}
        }),
    );
    let cancelled = response_with_id_timeout(&responses, 3, Duration::from_secs(3));

    assert_eq!(cancelled["error"]["code"], -32800);
    let upstream_cancel = wait_for_file_content(
        &cancellation_log,
        "notifications/cancelled",
        Duration::from_secs(2),
    );
    assert!(upstream_cancel.contains("requestId"));
    send_message(&mut stdin, json!({"jsonrpc":"2.0","id":4,"method":"ping"}));
    assert_eq!(response_with_id(&responses, 4)["result"], json!({}));
    assert_eq!(
        wait_for_file_content(&late_response_log, "sent", Duration::from_secs(3)),
        "sent"
    );
    assert!(matches!(
        responses.recv_timeout(Duration::from_millis(200)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(5)).success());
}

#[test]
fn gateway_deadline_is_absolute_even_when_the_server_reports_progress() {
    let temporary = TestDirectory::new("mcp-gateway-deadline");
    let server = compile_local_mcp_server(temporary.path());
    let config_root = temporary.path().join("config");
    write_catalog(
        &config_root,
        json!({
            "servers": {
                "docs": {
                    "transport": "stdio",
                    "command": server,
                    "limits": {"operationSeconds":1},
                    "env": {
                        "MCP_FIXTURE_PROGRESS": {"value":"1"},
                        "MCP_FIXTURE_DELAY_MS": {"value":"2200"}
                    }
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
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    let _ = response_with_id(&responses, 2);
    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{
                "name":"docs__echo",
                "arguments":{"message":"finish after deadline"},
                "_meta":{"progressToken":"host-timeout-progress"}
            }
        }),
    );
    let progress = next_message(&responses);
    assert_eq!(progress["params"]["progressToken"], "host-timeout-progress");
    let timed_out = response_with_id_timeout(&responses, 3, Duration::from_secs(3));

    assert_eq!(timed_out["result"]["isError"], true);
    assert!(
        timed_out["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("timed out")
    );
    send_message(&mut stdin, json!({"jsonrpc":"2.0","id":4,"method":"ping"}));
    assert_eq!(response_with_id(&responses, 4)["result"], json!({}));
    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(5)).success());
}

#[test]
fn gateway_refreshes_tool_changes_before_notifying_and_suppresses_noop_updates() {
    let temporary = TestDirectory::new("mcp-gateway-tool-list-changes");
    let server = compile_local_mcp_server(temporary.path());
    let config_root = temporary.path().join("config");
    write_catalog(
        &config_root,
        json!({
            "servers": {
                "docs": {
                    "transport": "stdio",
                    "command": server,
                    "env": {"MCP_FIXTURE_CHANGE_TOOL_LIST": {"value":"true"}}
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
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    assert_eq!(
        response_with_id(&responses, 2)["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"docs__echo","arguments":{"message":"change tools"}}}),
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut call_result = None;
    let mut tools_changed = false;
    while call_result.is_none() || !tools_changed {
        let message = next_message_timeout(
            &responses,
            deadline.saturating_duration_since(Instant::now()),
        );
        if message["method"] == "notifications/tools/list_changed" {
            tools_changed = true;
        }
        if message["id"] == 3 {
            call_result = Some(message);
        }
    }
    assert_eq!(call_result.unwrap()["result"]["isError"], false);

    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":4,"method":"tools/list","params":{}}),
    );
    let updated = response_with_id(&responses, 4);
    assert_eq!(updated["result"]["tools"].as_array().unwrap().len(), 2);
    assert_eq!(updated["result"]["tools"][1]["name"], "docs__tool-1");

    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"docs__echo","arguments":{"message":"repeat update"}}}),
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let message = next_message_timeout(
            &responses,
            deadline.saturating_duration_since(Instant::now()),
        );
        assert_ne!(message["method"], "notifications/tools/list_changed");
        if message["id"] == 5 {
            assert_eq!(message["result"]["isError"], false);
            break;
        }
    }
    assert!(matches!(
        responses.recv_timeout(Duration::from_millis(200)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));

    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(5)).success());
}

#[test]
fn gateway_reports_missing_startup_environment_references_without_leaking_names() {
    let temporary = TestDirectory::new("mcp-gateway-missing-env");
    let config_root = temporary.path().join("config");
    write_catalog(
        &config_root,
        json!({
            "servers": {
                "docs": {
                    "transport": "stdio",
                    "command": temporary.path().join("must-not-start"),
                    "envFrom": {"MCP_FIXTURE_SOURCE":"AIFUEL_TEST_NOT_SET_MCP_GATEWAY_29"}
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
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    let failure = response_with_id(&responses, 2);

    let message = failure["error"]["message"].as_str().unwrap();
    assert!(message.contains("missing environment reference"));
    assert!(!message.contains("AIFUEL_TEST_NOT_SET_MCP_GATEWAY_29"));
    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(5)).success());
}
