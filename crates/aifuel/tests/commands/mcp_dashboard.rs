use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};

use crate::support::{TestDirectory, ai_fuel_config_dir, start_gemini_fixture};

/// Point a spawned command's user config root at the isolated test
/// directory, matching `support::ai_fuel_config_dir`.
fn configure_user_config_root(command: &mut Command, root: &TestDirectory) {
    #[cfg(windows)]
    command
        .env("APPDATA", root.path())
        .env("USERPROFILE", root.path());
    #[cfg(target_os = "macos")]
    command.env("HOME", root.path());
    #[cfg(all(unix, not(target_os = "macos")))]
    command
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path().join(".config"));
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
        76
    );
    assert_eq!(value["providers"][0]["key"], "gemini");
    assert_eq!(
        value["providers"][0]["windows"][0]["remaining_percent"],
        50.0
    );
}

#[test]
fn dashboard_serves_embedded_html_over_loopback() {
    let (mut child, address) = start_dashboard(|_| {});
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
    let (mut child, address) = start_dashboard(|command| {
        command
            .env("AIFUEL_HOME", home.path())
            .env("AIFUEL_GEMINI_API_URL", &endpoint);
    });
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

/// Start the dashboard and return the process plus its loopback address.
/// `configure` applies the test's environment isolation (config root,
/// fixture endpoints).
///
/// The child's stdout is drained on a thread: dropping the pipe's read end
/// right after the address line would make the server's next `println!`
/// hit EPIPE and panic, which kills the process mid-test.
fn start_dashboard(configure: impl FnOnce(&mut Command)) -> (Child, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aifuel"));
    command
        .args(["--no-browser", "--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    configure(&mut command);
    let mut child = command.spawn().expect("dashboard should start");
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
        .expect("dashboard address should be printed")
        .to_owned();
    std::thread::spawn(move || {
        let mut line = String::new();
        while stdout.read_line(&mut line).unwrap_or(0) > 0 {
            line.clear();
        }
    });
    (child, address)
}

fn dashboard_request(address: &str, request: &[u8]) -> String {
    let mut stream = TcpStream::connect(address).expect("dashboard should accept loopback HTTP");
    stream
        .write_all(request)
        .expect("dashboard request should be writable");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("dashboard response should be readable");
    response
}

/// The Connect panel endpoints mirror `aifuel auth`: listing reports the
/// API-key integrations' credential sources, a POSTed key lands in the same
/// Credential Store `aifuel auth set-key` writes without ever being echoed,
/// and removal returns the CLI's still-bound warnings.
#[test]
fn dashboard_connect_stores_and_removes_a_managed_credential() {
    let root = TestDirectory::new("dashboard-connect");
    let credentials_file = ai_fuel_config_dir(root.path()).join("credentials.json");
    let (mut child, address) =
        start_dashboard(|command| configure_user_config_root(command, &root));

    let response = dashboard_request(
        &address,
        b"GET /api/auth HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "auth list: {response}"
    );
    assert!(response.contains(r#""id": "openai:api-key""#));
    assert!(response.contains(r#""stored": false"#));

    // A non-JSON mutation body is rejected: the form contract is JSON, which
    // a cross-site HTML form cannot produce.
    let response = dashboard_request(
        &address,
        b"POST /api/auth/set-key HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: text/plain\r\nContent-Length: 4\r\nConnection: close\r\n\r\njunk",
    );
    assert!(
        response.starts_with("HTTP/1.1 400"),
        "non-JSON set-key: {response}"
    );
    assert!(!credentials_file.exists(), "rejected POST must not write");

    let body = r#"{"integration":"openai:api-key","key":"sk-test"}"#;
    let request = format!(
        "POST /api/auth/set-key HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: https://example.com\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let response = dashboard_request(&address, request.as_bytes());
    assert!(
        response.starts_with("HTTP/1.1 403"),
        "cross-origin set-key: {response}"
    );
    assert!(!credentials_file.exists(), "rejected POST must not write");

    let key = "sk-dashboard-test-key";
    let body = format!("{{\"integration\":\"openai:api-key\",\"key\":\"{key}\"}}");
    let request = format!(
        "POST /api/auth/set-key HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let response = dashboard_request(&address, request.as_bytes());
    assert!(response.starts_with("HTTP/1.1 200"), "set-key: {response}");
    assert!(response.contains(r#""ok": true"#));
    assert!(
        !response.contains(key),
        "the stored key must never be echoed back"
    );
    let stored = fs::read_to_string(&credentials_file)
        .expect("the credential store should exist after set-key");
    assert!(stored.contains(r#""openai:api-key""#));
    assert!(stored.contains(key));

    let response = dashboard_request(
        &address,
        b"GET /api/auth HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    );
    assert!(response.contains(r#""stored": true"#));
    assert!(
        !response.contains(key),
        "auth list must report presence, never material"
    );

    let body = r#"{"credential":"openai:api-key"}"#;
    let request = format!(
        "POST /api/auth/remove HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let response = dashboard_request(&address, request.as_bytes());
    assert!(response.starts_with("HTTP/1.1 200"), "remove: {response}");
    // Removing the only pool member strands the EnvOrStore binding, so the
    // response carries the same warning `aifuel auth remove` prints.
    assert!(response.contains("pool now holds no keys"));

    let response = dashboard_request(
        &address,
        b"GET /api/auth HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    );
    assert!(response.contains(r#""stored": false"#));

    child.kill().expect("dashboard process should be stoppable");
    child.wait().expect("dashboard process should be reaped");

    // `aifuel auth list` shares the store; the removal is visible to the CLI.
    let mut auth_list = Command::new(env!("CARGO_BIN_EXE_aifuel"));
    auth_list.args(["auth", "list"]);
    configure_user_config_root(&mut auth_list, &root);
    let output = auth_list.output().expect("auth list should run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("openai:api-key"));
    assert!(!stdout.contains("(present)"), "{stdout}");
}
