use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

pub fn compile_local_mcp_server(directory: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/local_mcp_server.rs");
    let server = directory.join(format!("local-mcp-server.{}", env::consts::EXE_EXTENSION));
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(rustc)
        .arg("--edition=2024")
        .arg(&source)
        .arg("-o")
        .arg(&server)
        .output()
        .expect("Rust compiler should build the local MCP fixture");
    assert!(
        output.status.success(),
        "local MCP fixture should compile: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    server
}

pub fn compile_resource_mcp_server(directory: &Path) -> PathBuf {
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/resource_mcp_server.rs");
    let server = directory.join(format!(
        "resource-mcp-server.{}",
        env::consts::EXE_EXTENSION
    ));
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(rustc)
        .arg("--edition=2024")
        .arg(&source)
        .arg("-o")
        .arg(&server)
        .output()
        .expect("Rust compiler should build the resource MCP fixture");
    assert!(
        output.status.success(),
        "resource MCP fixture should compile: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    server
}

pub fn user_config_file(config_root: &Path) -> PathBuf {
    #[cfg(target_os = "windows")]
    let path = config_root.join("aifuel").join("mcp.json");

    #[cfg(target_os = "macos")]
    let path = config_root
        .join("Library")
        .join("Application Support")
        .join("aifuel")
        .join("mcp.json");

    #[cfg(all(unix, not(target_os = "macos")))]
    let path = config_root.join("aifuel").join("mcp.json");

    path
}

pub fn write_catalog(config_root: &Path, catalog: Value) {
    let config_path = user_config_file(config_root);
    fs::create_dir_all(config_path.parent().expect("catalog parent should exist"))
        .expect("catalog directory should be created");
    fs::write(
        config_path,
        serde_json::to_vec(&catalog).expect("catalog should serialize"),
    )
    .expect("catalog should be writable");
}

pub fn configure_user_config_root(command: &mut Command, config_root: &Path) {
    #[cfg(target_os = "windows")]
    command
        .env("APPDATA", config_root)
        .env("USERPROFILE", config_root);

    #[cfg(target_os = "macos")]
    command.env("HOME", config_root);

    #[cfg(all(unix, not(target_os = "macos")))]
    command
        .env("XDG_CONFIG_HOME", config_root)
        .env("HOME", config_root);
}

pub fn response_reader(child: &mut Child) -> Receiver<String> {
    let stdout = child.stdout.take().expect("gateway stdout should be piped");
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if sender.send(line).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(format!("stdout read error: {error}"));
                    break;
                }
            }
        }
    });
    receiver
}

pub fn start_gateway(config_root: &Path) -> (Child, Receiver<String>) {
    start_gateway_for_agent(config_root, "codex")
}

pub fn start_gateway_for_agent(config_root: &Path, agent_id: &str) -> (Child, Receiver<String>) {
    start_gateway_for_agent_with_environment(config_root, agent_id, &[])
}

pub fn start_gateway_with_environment(
    config_root: &Path,
    environment: &[(&str, &str)],
) -> (Child, Receiver<String>) {
    start_gateway_for_agent_with_environment(config_root, "codex", environment)
}

fn start_gateway_for_agent_with_environment(
    config_root: &Path,
    agent_id: &str,
    environment: &[(&str, &str)],
) -> (Child, Receiver<String>) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aifuel"));
    command
        .args(["mcp", "gateway", "--agent", agent_id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("AIFUEL_TEST_UNLISTED_SECRET", "must-not-inherit")
        .env("AIFUEL_TEST_SECRET_SOURCE", "fixture-secret");
    for (key, value) in environment {
        command.env(key, value);
    }
    configure_user_config_root(&mut command, config_root);
    let mut child = command.spawn().expect("gateway command should start");
    let responses = response_reader(&mut child);
    (child, responses)
}

pub fn initialize_host(stdin: &mut impl Write, responses: &Receiver<String>) {
    initialize_host_with_protocol_version(stdin, responses, "2025-11-25");
}

pub fn initialize_host_with_protocol_version(
    stdin: &mut impl Write,
    responses: &Receiver<String>,
    protocol_version: &str,
) {
    send_message(
        stdin,
        initialize_request_with_protocol_version(1, protocol_version),
    );
    assert_eq!(
        response_with_id(responses, 1)["result"]["protocolVersion"],
        protocol_version
    );
    send_message(
        stdin,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    );
}

pub fn initialize_request(id: u64) -> Value {
    initialize_request_with_protocol_version(id, "2025-11-25")
}

pub fn initialize_request_with_protocol_version(id: u64, protocol_version: &str) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id":id,
        "method":"initialize",
        "params":{
            "protocolVersion":protocol_version,
            "capabilities":{},
            "clientInfo":{"name":"fixture-host","version":"1"}
        }
    })
}

pub fn read_response(reader: &mut impl BufRead, id: u64) -> Value {
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("gateway response should be readable");
        let response: Value = serde_json::from_str(line.trim_end())
            .unwrap_or_else(|error| panic!("gateway should emit JSON-RPC, got {line:?}: {error}"));
        if response["id"] == id {
            return response;
        }
    }
}

pub fn send_message(stdin: &mut impl Write, message: Value) {
    serde_json::to_writer(&mut *stdin, &message).expect("MCP request should serialize");
    stdin
        .write_all(b"\n")
        .expect("MCP request should be written");
    stdin.flush().expect("MCP request should be flushed");
}

pub fn response_with_id(responses: &Receiver<String>, id: u64) -> Value {
    loop {
        let response = next_message(responses);
        if response["id"] == id {
            return response;
        }
    }
}

pub fn response_with_id_timeout(responses: &Receiver<String>, id: u64, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let response = next_message_timeout(responses, remaining);
        if response["id"] == id {
            return response;
        }
    }
}

pub fn next_message(responses: &Receiver<String>) -> Value {
    next_message_timeout(responses, Duration::from_secs(10))
}

pub fn next_message_timeout(responses: &Receiver<String>, timeout: Duration) -> Value {
    let line = responses
        .recv_timeout(timeout)
        .expect("gateway should answer within the configured test deadline");
    serde_json::from_str(&line)
        .unwrap_or_else(|error| panic!("gateway should emit JSON-RPC, got {line:?}: {error}"))
}

pub fn wait_for_exit(child: &mut Child, timeout: Duration) -> ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("gateway status should be readable") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("gateway did not exit within {timeout:?}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

pub fn wait_for_file_content(path: &Path, expected: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(contents) = fs::read_to_string(path)
            && contents.contains(expected)
        {
            return contents;
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("fixture did not record {expected:?} in {}", path.display());
}
