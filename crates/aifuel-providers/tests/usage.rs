use aifuel_core::ProviderKey;
use aifuel_providers::{CollectionConfig, UsageService};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

struct TestHome {
    path: PathBuf,
}

impl TestHome {
    fn new() -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock should be after the unix epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("aifuel-usage-test-{}-{suffix}", std::process::id()));
        fs::create_dir_all(path.join(".gemini")).expect("test home should be creatable");
        fs::write(
            path.join(".gemini/oauth_creds.json"),
            r#"{"access_token":"test-token"}"#,
        )
        .expect("test credentials should be writable");
        Self { path }
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn response_for(mut stream: TcpStream) {
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        let count = stream
            .read(&mut buffer)
            .expect("request should be readable");
        if count == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..count]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let request = String::from_utf8_lossy(&request);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");
    let body = match path {
        "/loadCodeAssist" => {
            r#"{"currentTier":{"id":"free","name":"Free"},"cloudaicompanionProject":"test-project"}"#
        }
        "/retrieveUserQuota" => {
            r#"{"buckets":[{"modelId":"gemini-3-flash","remainingFraction":0.5,"resetTime":"2030-01-01T00:00:00Z"},{"modelId":"tab_completion","remainingFraction":0.9}]}"#
        }
        _ => r#"{"error":"not found"}"#,
    };
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .expect("response should be writable");
}

fn start_fixed_server(body: &'static str) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fixture server should bind");
    let address = listener
        .local_addr()
        .expect("fixture address should be available");
    let server = thread::spawn(move || {
        let stream = listener
            .incoming()
            .next()
            .expect("fixture request should arrive")
            .expect("fixture connection should open");
        response_with_body(stream, body);
    });
    (format!("http://{address}/"), server)
}

fn response_with_body(mut stream: TcpStream, body: &str) {
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        let count = stream
            .read(&mut buffer)
            .expect("request should be readable");
        if count == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..count]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .expect("response should be writable");
}

#[tokio::test]
async fn gemini_collection_normalizes_live_model_quota() {
    let home = TestHome::new();
    let listener = TcpListener::bind("127.0.0.1:0").expect("fixture server should bind");
    let address = listener
        .local_addr()
        .expect("fixture address should be available");
    let server = thread::spawn(move || {
        for stream in listener.incoming().take(2) {
            response_for(stream.expect("fixture connection should open"));
        }
    });

    let config = CollectionConfig {
        gemini_api_url: format!("http://{address}/"),
        ..CollectionConfig::default()
    };
    let service = UsageService::new(&home.path, config).expect("usage service should initialize");
    let report = service.collect().await;
    let cached = service.status(false).await;
    server.join().expect("fixture server should finish");

    assert_eq!(report.providers.len(), 1);
    assert_eq!(report.providers[0].key, ProviderKey::Gemini);
    assert_eq!(report.providers[0].status, "ok");
    assert_eq!(report.providers[0].windows[0].label, "gemini-3.5-flash");
    assert_eq!(report.providers[0].windows[0].remaining_percent, Some(50.0));
    assert_eq!(report.collection.outcome.as_deref(), Some("complete"));
    assert!(report.discovery_errors.is_empty());
    assert_eq!(cached, report);
    assert_eq!(
        fs::read_to_string(home.path.join(".gemini/oauth_creds.json"))
            .expect("credentials should remain readable"),
        r#"{"access_token":"test-token"}"#
    );
}

#[tokio::test]
async fn claude_collection_normalizes_usage_windows() {
    let home = TestHome::new();
    fs::create_dir_all(home.path.join(".claude")).expect("Claude directory should exist");
    fs::write(
        home.path.join(".claude/.credentials.json"),
        r#"{"accessToken":"test-token"}"#,
    )
    .expect("Claude credentials should exist");
    let (endpoint, server) = start_fixed_server(
        r#"{"five_hour":{"used_percentage":25,"resets_at":1893456000},"seven_day":{"remaining_percentage":90,"resets_at":1893456000}}"#,
    );
    let config = CollectionConfig {
        claude_usage_url: endpoint,
        ..CollectionConfig::default()
    };
    let service = UsageService::new(&home.path, config).expect("usage service should initialize");
    let report = service.collect().await;
    server.join().expect("fixture server should finish");

    assert_eq!(report.providers[0].key, ProviderKey::Claude);
    assert_eq!(report.providers[0].windows[0].remaining_percent, Some(75.0));
}

#[tokio::test]
async fn codex_collection_normalizes_rate_limit_windows_and_account() {
    let home = TestHome::new();
    fs::create_dir_all(home.path.join(".codex")).expect("Codex directory should exist");
    fs::write(
        home.path.join(".codex/auth.json"),
        r#"{"access_token":"test-token","account_id":"account-1"}"#,
    )
    .expect("Codex credentials should exist");
    let (endpoint, server) = start_fixed_server(
        r#"{"plan_type":"pro","rate_limit":{"primary_window":{"used_percent":25,"limit_window_seconds":18000,"reset_at":1893456000}}}"#,
    );
    let config = CollectionConfig {
        codex_usage_url: endpoint,
        ..CollectionConfig::default()
    };
    let service = UsageService::new(&home.path, config).expect("usage service should initialize");
    let report = service.collect().await;
    server.join().expect("fixture server should finish");

    assert_eq!(report.providers[0].key, ProviderKey::Codex);
    assert_eq!(report.providers[0].account_id.as_deref(), Some("account-1"));
    assert_eq!(report.providers[0].windows[0].remaining_percent, Some(75.0));
}

#[tokio::test]
async fn copilot_collection_accepts_comment_lines_and_quota_snapshots() {
    let home = TestHome::new();
    fs::create_dir_all(home.path.join(".copilot")).expect("Copilot directory should exist");
    fs::write(
        home.path.join(".copilot/config.json"),
        "// provider config\n{\"copilotTokens\":{\"oauth:user-1\":\"test-token\"}}",
    )
    .expect("Copilot credentials should exist");
    let (endpoint, server) = start_fixed_server(
        r#"{"copilot_plan":"pro","quota_snapshots":{"premium_interactions":{"entitlement":100,"remaining":40}}}"#,
    );
    let config = CollectionConfig {
        copilot_user_url: endpoint.clone(),
        copilot_token_url: endpoint,
        ..CollectionConfig::default()
    };
    let service = UsageService::new(&home.path, config).expect("usage service should initialize");
    let report = service.collect().await;
    server.join().expect("fixture server should finish");

    assert_eq!(report.providers[0].key, ProviderKey::Copilot);
    assert_eq!(report.providers[0].windows[0].remaining_percent, Some(40.0));
    assert_eq!(report.providers[0].account_id.as_deref(), Some("user-1"));
}
