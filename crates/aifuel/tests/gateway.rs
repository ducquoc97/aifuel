#[allow(dead_code)]
#[path = "support/gateway.rs"]
mod gateway_support;
#[allow(dead_code)]
mod support;

use gateway_support::*;
use serde_json::json;
use std::fs;
use std::process::{Command, Stdio};
use std::time::Duration;
use support::TestDirectory;

#[test]
fn local_gateway_lists_and_calls_a_selected_server_tool() {
    let temporary = TestDirectory::new("mcp-gateway");
    let server = compile_local_mcp_server(temporary.path());
    let server_log = temporary.path().join("server-request.json");
    let server_start = temporary.path().join("server-start.txt");
    let server_exit = temporary.path().join("server-exit");
    let config_root = temporary.path().join("config");
    write_catalog(
        &config_root,
        json!({
            "servers": {
                "docs": {
                    "transport": "stdio",
                    "command": server,
                    "args": ["literal argument"],
                    "env": {
                        "MCP_FIXTURE_LOG": {"value": server_log},
                        "MCP_FIXTURE_START_LOG": {"value": server_start},
                        "MCP_FIXTURE_EXIT": {"value": server_exit}
                    },
                    "envFrom": {"MCP_FIXTURE_SOURCE":"AIFUEL_TEST_SECRET_SOURCE"}
                },
                "not-selected": {
                    "transport": "stdio",
                    "command": temporary.path().join("must-not-start")
                }
            },
            "defaults": [],
            "agents": {"codex": {"servers": ["docs"]}}
        }),
    );

    let (mut gateway, responses) = start_gateway(&config_root);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");

    send_message(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "fixture-host", "version": "1"}
            }
        }),
    );
    let initialized = response_with_id(&responses, 1);
    assert_eq!(initialized["result"]["protocolVersion"], "2025-11-25");

    send_message(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
    );
    send_message(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    );
    let listed = response_with_id(&responses, 2);
    assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 1);
    let tool = &listed["result"]["tools"][0];
    assert_eq!(tool["name"], "docs__echo");
    assert_eq!(tool["description"], "Echoes request input");
    assert_eq!(tool["annotations"]["readOnlyHint"], false);
    assert_eq!(tool["execution"]["taskSupport"], "forbidden");
    let start_details = fs::read_to_string(&server_start)
        .expect("server startup should record its process context");
    let actual_cwd = start_details
        .lines()
        .find_map(|line| line.strip_prefix("cwd="))
        .expect("server startup should record its current directory");
    assert_eq!(
        fs::canonicalize(actual_cwd).expect("server current directory should resolve"),
        fs::canonicalize(&config_root).expect("configured home directory should resolve")
    );
    assert!(start_details.contains("args=[\"literal argument\"]"));
    assert!(start_details.contains("path_present=true"));
    assert!(start_details.contains("unlisted_secret_present=false"));
    assert!(start_details.contains("source_value=fixture-secret"));

    send_message(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "docs__echo",
                "arguments": {"message": "hello from the host"}
            }
        }),
    );
    let called = response_with_id(&responses, 3);
    assert_eq!(called["result"]["isError"], false);
    assert_eq!(called["result"]["content"][0]["text"], "fixture-result");
    let forwarded_call = fs::read_to_string(&server_log).expect("server should record the call");
    assert!(forwarded_call.contains("hello from the host"));

    drop(stdin);
    let status = wait_for_exit(&mut gateway, Duration::from_secs(10));
    assert!(status.success());
    assert!(
        server_exit.exists(),
        "gateway should close the owned server's stdin"
    );
}

#[test]
fn gateway_rejects_hosts_that_do_not_negotiate_the_approved_protocol() {
    let temporary = TestDirectory::new("mcp-gateway-host-version");
    let config_root = temporary.path().join("config");
    write_catalog(&config_root, json!({}));
    let (mut gateway, responses) = start_gateway(&config_root);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");

    send_message(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2026-07-28",
                "capabilities": {},
                "clientInfo": {"name": "unsupported-host", "version": "1"}
            }
        }),
    );
    let response = response_with_id(&responses, 1);

    assert_eq!(response["error"]["code"], -32602);
    assert!(response["result"].is_null());
    drop(stdin);
    assert_eq!(
        wait_for_exit(&mut gateway, Duration::from_secs(5)).code(),
        Some(2)
    );
}

#[test]
fn gateway_rejects_an_incompatible_local_server_instead_of_returning_empty_tools() {
    let temporary = TestDirectory::new("mcp-gateway-server-version");
    let server = compile_local_mcp_server(temporary.path());
    let config_root = temporary.path().join("config");
    write_catalog(
        &config_root,
        json!({
            "servers": {
                "docs": {
                    "transport": "stdio",
                    "command": server,
                    "env": {"MCP_FIXTURE_VERSION": {"value": "2026-07-28"}}
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
    let response = response_with_id(&responses, 2);

    assert!(response["error"].is_object());
    assert!(response["result"]["tools"].is_null());
    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(5)).success());
}

#[test]
fn gateway_reports_dangling_catalog_references_before_starting_stdio() {
    let temporary = TestDirectory::new("mcp-gateway-invalid-catalog");
    let config_root = temporary.path().join("config");
    write_catalog(
        &config_root,
        json!({"servers": {}, "defaults": ["missing"]}),
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_aifuel"));
    command
        .args(["mcp", "gateway", "--agent", "codex"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_user_config_root(&mut command, &config_root);
    let output = command.output().expect("gateway command should run");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown server id"));
}
