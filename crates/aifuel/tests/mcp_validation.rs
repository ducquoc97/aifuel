use std::io::Write;
use std::process::{Command, Stdio};

#[allow(dead_code)]
mod support;

#[test]
fn invalid_status_calls_leave_the_resource_uncollected() {
    let home = support::TestDirectory::new("invalid-mcp-status");
    let mut child = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .arg("mcp")
        .env("AIFUEL_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("MCP server should start");
    let mut stdin = child.stdin.take().unwrap();
    for (index, arguments) in [
        serde_json::json!([]),
        serde_json::json!("invalid"),
        serde_json::json!(false),
        serde_json::json!(1),
        serde_json::json!({"refresh": true, "unknown": true}),
    ]
    .into_iter()
    .enumerate()
    {
        writeln!(
            stdin,
            "{}",
            serde_json::json!({
                "jsonrpc": "2.0", "id": index, "method": "tools/call",
                "params": {"name": "get_status", "arguments": arguments}
            })
        )
        .unwrap();
    }
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0", "method": "tools/call",
            "params": {"name": "get_status", "arguments": {"refresh": true}}
        })
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0", "id": 5, "method": "resources/read",
            "params": {"uri": "aifuel://status"}
        })
    )
    .unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let responses: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 6);
    for response in &responses[..5] {
        assert_eq!(response["error"]["code"], -32602);
    }
    let resource: serde_json::Value = serde_json::from_str(
        responses[5]["result"]["contents"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        resource["collection"]["state"], "not_collected",
        "invalid requests must not trigger provider I/O or populate the status cache"
    );
}
