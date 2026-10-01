use super::*;
use aifuel_core::ProviderKey;
use std::io::{Read, Write as IoWrite};
use std::net::TcpListener;
use std::sync::mpsc::{self, Receiver};
use std::thread;

fn test_dir(label: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("test clock should be after the unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "aifuel-webhooks-{label}-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("test directory should be creatable");
    path
}

/// A loopback HTTP receiver that records request bodies and answers 200.
/// It accepts at most `max` connections; the test asserts how many arrived.
fn start_webhook_stub(max: usize) -> (String, Receiver<String>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("stub should bind loopback");
    let address = listener
        .local_addr()
        .expect("stub address should be available");
    let (sender, bodies) = mpsc::channel();
    let server = thread::spawn(move || {
        for stream in listener.incoming().take(max) {
            let mut stream = stream.expect("stub connection should open");
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            let body = loop {
                let count = stream
                    .read(&mut buffer)
                    .expect("request should be readable");
                if count == 0 {
                    break String::new();
                }
                request.extend_from_slice(&buffer[..count]);
                if let Some(body) = complete_body(&request) {
                    break body;
                }
            };
            let _ = sender.send(body);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .expect("stub response should be writable");
        }
    });
    (format!("http://{address}/hook"), bodies, server)
}

/// Extract the request body once headers and `Content-Length` bytes arrived.
fn complete_body(request: &[u8]) -> Option<String> {
    let header_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")?;
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
    let body_start = header_end + 4;
    if request.len() < body_start + length {
        return None;
    }
    Some(String::from_utf8_lossy(&request[body_start..body_start + length]).into_owned())
}

fn report_with_window(used_percent: f64) -> StatusReport {
    let provider = ProviderUsage::success(
        ProviderKey::Gemini,
        vec![QuotaWindow::new(
            "gemini-3.5-flash",
            "daily",
            None,
            Some(100.0 - used_percent),
            Some(1_900_000_000.0),
        )],
    );
    StatusReport::from_usage(1_800_000_000.0, vec![provider], Vec::new())
}

fn notifier_for(dir: &Path, url: &str) -> WebhookNotifier {
    fs::write(
        dir.join(WEBHOOKS_FILE_NAME),
        format!(r#"{{"webhooks": [{{"url": "{url}"}}]}}"#),
    )
    .expect("webhooks.json should be writable");
    WebhookNotifier::load(dir)
        .expect("webhooks.json should parse")
        .expect("a configured file yields a notifier")
}

#[test]
fn missing_config_disables_delivery() {
    let dir = test_dir("missing");
    assert!(
        WebhookNotifier::load(&dir)
            .expect("load should succeed")
            .is_none()
    );
}

#[test]
fn webhook_urls_follow_the_gateway_trust_rule() {
    assert!(validate_url("https://hooks.example.com/services/abc").is_ok());
    assert!(validate_url("http://127.0.0.1:8787/hook").is_ok());
    assert!(validate_url("http://[::1]:8787/hook").is_ok());
    assert!(validate_url("http://localhost:8787/hook").is_ok());
    // Plain HTTP is only allowed on loopback - the gateway's trust rule.
    assert!(validate_url("http://hooks.example.com/hook").is_err());
    assert!(validate_url("http://10.0.0.5/hook").is_err());
    // User information and fragments are rejected the same way.
    assert!(validate_url("https://user@hooks.example.com/hook").is_err());
    assert!(validate_url("https://user:pw@hooks.example.com/hook").is_err());
    assert!(validate_url("https://hooks.example.com/hook#frag").is_err());
    assert!(validate_url("ftp://127.0.0.1/hook").is_err());
    assert!(validate_url("not a url").is_err());
}

#[test]
fn config_applies_defaults_and_skips_undeliverable_entries() {
    let dir = test_dir("defaults");
    fs::write(
        dir.join(WEBHOOKS_FILE_NAME),
        r#"{
            "defaults": {"events": ["quota_reset"], "threshold_percent": 75},
            "webhooks": [
                {"url": "https://hooks.example.com/a"},
                {"url": "https://hooks.example.com/b", "events": ["threshold_crossed"], "threshold_percent": 95},
                {"url": "http://hooks.example.com/c"},
                {"url": "https://hooks.example.com/d", "events": ["bogus_event"]},
                {"url": "https://hooks.example.com/e", "threshold_percent": 140}
            ]
        }"#,
    )
    .expect("webhooks.json should be writable");

    let notifier = WebhookNotifier::load(&dir)
        .expect("config should parse")
        .expect("a configured file yields a notifier");

    // Only the two valid entries survive; plain-HTTP remote, unknown-event,
    // and out-of-range-threshold entries are skipped with warnings.
    assert_eq!(notifier.endpoints.len(), 2);
    assert_eq!(notifier.endpoints[0].threshold_percent, 75.0);
    assert_eq!(notifier.endpoints[0].events, vec![EventKind::QuotaReset]);
    assert_eq!(notifier.endpoints[1].threshold_percent, 95.0);
    assert_eq!(
        notifier.endpoints[1].events,
        vec![EventKind::ThresholdCrossed]
    );
    // URL secrets stay out of the logged display form.
    assert_eq!(notifier.endpoints[0].display, "https://hooks.example.com");
}

#[tokio::test]
async fn deliver_posts_once_per_threshold_crossing() {
    let dir = test_dir("crossing");
    let (url, bodies, server) = start_webhook_stub(1);
    let notifier = notifier_for(&dir, &url);
    let report = report_with_window(95.0);

    notifier.deliver(&report).await;
    notifier.deliver(&report).await;
    server
        .join()
        .expect("stub should have taken its one request");

    // The crossing notified once; the identical second collection did not.
    let body: serde_json::Value =
        serde_json::from_str(&bodies.recv().expect("one payload")).expect("payload is JSON");
    assert_eq!(body["event"], "threshold_crossed");
    assert_eq!(body["provider"], "gemini");
    assert_eq!(body["provider_name"], "Gemini CLI");
    assert_eq!(body["window"], "gemini-3.5-flash");
    assert_eq!(body["window_period"], "daily");
    assert_eq!(body["authoritative"], true);
    assert_eq!(body["percent_used"], 95.0);
    assert_eq!(body["percent_remaining"], 5.0);
    assert_eq!(body["threshold_percent"], 90.0);
    assert_eq!(body["reset_at"], 1_900_000_000.0);
    assert_eq!(body["checked_at"], 1_800_000_000.0);
    assert!(bodies.try_recv().is_err());
}

#[tokio::test]
async fn deliver_reports_quota_reset_when_a_depleted_window_recovers() {
    let dir = test_dir("reset");
    let (url, bodies, server) = start_webhook_stub(2);
    let notifier = notifier_for(&dir, &url);

    notifier.deliver(&report_with_window(95.0)).await;
    notifier.deliver(&report_with_window(40.0)).await;
    server.join().expect("stub should have taken both events");

    let events: Vec<String> = bodies
        .iter()
        .map(|body| {
            serde_json::from_str::<serde_json::Value>(&body).expect("payload is JSON")["event"]
                .as_str()
                .expect("event name")
                .to_owned()
        })
        .collect();
    assert_eq!(events, ["threshold_crossed", "quota_reset"]);
}

#[tokio::test]
async fn deliver_rearms_after_the_window_recovers() {
    let dir = test_dir("rearm");
    let (url, bodies, server) = start_webhook_stub(3);
    let notifier = notifier_for(&dir, &url);

    notifier.deliver(&report_with_window(95.0)).await;
    notifier.deliver(&report_with_window(40.0)).await;
    notifier.deliver(&report_with_window(97.0)).await;
    server
        .join()
        .expect("stub should have taken all three events");

    let events: Vec<String> = bodies
        .iter()
        .map(|body| {
            serde_json::from_str::<serde_json::Value>(&body).expect("payload is JSON")["event"]
                .as_str()
                .expect("event name")
                .to_owned()
        })
        .collect();
    assert_eq!(
        events,
        ["threshold_crossed", "quota_reset", "threshold_crossed"]
    );
}

#[tokio::test]
async fn dedupe_state_survives_a_notifier_restart() {
    let dir = test_dir("restart");
    let (url, bodies, server) = start_webhook_stub(1);
    let report = report_with_window(95.0);

    notifier_for(&dir, &url).deliver(&report).await;
    // A new notifier over the same directory (a fresh process) must not
    // repeat the crossing it already announced.
    notifier_for(&dir, &url).deliver(&report).await;
    server
        .join()
        .expect("stub should have taken its one request");

    assert!(bodies.recv().is_ok());
    assert!(bodies.try_recv().is_err());
}

#[tokio::test]
async fn event_subscription_filters_delivery_but_not_state() {
    let dir = test_dir("filter");
    let (url, bodies, server) = start_webhook_stub(1);
    fs::write(
        dir.join(WEBHOOKS_FILE_NAME),
        format!(r#"{{"webhooks": [{{"url": "{url}", "events": ["quota_reset"]}}]}}"#),
    )
    .expect("webhooks.json should be writable");
    let notifier = WebhookNotifier::load(&dir)
        .expect("config should parse")
        .expect("notifier");

    // The crossing is not subscribed, so nothing is posted - but the state
    // still records that the window went over, so its recovery notifies.
    notifier.deliver(&report_with_window(95.0)).await;
    assert!(bodies.try_recv().is_err());
    notifier.deliver(&report_with_window(40.0)).await;
    server
        .join()
        .expect("stub should have taken the reset event");

    let body: serde_json::Value =
        serde_json::from_str(&bodies.recv().expect("one payload")).expect("payload is JSON");
    assert_eq!(body["event"], "quota_reset");
    assert_eq!(body["percent_used"], 40.0);
}

#[tokio::test]
async fn distinct_thresholds_track_the_same_window_independently() {
    let dir = test_dir("thresholds");
    let (low_url, low_bodies, low_server) = start_webhook_stub(2);
    let (high_url, high_bodies, high_server) = start_webhook_stub(2);
    fs::write(
        dir.join(WEBHOOKS_FILE_NAME),
        format!(
            r#"{{"webhooks": [
                {{"url": "{low_url}", "threshold_percent": 80}},
                {{"url": "{high_url}", "threshold_percent": 96}}
            ]}}"#
        ),
    )
    .expect("webhooks.json should be writable");
    let notifier = WebhookNotifier::load(&dir)
        .expect("config should parse")
        .expect("notifier");

    // 92% crosses only the 80% endpoint.
    notifier.deliver(&report_with_window(92.0)).await;
    let body: serde_json::Value =
        serde_json::from_str(&low_bodies.recv().expect("low endpoint payload"))
            .expect("payload is JSON");
    assert_eq!(body["threshold_percent"], 80.0);
    assert!(high_bodies.try_recv().is_err());

    // 97% is still above 80 so it does not re-notify there, while the 96%
    // endpoint sees its own first crossing.
    notifier.deliver(&report_with_window(97.0)).await;
    assert!(high_bodies.recv().is_ok());
    // Dropping to 40% leaves both endpoints over -> each gets quota_reset.
    notifier.deliver(&report_with_window(40.0)).await;
    low_server.join().expect("low stub took both events");
    high_server.join().expect("high stub took both events");
    let last_events: Vec<String> = low_bodies
        .iter()
        .chain(high_bodies.iter())
        .map(|body| {
            serde_json::from_str::<serde_json::Value>(&body).expect("payload is JSON")["event"]
                .as_str()
                .expect("event name")
                .to_owned()
        })
        .collect();
    assert_eq!(last_events, ["quota_reset", "quota_reset"]);
}

#[tokio::test]
async fn error_providers_and_percentless_windows_raise_no_events() {
    let dir = test_dir("skipped");
    let (url, bodies, _server) = start_webhook_stub(1);
    let notifier = notifier_for(&dir, &url);

    let mut report = report_with_window(95.0);
    report.providers[0].status = ProviderStatus::Error;
    notifier.deliver(&report).await;

    let report = StatusReport::from_usage(
        1_800_000_000.0,
        vec![ProviderUsage::success(
            ProviderKey::Codex,
            vec![QuotaWindow::new(
                "reset-only",
                "weekly",
                None,
                None,
                Some(1_900_000_000.0),
            )],
        )],
        Vec::new(),
    );
    notifier.deliver(&report).await;

    // Nothing was ever delivered: a broken collection must not fabricate
    // threshold events, and a window with no percentage cannot cross one.
    assert!(bodies.try_recv().is_err());
    // No state file is written when nothing transitioned.
    assert!(!dir.join(STATE_FILE_NAME).exists());
}
