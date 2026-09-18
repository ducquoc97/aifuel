#[allow(dead_code)]
#[path = "support/gateway.rs"]
mod gateway_support;
#[allow(dead_code)]
mod support;

use gateway_support::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::sync::Mutex;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use support::TestDirectory;

static GATEWAY_ROUTING_TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn gateway_routes_multiple_selected_servers_with_defaults_and_exact_overrides() {
    let _test_guard = GATEWAY_ROUTING_TEST_LOCK.lock().expect("test lock");
    let temporary = TestDirectory::new("mcp-gateway-routing");
    let server = compile_local_mcp_server(temporary.path());
    let config_root = temporary.path().join("config");
    let private_start = temporary.path().join("private-start.txt");
    write_catalog(
        &config_root,
        json!({
            "servers": {
                "docs": {
                    "transport": "stdio",
                    "command": server.clone(),
                    "env": {
                        "MCP_FIXTURE_TOOL_COUNT": {"value":"3"},
                        "MCP_FIXTURE_DESCRIPTION_BYTES": {"value":"300"},
                        "MCP_FIXTURE_RESULT": {"value":"docs-result"}
                    }
                },
                "search": {
                    "transport": "stdio",
                    "command": server.clone(),
                    "env": {
                        "MCP_FIXTURE_TOOL_COUNT": {"value":"3"},
                        "MCP_FIXTURE_DESCRIPTION_BYTES": {"value":"300"},
                        "MCP_FIXTURE_RESULT": {"value":"search-result"}
                    }
                },
                "private": {
                    "transport": "stdio",
                    "command": server,
                    "env": {"MCP_FIXTURE_START_LOG":{"value":private_start.clone()}}
                }
            },
            "defaults": ["docs", "search"],
            "agents": {
                "exact": {"servers":["search"]},
                "empty": {"servers":[]}
            },
            "gateway": {"maxMessageBytes":1024}
        }),
    );

    let (mut inherited, inherited_messages) = start_gateway_for_agent(&config_root, "inherited");
    let (mut exact, exact_messages) = start_gateway_for_agent(&config_root, "exact");
    let (mut empty, empty_messages) = start_gateway_for_agent(&config_root, "empty");
    let mut inherited_stdin = inherited.stdin.take().expect("inherited gateway stdin");
    let mut exact_stdin = exact.stdin.take().expect("exact gateway stdin");
    let mut empty_stdin = empty.stdin.take().expect("empty gateway stdin");
    initialize_host(&mut inherited_stdin, &inherited_messages);
    initialize_host(&mut exact_stdin, &exact_messages);
    initialize_host(&mut empty_stdin, &empty_messages);

    let inherited_tools = list_all_tools(&mut inherited_stdin, &inherited_messages, 2, 1024);
    let inherited_names = tool_names(&inherited_tools);
    assert_eq!(
        inherited_names,
        [
            "docs__echo",
            "docs__tool-1",
            "docs__tool-2",
            "search__echo",
            "search__tool-1",
            "search__tool-2"
        ]
    );

    let exact_tools = list_all_tools(&mut exact_stdin, &exact_messages, 2, 1024);
    assert_eq!(
        tool_names(&exact_tools),
        ["search__echo", "search__tool-1", "search__tool-2"]
    );
    send_message(
        &mut empty_stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    assert_eq!(
        response_with_id(&empty_messages, 2)["result"]["tools"],
        json!([])
    );

    send_tool_call(&mut inherited_stdin, 20, "docs__echo", "docs");
    let docs = response_with_id(&inherited_messages, 20);
    assert_eq!(docs["result"]["content"][0]["text"], "docs-result");
    send_tool_call(&mut inherited_stdin, 21, "search__echo", "search");
    let search = response_with_id(&inherited_messages, 21);
    assert_eq!(search["result"]["content"][0]["text"], "search-result");

    send_tool_call(&mut exact_stdin, 20, "docs__echo", "not selected");
    assert_eq!(
        response_with_id(&exact_messages, 20)["error"]["code"],
        -32601
    );
    send_tool_call(&mut inherited_stdin, 22, "private__echo", "not selected");
    assert_eq!(
        response_with_id(&inherited_messages, 22)["error"]["code"],
        -32601
    );
    assert!(
        !private_start.exists(),
        "an unselected server must not start"
    );

    drop(inherited_stdin);
    drop(exact_stdin);
    drop(empty_stdin);
    assert!(wait_for_exit(&mut inherited, Duration::from_secs(8)).success());
    assert!(wait_for_exit(&mut exact, Duration::from_secs(8)).success());
    assert!(wait_for_exit(&mut empty, Duration::from_secs(8)).success());
}

#[test]
fn gateway_keeps_known_tools_during_an_upstream_outage() {
    let _test_guard = GATEWAY_ROUTING_TEST_LOCK.lock().expect("test lock");
    let temporary = TestDirectory::new("mcp-gateway-outage");
    let server = compile_local_mcp_server(temporary.path());
    let config_root = temporary.path().join("config");
    let offline_marker = temporary.path().join("docs-offline");
    let exited_marker = temporary.path().join("docs-exited");
    write_catalog(
        &config_root,
        json!({
            "servers": {
                "docs": {
                    "transport": "stdio",
                    "command": server.clone(),
                    "env": {
                        "MCP_FIXTURE_CHANGE_TOOL_LIST": {"value":"1"},
                        "MCP_FIXTURE_EXIT_AFTER_CALL": {"value":"1"},
                        "MCP_FIXTURE_OFFLINE_MARKER": {"value":offline_marker.clone()},
                        "MCP_FIXTURE_EXIT_AFTER_CALL_MARKER": {"value":exited_marker.clone()},
                        "MCP_FIXTURE_RESULT": {"value":"docs-result"}
                    }
                },
                "search": {
                    "transport": "stdio",
                    "command": server,
                    "env": {"MCP_FIXTURE_RESULT":{"value":"search-result"}}
                }
            },
            "defaults": ["docs", "search"]
        }),
    );
    let (mut gateway, messages) = start_gateway_for_agent(&config_root, "inherited");
    let mut stdin = gateway.stdin.take().expect("gateway stdin");
    initialize_host(&mut stdin, &messages);
    let first_tools = list_all_tools(&mut stdin, &messages, 2, 8 * 1024 * 1024);
    assert_eq!(tool_names(&first_tools), ["docs__echo", "search__echo"]);

    send_tool_call(&mut stdin, 3, "docs__echo", "cause outage");
    assert_eq!(
        response_with_id(&messages, 3)["result"]["content"][0]["text"],
        "docs-result"
    );
    wait_for_file_content(&exited_marker, "exited", Duration::from_secs(2));
    wait_for_file_content(&offline_marker, "offline", Duration::from_secs(2));

    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":4,"method":"tools/list","params":{}}),
    );
    let stale_tools = response_with_id(&messages, 4);
    assert!(
        stale_tools["error"].is_null(),
        "known tools should remain listed"
    );
    assert_eq!(
        tool_names(
            stale_tools["result"]["tools"]
                .as_array()
                .expect("tools/list result")
        ),
        ["docs__echo", "search__echo"]
    );

    send_tool_call(&mut stdin, 5, "docs__echo", "no replay");
    let unavailable = response_with_id(&messages, 5);
    assert_eq!(unavailable["result"]["isError"], true);
    assert!(
        unavailable["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unavailable")
    );
    send_tool_call(&mut stdin, 6, "search__echo", "healthy server");
    assert_eq!(
        response_with_id(&messages, 6)["result"]["content"][0]["text"],
        "search-result"
    );

    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(8)).success());
}

#[test]
fn separate_gateways_keep_upstream_processes_and_cursors_isolated() {
    let _test_guard = GATEWAY_ROUTING_TEST_LOCK.lock().expect("test lock");
    let temporary = TestDirectory::new("mcp-gateway-session-isolation");
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
                        "MCP_FIXTURE_RESPONSE_ID": {"value":"1"},
                        "MCP_FIXTURE_TOOL_COUNT": {"value":"4"},
                        "MCP_FIXTURE_DESCRIPTION_BYTES": {"value":"350"}
                    }
                }
            },
            "defaults": ["docs"],
            "gateway": {"maxMessageBytes":1024}
        }),
    );
    let (mut codex, codex_messages) = start_gateway_for_agent(&config_root, "codex");
    let (mut claude, claude_messages) = start_gateway_for_agent(&config_root, "claude");
    let mut codex_stdin = codex.stdin.take().expect("Codex gateway stdin");
    let mut claude_stdin = claude.stdin.take().expect("Claude gateway stdin");
    initialize_host(&mut codex_stdin, &codex_messages);
    initialize_host(&mut claude_stdin, &claude_messages);

    send_message(
        &mut codex_stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    send_message(
        &mut claude_stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    let codex_page = response_with_id(&codex_messages, 2);
    let claude_page = response_with_id(&claude_messages, 2);
    assert!(serde_json::to_vec(&codex_page).unwrap().len() <= 1024);
    assert!(serde_json::to_vec(&claude_page).unwrap().len() <= 1024);
    let codex_cursor = codex_page["result"]["nextCursor"]
        .as_str()
        .expect("first Codex page should continue");
    let claude_cursor = claude_page["result"]["nextCursor"]
        .as_str()
        .expect("first Claude page should continue");
    assert_ne!(codex_cursor, claude_cursor);

    send_message(
        &mut claude_stdin,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":{"cursor":codex_cursor}}),
    );
    assert_eq!(
        response_with_id(&claude_messages, 3)["error"]["code"],
        -32602
    );

    send_tool_call(&mut codex_stdin, 4, "docs__echo", "codex process");
    send_tool_call(&mut claude_stdin, 4, "docs__echo", "claude process");
    let codex_pid = response_with_id(&codex_messages, 4)["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    let claude_pid = response_with_id(&claude_messages, 4)["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(codex_pid, claude_pid);

    drop(codex_stdin);
    drop(claude_stdin);
    assert!(wait_for_exit(&mut codex, Duration::from_secs(8)).success());
    assert!(wait_for_exit(&mut claude, Duration::from_secs(8)).success());
}

fn list_all_tools(
    stdin: &mut impl Write,
    messages: &Receiver<String>,
    first_id: u64,
    max_message_bytes: usize,
) -> Vec<Value> {
    let mut cursor = None;
    let mut tools = Vec::new();
    for page_index in 0..20 {
        let params = cursor
            .as_ref()
            .map(|cursor| json!({"cursor":cursor}))
            .unwrap_or_else(|| json!({}));
        let id = first_id + page_index;
        send_message(
            stdin,
            json!({"jsonrpc":"2.0","id":id,"method":"tools/list","params":params}),
        );
        let page = response_with_id(messages, id);
        assert!(page["error"].is_null(), "tools/list failed: {page}");
        assert!(serde_json::to_vec(&page).unwrap().len() <= max_message_bytes);
        tools.extend(
            page["result"]["tools"]
                .as_array()
                .expect("tools/list result")
                .iter()
                .cloned(),
        );
        cursor = page["result"]["nextCursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            return tools;
        }
    }
    panic!("gateway tool listing should finish within twenty pages");
}

fn tool_names(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name").to_owned())
        .collect()
}

fn send_tool_call(stdin: &mut impl Write, id: u64, name: &str, message: &str) {
    send_message(
        stdin,
        json!({
            "jsonrpc":"2.0",
            "id":id,
            "method":"tools/call",
            "params":{"name":name,"arguments":{"message":message}}
        }),
    );
}

#[test]
fn concurrent_server_calls_keep_progress_and_cancellation_scoped() {
    let _test_guard = GATEWAY_ROUTING_TEST_LOCK.lock().expect("test lock");
    let temporary = TestDirectory::new("mcp-gateway-concurrency");
    let server = compile_local_mcp_server(temporary.path());
    let config_root = temporary.path().join("config");
    let cancellation_log = temporary.path().join("docs-cancellation.json");
    let late_response_log = temporary.path().join("docs-late-response.txt");
    write_catalog(
        &config_root,
        json!({
            "servers": {
                "docs": {
                    "transport":"stdio",
                    "command":server.clone(),
                    "env": {
                        "MCP_FIXTURE_PROGRESS":{"value":"1"},
                        "MCP_FIXTURE_DELAY_MS":{"value":"2500"},
                        "MCP_FIXTURE_CANCEL_LOG":{"value":cancellation_log.clone()},
                        "MCP_FIXTURE_LATE_RESPONSE_LOG":{"value":late_response_log.clone()}
                    }
                },
                "search": {
                    "transport":"stdio",
                    "command":server,
                    "env": {
                        "MCP_FIXTURE_PROGRESS":{"value":"1"},
                        "MCP_FIXTURE_RESULT":{"value":"search-result"}
                    }
                }
            },
            "defaults":["docs","search"]
        }),
    );
    let (mut gateway, messages) = start_gateway_for_agent(&config_root, "inherited");
    let mut stdin = gateway.stdin.take().expect("gateway stdin");
    initialize_host(&mut stdin, &messages);
    let listed = list_all_tools(&mut stdin, &messages, 2, 8 * 1024 * 1024);
    assert_eq!(tool_names(&listed), ["docs__echo", "search__echo"]);

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{"name":"docs__echo","arguments":{"message":"cancel"},"_meta":{"progressToken":"docs-progress"}}
        }),
    );
    let docs_progress = next_message_timeout(&messages, Duration::from_secs(8));
    assert_eq!(docs_progress["method"], "notifications/progress");
    assert_eq!(docs_progress["params"]["progressToken"], "docs-progress");

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":4,
            "method":"tools/call",
            "params":{"name":"search__echo","arguments":{"message":"healthy"},"_meta":{"progressToken":"search-progress"}}
        }),
    );
    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "method":"notifications/cancelled",
            "params":{"requestId":3,"reason":"cancel only docs"}
        }),
    );

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut results = HashMap::new();
    while results.len() < 2 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let response = next_message_timeout(&messages, remaining);
        if response["method"] == "notifications/progress"
            && let Some(token) = response["params"]["progressToken"].as_str()
        {
            assert_eq!(token, "search-progress");
        }
        if let Some(id) = response["id"].as_u64()
            && matches!(id, 3 | 4)
        {
            results.insert(id, response);
        }
    }
    assert_eq!(results[&3]["error"]["code"], -32800);
    assert_eq!(results[&4]["result"]["content"][0]["text"], "search-result");
    wait_for_file_content(
        &cancellation_log,
        "notifications/cancelled",
        Duration::from_secs(8),
    );
    wait_for_file_content(&late_response_log, "sent", Duration::from_secs(8));
    while let Ok(line) = messages.recv_timeout(Duration::from_millis(200)) {
        let message: Value =
            serde_json::from_str(&line).expect("gateway should emit JSON-RPC messages");
        assert!(
            !matches!(message["id"].as_u64(), Some(3 | 4)),
            "cancelled or completed tool calls must not be replayed: {message}"
        );
    }

    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(8)).success());
}
