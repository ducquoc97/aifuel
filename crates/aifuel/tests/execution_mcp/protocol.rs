use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

use crate::support::TestDirectory;
use crate::support_protocol::call_tools;

#[test]
fn execution_endpoint_is_separate_and_cannot_grant_permissions() {
    let directory = TestDirectory::new("execution-protocol");
    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args(["mcp", "execution"])
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            let mut input = child.stdin.take().expect("MCP stdin should be piped");
            for request in [
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
                json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            ] {
                writeln!(input, "{request}").expect("request should be written");
            }
            drop(input);
            child.wait_with_output()
        })
        .expect("execution MCP server should start");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .expect("stdout should be UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("each frame should be JSON"))
        .collect();
    assert_eq!(
        responses[0]["result"]["serverInfo"]["name"],
        "aifuel-execution"
    );
    let tools = responses[1]["result"]["tools"]
        .as_array()
        .expect("tool list should be an array");
    for operation in [
        "list_agents",
        "list_models",
        "resolve_run",
        "start_run",
        "get_run",
        "read_events",
        "get_result",
        "cancel_run",
        "resume_session",
        "answer_input",
    ] {
        assert!(
            tools.iter().any(|tool| tool["name"] == operation),
            "missing {operation}"
        );
    }
    let list_agents = tools
        .iter()
        .find(|tool| tool["name"] == "list_agents")
        .expect("list_agents should be advertised");
    assert_eq!(
        list_agents["inputSchema"]["properties"]["provider"]["type"],
        "string"
    );
    let list_models = tools
        .iter()
        .find(|tool| tool["name"] == "list_models")
        .expect("list_models should be advertised");
    assert_eq!(
        list_models["inputSchema"]["properties"]["refresh"]["type"],
        "boolean"
    );
    assert!(
        !tools
            .iter()
            .find(|tool| tool["name"] == "resolve_run")
            .expect("resolve_run should be advertised")["inputSchema"]["required"]
            .as_array()
            .expect("required should be an array")
            .iter()
            .any(|field| field == "provider")
    );
    assert!(
        !tools
            .iter()
            .any(|tool| tool["name"].as_str().unwrap().contains("approv"))
    );
    assert!(!tools.iter().any(|tool| tool["name"] == "get_status"));
}

#[test]
fn list_agents_returns_provider_owned_presence_version_and_independent_capabilities() {
    let directory = TestDirectory::new("execution-agents");
    let all = call_tools(
        &directory,
        &[json!({"name":"list_agents","arguments":{}})],
        Some(directory.path()),
    );
    let agents = all[0]["result"]["structuredContent"]["agents"]
        .as_array()
        .expect("agents should be an array");
    assert!(!agents.is_empty());
    let codex = agents
        .iter()
        .find(|agent| agent["provider"] == "codex")
        .expect("the compiled Codex integration should be listed");
    assert_eq!(codex["integration"], "compiled");
    assert!(codex["native_presence"]["state"].is_string());
    assert!(
        codex["native_presence"]["reason"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
    assert!(
        codex["native_version"]["reason"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
    assert!(codex["native_version"].get("version").is_some());
    for capability in [
        "model_catalog",
        "streaming",
        "read_only",
        "workspace_write",
        "external_mcp_tools",
        "resume",
        "effort",
        "account_selection",
        "ordinary_input",
        "permission_approval",
    ] {
        let assessment = &codex["capabilities"][capability];
        assert!(assessment["declared"]["state"].is_string(), "{capability}");
        assert!(assessment["current"]["state"].is_string(), "{capability}");
        assert!(assessment["declared"]["reason"].is_string(), "{capability}");
        assert!(assessment["current"]["reason"].is_string(), "{capability}");
    }

    let filtered = call_tools(
        &directory,
        &[json!({"name":"list_agents","arguments":{"provider":"codex"}})],
        Some(directory.path()),
    );
    let filtered_agents = filtered[0]["result"]["structuredContent"]["agents"]
        .as_array()
        .expect("filtered agents should be an array");
    assert_eq!(filtered_agents.len(), 1);
    assert_eq!(filtered_agents[0]["provider"], "codex");
}
