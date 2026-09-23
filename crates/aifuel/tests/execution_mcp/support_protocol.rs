use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::support::{TestDirectory, ai_fuel_config_dir};

pub fn call_tools(directory: &TestDirectory, calls: &[Value], path: Option<&Path>) -> Vec<Value> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aifuel"));
    command
        .args(["mcp", "execution"])
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"));
    if let Some(path) = path {
        command.env("PATH", path);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("execution MCP server should start");
    let mut input = child.stdin.take().expect("MCP stdin should be piped");
    writeln!(input,"{}",json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).expect("initialize request should be written");
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .expect("initialized notification should be written");
    for (index, call) in calls.iter().enumerate() {
        writeln!(
            input,
            "{}",
            json!({
                "jsonrpc":"2.0",
                "id":index + 2,
                "method":"tools/call",
                "params":call
            })
        )
        .expect("tool request should be written");
    }
    drop(input);
    let command_output = child.wait_with_output().expect("MCP server should exit");
    assert!(
        command_output.status.success(),
        "{}",
        String::from_utf8_lossy(&command_output.stderr)
    );
    String::from_utf8(command_output.stdout)
        .expect("MCP frames should be UTF-8")
        .lines()
        .skip(1)
        .map(|line| serde_json::from_str(line).expect("MCP frame should be JSON"))
        .collect()
}

pub fn write_execution_config(directory: &TestDirectory, config: Value) {
    let path = ai_fuel_config_dir(directory.path()).join("execution.json");
    std::fs::create_dir_all(path.parent().expect("config path has a parent"))
        .expect("config directory should exist");
    std::fs::write(
        path,
        serde_json::to_vec(&config).expect("config should serialize"),
    )
    .expect("config should be written");
}
