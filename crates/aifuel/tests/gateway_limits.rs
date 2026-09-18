#[allow(dead_code)]
#[path = "support/gateway.rs"]
mod gateway_support;
#[allow(dead_code)]
mod support;

use gateway_support::*;
use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::time::Duration;
use support::TestDirectory;

#[test]
fn gateway_paginates_after_name_rewriting_with_the_complete_response_envelope() {
    let temporary = TestDirectory::new("mcp-gateway-pagination");
    let server = compile_local_mcp_server(temporary.path());
    let config_root = temporary.path().join("config");
    write_catalog(
        &config_root,
        json!({
            "servers": {
                "docs": {
                    "transport": "stdio",
                    "command": server,
                    "env": {
                        "MCP_FIXTURE_TOOL_COUNT": {"value":"4"},
                        "MCP_FIXTURE_DESCRIPTION_BYTES": {"value":"350"}
                    }
                }
            },
            "defaults": [],
            "agents": {"codex": {"servers": ["docs"]}},
            "gateway": {"maxMessageBytes": 1024}
        }),
    );
    let (mut gateway, responses) = start_gateway(&config_root);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);

    let mut cursor = None;
    let mut tools = Vec::new();
    for id in 2..8 {
        let params = cursor
            .as_ref()
            .map(|cursor| json!({"cursor":cursor}))
            .unwrap_or_else(|| json!({}));
        send_message(
            &mut stdin,
            json!({"jsonrpc":"2.0","id":id,"method":"tools/list","params":params}),
        );
        let page = response_with_id(&responses, id);
        assert!(
            serde_json::to_vec(&page).unwrap().len() <= 1024,
            "serialized host response must fit the configured byte limit"
        );
        tools.extend(page["result"]["tools"].as_array().unwrap().iter().cloned());
        cursor = page["result"]["nextCursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }

    assert_eq!(tools.len(), 4);
    assert_eq!(tools[0]["name"], "docs__echo");
    assert_eq!(tools[1]["name"], "docs__tool-1");
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":20,"method":"tools/list","params":{"cursor":"stale"}}),
    );
    assert_eq!(response_with_id(&responses, 20)["error"]["code"], -32602);
    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(5)).success());
}

#[test]
fn gateway_reports_a_single_tool_that_cannot_fit_in_a_host_message() {
    let temporary = TestDirectory::new("mcp-gateway-oversized-tool");
    let server = compile_local_mcp_server(temporary.path());
    let config_root = temporary.path().join("config");
    write_catalog(
        &config_root,
        json!({
            "servers": {
                "docs": {
                    "transport": "stdio",
                    "command": server,
                    "env": {"MCP_FIXTURE_DESCRIPTION_BYTES": {"value":"900"}}
                }
            },
            "defaults": [],
            "agents": {"codex": {"servers": ["docs"]}},
            "gateway": {"maxMessageBytes": 512}
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

    assert!(response["error"].is_object());
    assert!(response["result"]["tools"].is_null());
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("exceeds the host message byte limit")
    );
    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(5)).success());
}

#[test]
fn oversized_tool_result_returns_a_bounded_error_without_closing_the_host_session() {
    let temporary = TestDirectory::new("mcp-gateway-oversized-result");
    let server = compile_local_mcp_server(temporary.path());
    let config_root = temporary.path().join("config");
    write_catalog(
        &config_root,
        json!({
            "servers": {
                "docs": {
                    "transport": "stdio",
                    "command": server,
                    "env": {"MCP_FIXTURE_RESULT_BYTES": {"value":"2000000"}}
                }
            },
            "defaults": [],
            "agents": {"codex": {"servers": ["docs"]}},
            "gateway": {"maxMessageBytes": 512}
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
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"docs__echo","arguments":{"message":"large result"}}}),
    );
    let result = response_with_id(&responses, 3);

    assert_eq!(result["result"]["isError"], true);
    assert!(
        result["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("exceeds the host message byte limit")
    );
    send_message(&mut stdin, json!({"jsonrpc":"2.0","id":4,"method":"ping"}));
    assert_eq!(response_with_id(&responses, 4)["result"], json!({}));
    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(5)).success());
}

#[test]
fn stalled_host_stdout_terminates_the_gateway_and_its_local_process_tree() {
    let temporary = TestDirectory::new("mcp-gateway-stalled-host");
    let server = compile_local_mcp_server(temporary.path());
    let server_exit = temporary.path().join("server-exit");
    let child_started = temporary.path().join("child-started");
    let grandchild_started = temporary.path().join("grandchild-started");
    let late_child = temporary.path().join("late-child");
    let server_log = temporary.path().join("server-request.json");
    let config_root = temporary.path().join("config");
    write_catalog(
        &config_root,
        json!({
            "servers": {
                "docs": {
                    "transport": "stdio",
                    "command": server,
                    "limits": {"shutdownSeconds":1},
                    "env": {
                        "MCP_FIXTURE_LOG": {"value":server_log},
                        "MCP_FIXTURE_RESULT_BYTES": {"value":"2000000"},
                        "MCP_FIXTURE_EXIT": {"value":server_exit},
                        "MCP_FIXTURE_CHILD_STARTED": {"value":child_started},
                        "MCP_FIXTURE_GRANDCHILD_STARTED": {"value":grandchild_started},
                        "MCP_FIXTURE_LATE_MARKER": {"value":late_child}
                    }
                }
            },
            "defaults": [],
            "agents": {"codex": {"servers": ["docs"]}},
            "gateway": {
                "maxMessageBytes": 4194304,
                "maxOutputBufferBytes": 8388608,
                "outputStallSeconds": 1
            }
        }),
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_aifuel"));
    command
        .args(["mcp", "gateway", "--agent", "codex"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    configure_user_config_root(&mut command, &config_root);
    let mut gateway = command.spawn().expect("gateway command should start");
    let mut stdout = BufReader::new(
        gateway
            .stdout
            .take()
            .expect("gateway stdout should be piped"),
    );
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    send_message(&mut stdin, initialize_request(1));
    assert_eq!(
        read_response(&mut stdout, 1)["result"]["protocolVersion"],
        "2025-11-25"
    );
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    );
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    assert_eq!(
        read_response(&mut stdout, 2)["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(child_started.exists());
    wait_for_file_content(&grandchild_started, "started", Duration::from_secs(2));
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"docs__echo","arguments":{"message":"large"}}}),
    );
    let recorded_call =
        wait_for_file_content(&server_log, "resultBytes=2000000", Duration::from_secs(2));
    assert!(recorded_call.contains("resultBytes=2000000"));
    let status = wait_for_exit(&mut gateway, Duration::from_secs(8));
    drop(stdout);
    drop(stdin);
    std::thread::sleep(Duration::from_secs(4));

    assert!(!status.success());
    assert!(server_exit.exists());
    assert!(
        !late_child.exists(),
        "stalled host cleanup should stop the server process tree"
    );
}
