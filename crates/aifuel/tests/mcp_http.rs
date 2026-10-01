//! Streamable-HTTP coverage for the three AI Fuel MCP servers: POST/GET/DELETE
//! `/mcp`, `MCP-Session-Id`, JSON responses, 202 notifications, and SSE.

#[allow(dead_code)]
#[path = "support/gateway.rs"]
mod gateway_support;
#[allow(dead_code)]
mod support;

use gateway_support::{compile_local_mcp_server, write_catalog};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use support::TestDirectory;

struct HttpResponse {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| {
            panic!(
                "response body should be JSON, got {:?}: {error}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }
}

/// One blocking HTTP/1.1 exchange against `POST/GET/DELETE /mcp`.
fn request(
    port: u16,
    method: &str,
    session: Option<&str>,
    body: Option<&Value>,
) -> HttpResponse {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("server should accept");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("read timeout should apply");
    let payload = body
        .map(|message| serde_json::to_vec(message).expect("message should serialize"))
        .unwrap_or_default();
    let mut head = format!(
        "{method} /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n"
    );
    if let Some(session) = session {
        head.push_str(&format!("MCP-Session-Id: {session}\r\n"));
        head.push_str("MCP-Protocol-Version: 2025-11-25\r\n");
    }
    if body.is_some() {
        head.push_str("Accept: application/json, text/event-stream\r\n");
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", payload.len()));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(&payload))
        .expect("request should be written");

    let mut received = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let count = stream.read(&mut chunk).expect("response should be readable");
        if count == 0 {
            break;
        }
        received.extend_from_slice(&chunk[..count]);
        if let Some(end) = header_end(&received) {
            let headers = parse_headers(&received[..end]);
            let buffered = &received[end + 4..];
            let complete = if headers
                .get("transfer-encoding")
                .is_some_and(|value| value.contains("chunked"))
            {
                buffered.ends_with(b"0\r\n\r\n")
            } else if let Some(length) = headers
                .get("content-length")
                .and_then(|value| value.parse::<usize>().ok())
            {
                buffered.len() >= length
            } else {
                true
            };
            if complete {
                break;
            }
        }
    }
    let end = header_end(&received).expect("response should contain a header block");
    let head_bytes = &received[..end];
    let status = String::from_utf8_lossy(head_bytes)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .expect("response should carry an HTTP status");
    let headers = parse_headers(head_bytes);
    let raw = &received[end + 4..];
    let body = if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.contains("chunked"))
    {
        decode_chunked(raw)
    } else {
        raw.to_vec()
    };
    HttpResponse {
        status,
        headers,
        body,
    }
}

fn decode_chunked(raw: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    let mut rest = raw;
    loop {
        let Some(size_end) = rest.windows(2).position(|w| w == b"\r\n") else {
            break;
        };
        let size = usize::from_str_radix(
            std::str::from_utf8(&rest[..size_end])
                .unwrap_or("0")
                .trim(),
            16,
        )
        .unwrap_or(0);
        if size == 0 {
            break;
        }
        rest = &rest[size_end + 2..];
        let take = size.min(rest.len());
        body.extend_from_slice(&rest[..take]);
        rest = &rest[take..];
        if rest.starts_with(b"\r\n") {
            rest = &rest[2..];
        }
    }
    body
}

fn header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
}

fn parse_headers(head: &[u8]) -> HashMap<String, String> {
    String::from_utf8_lossy(head)
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_owned()))
        .collect()
}

/// Spawn an MCP server with `--http --port 0` and read the bound address from
/// its first stdout line. stderr goes to `server.log` in the test directory.
fn start_http_mcp(directory: &TestDirectory, args: &[&str]) -> (Child, u16) {
    let log = std::fs::File::create(directory.path().join("server.log"))
        .expect("server log should be creatable");
    let mut command = Command::new(env!("CARGO_BIN_EXE_aifuel"));
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log))
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"))
        .env("AIFUEL_HOME", directory.path());
    let mut child = command.spawn().expect("MCP server should start");
    let stdout = child.stdout.take().expect("server stdout should be piped");
    let mut reader = BufReader::new(stdout);
    let mut banner = String::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while banner.is_empty() {
        assert!(Instant::now() < deadline, "server should print its address");
        match reader.read_line(&mut banner) {
            Ok(0) => panic!("server exited before printing its address"),
            Ok(_) => {}
            Err(error) => panic!("server banner should be readable: {error}"),
        }
    }
    let port = banner
        .split("http://127.0.0.1:")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .and_then(|digits| digits.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("server banner should carry the bound port, got {banner:?}"));
    // Keep the reader alive inside the child handle's guard so a late stdout
    // write never hits a closed pipe.
    let _ = std::thread::Builder::new()
        .name("mcp-http-banner-drain".to_owned())
        .spawn(move || {
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|count| count > 0) {
                line.clear();
            }
        });
    (child, port)
}

fn initialize(port: u16) -> String {
    let response = request(
        port,
        "POST",
        None,
        Some(&json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-11-25","capabilities":{},
                      "clientInfo":{"name":"test","version":"1"}}
        })),
    );
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["result"]["protocolVersion"], "2025-11-25");
    response
        .header("mcp-session-id")
        .expect("initialize should issue MCP-Session-Id")
        .to_owned()
}

fn initialize_and_notify(port: u16) -> String {
    let session = initialize(port);
    let accepted = request(
        port,
        "POST",
        Some(&session),
        Some(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})),
    );
    assert_eq!(accepted.status, 202);
    assert!(accepted.body.is_empty());
    session
}

struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn mcp_serves_status_over_streamable_http() {
    let directory = TestDirectory::new("mcp-http-status");
    let (child, port) = start_http_mcp(&directory, &["mcp", "--http", "--port", "0"]);
    let _server = ServerGuard(child);

    // Requests outside a session are rejected before initialize.
    let rejected = request(
        port,
        "POST",
        None,
        Some(&json!({"jsonrpc":"2.0","id":9,"method":"tools/list"})),
    );
    assert_eq!(rejected.status, 400);

    let session = initialize_and_notify(port);

    let listed = request(
        port,
        "POST",
        Some(&session),
        Some(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})),
    );
    assert_eq!(listed.status, 200);
    assert_eq!(listed.json()["result"]["tools"][0]["name"], "get_status");

    let called = request(
        port,
        "POST",
        Some(&session),
        Some(&json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"get_status","arguments":{}}
        })),
    );
    assert_eq!(called.status, 200);
    assert_eq!(called.json()["result"]["isError"], false);
    assert!(called.json()["result"]["structuredContent"].is_object());

    // The status endpoint emits no server-initiated messages.
    let stream = request(port, "GET", Some(&session), None);
    assert_eq!(stream.status, 405);

    let deleted = request(port, "DELETE", Some(&session), None);
    assert_eq!(deleted.status, 200);
    let stale = request(
        port,
        "POST",
        Some(&session),
        Some(&json!({"jsonrpc":"2.0","id":4,"method":"ping"})),
    );
    assert_eq!(stale.status, 404);
}

#[test]
fn mcp_serves_execution_over_streamable_http() {
    let directory = TestDirectory::new("mcp-http-execution");
    let (child, port) =
        start_http_mcp(&directory, &["mcp", "execution", "--http", "--port", "0"]);
    let _server = ServerGuard(child);

    let session = initialize_and_notify(port);

    let listed = request(
        port,
        "POST",
        Some(&session),
        Some(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})),
    );
    assert_eq!(listed.status, 200);
    let listed_body = listed.json();
    let tools = listed_body["result"]["tools"]
        .as_array()
        .expect("execution tools should list");
    assert!(tools.iter().any(|tool| tool["name"] == "list_models"));

    let called = request(
        port,
        "POST",
        Some(&session),
        Some(&json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"list_models","arguments":{}}
        })),
    );
    assert_eq!(called.status, 200);
    let called_body = called.json();
    assert!(called_body["error"].is_null());
    assert_eq!(called_body["result"]["content"][0]["type"], "text");

    // A second HTTP session is independent: it must initialize itself.
    let other = initialize(port);
    let ping = request(
        port,
        "POST",
        Some(&other),
        Some(&json!({"jsonrpc":"2.0","id":10,"method":"ping"})),
    );
    assert_eq!(ping.status, 200);
    assert_eq!(ping.json()["result"], json!({}));

    let deleted = request(port, "DELETE", Some(&session), None);
    assert_eq!(deleted.status, 200);
}

#[test]
fn mcp_serves_gateway_over_streamable_http() {
    let directory = TestDirectory::new("mcp-http-gateway");
    let server = compile_local_mcp_server(directory.path());
    let config_root = directory.path().join("config");
    write_catalog(
        &config_root,
        json!({
            "servers": {
                "docs": {"transport":"stdio","command":server}
            },
            "defaults":[],
            "agents":{"codex":{"servers":["docs"]}}
        }),
    );

    let log = std::fs::File::create(directory.path().join("gateway-server.log"))
        .expect("server log should be creatable");
    let mut command = Command::new(env!("CARGO_BIN_EXE_aifuel"));
    command
        .args(["mcp", "gateway", "--agent", "codex", "--http", "--port", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log));
    gateway_support::configure_user_config_root(&mut command, &config_root);
    let mut child = command.spawn().expect("gateway server should start");
    let stdout = child.stdout.take().expect("gateway stdout should be piped");
    let mut reader = BufReader::new(stdout);
    let mut banner = String::new();
    reader
        .read_line(&mut banner)
        .expect("gateway banner should be readable");
    let port = banner
        .split("http://127.0.0.1:")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .and_then(|digits| digits.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("gateway banner should carry the bound port, got {banner:?}"));
    let _ = std::thread::Builder::new()
        .name("mcp-http-gateway-drain".to_owned())
        .spawn(move || {
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|count| count > 0) {
                line.clear();
            }
        });
    let _server = ServerGuard(child);

    let session = initialize_and_notify(port);

    let listed = request(
        port,
        "POST",
        Some(&session),
        Some(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})),
    );
    assert_eq!(listed.status, 200);
    let listed_body = listed.json();
    let tools = listed_body["result"]["tools"]
        .as_array()
        .expect("gateway tools should list");
    assert!(
        tools.iter().any(|tool| tool["name"] == "docs__echo"),
        "gateway should expose the fixture tool: {tools:?}"
    );

    let called = request(
        port,
        "POST",
        Some(&session),
        Some(&json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"docs__echo","arguments":{"message":"hello"}}
        })),
    );
    assert_eq!(called.status, 200);
    assert_eq!(called.json()["result"]["isError"], false);

    // The gateway emits server-initiated messages, so GET attaches an SSE
    // stream that stays open for the session.
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("GET should connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("read timeout should apply");
    write!(
        stream,
        "GET /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: text/event-stream\r\nMCP-Session-Id: {session}\r\nMCP-Protocol-Version: 2025-11-25\r\n\r\n"
    )
    .expect("GET should be written");
    let mut head = Vec::new();
    let mut chunk = [0_u8; 1024];
    while header_end(&head).is_none() {
        let count = stream.read(&mut chunk).expect("SSE head should be readable");
        assert!(count > 0, "SSE response should stay open");
        head.extend_from_slice(&chunk[..count]);
    }
    let head_text = String::from_utf8_lossy(&head).to_lowercase();
    assert!(head_text.starts_with("http/1.1 200"));
    assert!(head_text.contains("content-type: text/event-stream"));

    drop(stream);
    let deleted = request(port, "DELETE", Some(&session), None);
    assert_eq!(deleted.status, 200);
    let stale = request(
        port,
        "POST",
        Some(&session),
        Some(&json!({"jsonrpc":"2.0","id":4,"method":"ping"})),
    );
    assert_eq!(stale.status, 404);
}
