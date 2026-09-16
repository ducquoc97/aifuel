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
    server.join().expect("fixture server should finish");

    assert_eq!(report.providers.len(), 1);
    assert_eq!(report.providers[0].key, ProviderKey::Gemini);
    assert_eq!(report.providers[0].status, "ok");
    assert_eq!(report.providers[0].windows[0].label, "gemini-3.5-flash");
    assert_eq!(report.providers[0].windows[0].remaining_percent, Some(50.0));
    assert_eq!(report.collection.outcome.as_deref(), Some("complete"));
    assert!(report.discovery_errors.is_empty());
}
