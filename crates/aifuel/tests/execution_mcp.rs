use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn execution_endpoint_is_separate_and_cannot_grant_permissions() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args(["mcp", "execution"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    ] {
        writeln!(input, "{request}").unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        responses[0]["result"]["serverInfo"]["name"],
        "aifuel-execution"
    );
    let tools = responses[1]["result"]["tools"].as_array().unwrap();
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
    assert!(
        !tools
            .iter()
            .any(|tool| tool["name"].as_str().unwrap().contains("approv"))
    );
    assert!(!tools.iter().any(|tool| tool["name"] == "get_status"));
}
