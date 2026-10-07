use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use crate::support::{TestDirectory, ai_fuel_config_dir};

/// A Code Assist fixture whose `retrieveUserQuota` answers with the
/// remaining fraction the test currently holds, so one endpoint can walk a
/// provider across and back under the webhook threshold.
fn start_quota_fixture(remaining: Arc<Mutex<f64>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fixture should bind loopback");
    let address = listener
        .local_addr()
        .expect("fixture address should be available");
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.expect("fixture connection should open");
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let count = stream
                    .read(&mut buffer)
                    .expect("request should be readable");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if request_complete(&request) {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&request);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("");
            let body = if path.ends_with("loadCodeAssist") {
                r#"{"currentTier":{"id":"free","name":"Free"},"cloudaicompanionProject":"test-project"}"#
                    .to_owned()
            } else {
                let remaining = *remaining.lock().expect("fixture remaining");
                format!(
                    r#"{{"buckets":[{{"modelId":"gemini-3-flash","remainingFraction":{remaining},"resetTime":"2030-01-01T00:00:00Z"}}]}}"#
                )
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("fixture response should be writable");
        }
    });
    format!("http://{address}/")
}

/// True once the request carries its full `Content-Length` body - the
/// collector POSTs JSON, so headers alone are not a complete request.
fn request_complete(request: &[u8]) -> bool {
    let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let length = headers
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("content-length"))
        .and_then(|line| {
            line.split_once(':')
                .map(|(_, value)| value.trim().to_owned())
        })
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    request.len() >= header_end + 4 + length
}

/// A loopback HTTP receiver recording webhook request bodies.
fn start_webhook_receiver() -> (String, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("receiver should bind loopback");
    let address = listener
        .local_addr()
        .expect("receiver address should be available");
    let (sender, bodies) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.expect("receiver connection should open");
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let count = stream
                    .read(&mut buffer)
                    .expect("request should be readable");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if request_complete(&request) {
                    break;
                }
            }
            if let Some(body) = request_body(&request) {
                let _ = sender.send(body);
            }
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
        }
    });
    (format!("http://{address}/hook"), bodies)
}

fn request_body(request: &[u8]) -> Option<String> {
    let header_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")?;
    Some(String::from_utf8_lossy(&request[header_end + 4..]).into_owned())
}

fn configure(command: &mut Command, root: &Path) {
    command
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("APPDATA", root)
        .env("XDG_CONFIG_HOME", root.join(".config"));
}

fn collect_json(root: &Path, endpoint: &str) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aifuel"));
    command
        .args(["--json"])
        .env("AIFUEL_GEMINI_API_URL", endpoint);
    configure(&mut command, root);
    command.output().expect("aifuel should start")
}

#[test]
fn json_collection_delivers_threshold_and_reset_events_to_the_webhook() {
    let home = TestDirectory::new("webhooks");
    fs::create_dir_all(home.path().join(".gemini/antigravity-cli"))
        .expect("Antigravity directory should exist");
    fs::write(
        home.path()
            .join(".gemini/antigravity-cli/antigravity-oauth-token"),
        r#"{"access_token":"test-token"}"#,
    )
    .expect("Antigravity credentials should exist");
    let remaining = Arc::new(Mutex::new(0.05_f64));
    let endpoint = start_quota_fixture(Arc::clone(&remaining));
    let (webhook_url, bodies) = start_webhook_receiver();

    let config_dir = ai_fuel_config_dir(home.path());
    fs::create_dir_all(&config_dir).expect("aifuel config dir should be creatable");
    fs::write(
        config_dir.join("webhooks.json"),
        format!(r#"{{"webhooks": [{{"url": "{webhook_url}"}}]}}"#),
    )
    .expect("webhooks.json should be writable");

    // First collection crosses the default 90% threshold: one POST.
    let output = collect_json(home.path(), &endpoint);
    assert!(
        output.status.success(),
        "collection should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let received = bodies
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap_or_else(|_| {
            panic!(
                "the threshold crossing should be delivered; stderr: {}; stdout: {}",
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            )
        });
    let first: serde_json::Value =
        serde_json::from_str(&received).expect("webhook payload should be JSON");
    assert_eq!(first["event"], "threshold_crossed");
    assert_eq!(first["provider"], "antigravity");
    assert_eq!(first["window"], "gemini-3.5-flash");
    assert_eq!(first["percent_used"], 95.0);
    assert_eq!(first["threshold_percent"], 90.0);

    // The persisted state survives into the next process: the same crossing
    // is not re-announced on every refresh.
    let output = collect_json(home.path(), &endpoint);
    assert!(output.status.success());
    assert!(
        bodies.try_recv().is_err(),
        "an unchanged crossing must not notify again"
    );
    assert!(config_dir.join("webhook-state.json").exists());

    // The window dropping back under the threshold reports quota_reset.
    *remaining.lock().expect("fixture remaining") = 0.5;
    let output = collect_json(home.path(), &endpoint);
    assert!(output.status.success());
    let reset: serde_json::Value = serde_json::from_str(
        &bodies
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the recovery should deliver quota_reset"),
    )
    .expect("webhook payload should be JSON");
    assert_eq!(reset["event"], "quota_reset");
    assert_eq!(reset["provider"], "antigravity");
    assert_eq!(reset["percent_used"], 50.0);
}
