use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};

mod support;

#[cfg(unix)]
use support::install_slow_help_gemini;
use support::{
    TestDirectory, install_fake_command, install_fake_gemini, path_with, start_gemini_fixture,
};
#[test]
fn run_delegates_a_prompt_to_the_selected_gemini_integration() {
    let directory = TestDirectory::new("run");
    install_fake_gemini(directory.path());

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args(["run", "--provider", "gemini", "--prompt", "hello"])
        .env("PATH", path_with(directory.path()))
        .output()
        .expect("aifuel should start");

    assert!(
        output.status.success(),
        "aifuel run should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "fake gemini response: --prompt hello --skip-trust --approval-mode plan --output-format text"
    );
}
#[test]
fn run_emits_a_structured_result_when_json_is_requested() {
    let directory = TestDirectory::new("json-run");
    install_fake_gemini(directory.path());

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "gemini",
            "--model",
            "gemini-3-flash",
            "--prompt",
            "hello",
            "--output",
            "json",
        ])
        .env("PATH", path_with(directory.path()))
        .output()
        .expect("aifuel should start");

    assert!(output.status.success());
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("run should emit JSON");
    assert_eq!(value["provider_id"], "gemini");
    assert_eq!(value["requested_model"], "gemini-3-flash");
    assert!(value["effective_model"].is_null());
    assert_eq!(value["permission_profile"], "read-only");
    assert_eq!(value["status"], "succeeded");
    assert!(value["error"].is_null());
    assert!(
        value["output"]
            .as_str()
            .expect("provider output should be text")
            .contains("--output-format json")
    );
}
#[test]
fn run_uses_the_selected_claude_integration() {
    let directory = TestDirectory::new("claude-run");
    install_fake_command(directory.path(), "claude");

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args(["run", "--provider", "claude", "--prompt", "hello"])
        .env("PATH", path_with(directory.path()))
        .output()
        .expect("aifuel should start");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "fake claude response: --print hello --permission-mode plan --output-format text"
    );
}

#[test]
fn run_uses_the_selected_codex_integration() {
    let directory = TestDirectory::new("codex-run");
    install_fake_command(directory.path(), "codex");

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args(["run", "--provider", "codex", "--prompt", "hello"])
        .env("PATH", path_with(directory.path()))
        .output()
        .expect("aifuel should start");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "fake codex response: exec --skip-git-repo-check --sandbox read-only hello"
    );
}

#[test]
fn run_uses_the_selected_copilot_integration() {
    let directory = TestDirectory::new("copilot-run");
    install_fake_command(directory.path(), "copilot");

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args(["run", "--provider", "copilot", "--prompt", "hello"])
        .env("PATH", path_with(directory.path()))
        .output()
        .expect("aifuel should start");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "fake copilot response: --prompt hello --plan --output-format text"
    );
}

#[cfg(unix)]
#[test]
fn run_timeout_applies_to_provider_capability_preflight() {
    let directory = TestDirectory::new("slow-preflight");
    install_slow_help_gemini(directory.path());

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "gemini",
            "--prompt",
            "hello",
            "--timeout",
            "1s",
        ])
        .env("PATH", path_with(directory.path()))
        .output()
        .expect("aifuel should start");

    assert_eq!(output.status.code(), Some(5));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("provider capability preflight timed out")
    );
}

#[test]
fn mcp_serves_read_only_status_over_stdio() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("aifuel MCP server should start");

    let stdin = child.stdin.as_mut().expect("MCP stdin should be piped");
    writeln!(
        stdin,
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{{}},\"clientInfo\":{{\"name\":\"test\",\"version\":\"1\"}}}}}}"
    )
    .expect("initialize request should be written");
    writeln!(
        stdin,
        "{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":{{}}}}"
    )
    .expect("initialized notification should be written");
    writeln!(
        stdin,
        "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{{}}}}"
    )
    .expect("tools/list request should be written");
    writeln!(
        stdin,
        "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"resources/read\",\"params\":{{\"uri\":\"aifuel://status\"}}}}"
    )
    .expect("resources/read request should be written");
    drop(child.stdin.take());

    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("MCP stdout should be piped")
        .read_to_string(&mut stdout)
        .expect("MCP output should be readable");
    let status = child
        .wait()
        .expect("MCP server should exit after stdin closes");

    assert!(status.success());
    let responses: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("each MCP response should be JSON"))
        .collect();

    assert_eq!(responses.len(), 3);
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(
        responses[0]["result"]["capabilities"]["tools"],
        serde_json::json!({})
    );
    assert_eq!(responses[1]["id"], 2);
    assert_eq!(responses[1]["result"]["tools"][0]["name"], "get_status");
    assert_eq!(responses[2]["id"], 3);
    assert_eq!(
        responses[2]["result"]["contents"][0]["uri"],
        "aifuel://status"
    );
    let status_text = responses[2]["result"]["contents"][0]["text"]
        .as_str()
        .expect("status resource should contain JSON text");
    let status_value: serde_json::Value =
        serde_json::from_str(status_text).expect("status resource text should be JSON");
    assert_eq!(status_value["collection"]["state"], "not_collected");
}

#[test]
fn mcp_collects_selected_provider_status_without_executing_a_prompt() {
    let home = TestDirectory::new("mcp-status");
    fs::create_dir_all(home.path().join(".gemini")).expect("Gemini directory should exist");
    fs::write(
        home.path().join(".gemini/oauth_creds.json"),
        r#"{"access_token":"test-token"}"#,
    )
    .expect("Gemini credentials should exist");
    let (endpoint, server) = start_gemini_fixture();

    let mut child = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .arg("mcp")
        .env("AIFUEL_HOME", home.path())
        .env("AIFUEL_GEMINI_API_URL", endpoint)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("aifuel MCP server should start");
    let stdin = child.stdin.as_mut().expect("MCP stdin should be piped");
    writeln!(
        stdin,
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{{}},\"clientInfo\":{{\"name\":\"test\",\"version\":\"1\"}}}}}}"
    )
    .expect("initialize request should be written");
    writeln!(
        stdin,
        "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{{\"name\":\"get_status\",\"arguments\":{{\"provider_id\":\"gemini\",\"refresh\":true}}}}}}"
    )
    .expect("get_status request should be written");
    writeln!(
        stdin,
        "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"resources/read\",\"params\":{{\"uri\":\"aifuel://status\"}}}}"
    )
    .expect("status resource request should be written");
    drop(child.stdin.take());

    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("MCP stdout should be piped")
        .read_to_string(&mut stdout)
        .expect("MCP output should be readable");
    let status = child
        .wait()
        .expect("MCP server should exit after stdin closes");
    server.join().expect("fixture server should finish");

    assert!(status.success());
    let responses: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("each MCP response should be JSON"))
        .collect();
    assert_eq!(responses.len(), 3);
    assert_eq!(responses[1]["id"], 2);
    assert_eq!(
        responses[1]["result"]["structuredContent"]["providers"][0]["key"],
        "gemini"
    );
    assert_eq!(
        responses[1]["result"]["structuredContent"]["providers"][0]["windows"][0]["remaining_percent"],
        50.0
    );
    let cached = responses[2]["result"]["contents"][0]["text"]
        .as_str()
        .expect("status resource should contain JSON text");
    assert!(cached.contains("\"key\":\"gemini\""));
}

#[test]
fn json_status_command_collects_gemini_quota_through_the_real_binary() {
    let home = TestDirectory::new("status");
    fs::create_dir_all(home.path().join(".gemini")).expect("Gemini directory should exist");
    fs::write(
        home.path().join(".gemini/oauth_creds.json"),
        r#"{"access_token":"test-token"}"#,
    )
    .expect("Gemini credentials should exist");
    let (endpoint, server) = start_gemini_fixture();

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args(["--json"])
        .env("AIFUEL_HOME", home.path())
        .env("AIFUEL_GEMINI_API_URL", endpoint)
        .output()
        .expect("aifuel should start");
    server.join().expect("fixture server should finish");

    assert!(
        output.status.success(),
        "status command should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("status command should emit JSON");
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["collection"]["outcome"], "complete");
    assert_eq!(
        value["catalog"].as_array().expect("catalog array").len(),
        69
    );
    assert_eq!(value["providers"][0]["key"], "gemini");
    assert_eq!(
        value["providers"][0]["windows"][0]["remaining_percent"],
        50.0
    );
}

#[test]
fn dashboard_serves_embedded_html_over_loopback() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args(["--no-browser", "--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("dashboard should start");
    let stdout = child
        .stdout
        .take()
        .expect("dashboard stdout should be piped");
    let mut stdout = BufReader::new(stdout);
    let mut address_line = String::new();
    stdout
        .read_line(&mut address_line)
        .expect("dashboard should print its address");
    let address = address_line
        .trim()
        .strip_prefix("aifuel dashboard: http://")
        .expect("dashboard address should be printed");
    let mut stream = TcpStream::connect(address).expect("dashboard should accept loopback HTTP");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .expect("dashboard request should be writable");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("dashboard response should be readable");
    child.kill().expect("dashboard process should be stoppable");
    child.wait().expect("dashboard process should be reaped");

    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.contains("Subscription fuel gauge"));
}

#[test]
fn dashboard_serves_normalized_status_json() {
    let home = TestDirectory::new("dashboard-status");
    fs::create_dir_all(home.path().join(".gemini")).expect("Gemini directory should exist");
    fs::write(
        home.path().join(".gemini/oauth_creds.json"),
        r#"{"access_token":"test-token"}"#,
    )
    .expect("Gemini credentials should exist");
    let (endpoint, server) = start_gemini_fixture();
    let mut child = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args(["--no-browser", "--port", "0"])
        .env("AIFUEL_HOME", home.path())
        .env("AIFUEL_GEMINI_API_URL", endpoint)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("dashboard should start");
    let stdout = child
        .stdout
        .take()
        .expect("dashboard stdout should be piped");
    let mut stdout = BufReader::new(stdout);
    let mut address_line = String::new();
    stdout
        .read_line(&mut address_line)
        .expect("dashboard should print its address");
    let address = address_line
        .trim()
        .strip_prefix("aifuel dashboard: http://")
        .expect("dashboard address should be printed");
    let mut stream = TcpStream::connect(address).expect("dashboard should accept loopback HTTP");
    stream
        .write_all(
            b"GET /api/usage?force=1 HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        )
        .expect("dashboard request should be writable");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("dashboard response should be readable");
    child.kill().expect("dashboard process should be stoppable");
    child.wait().expect("dashboard process should be reaped");
    server.join().expect("fixture server should finish");

    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.contains(r#""schema_version": 1"#));
    assert!(response.contains(r#""key": "gemini""#));
}
