use super::*;
use aifuel_core::{AccessMode, AgentPresenceState, OutputFormat};
use std::io::Read;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tiny_http::{Header, Response, Server};

/// A unique temp home directory that removes itself on drop.
struct TestHome(PathBuf);

impl TestHome {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "aifuel-codex-oauth-test-{}-{}",
            std::process::id(),
            oauth_http::unix_now()
        ));
        let path = path.with_extension(format!("{}", rand_u32()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join(".codex")).expect("temp home");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn rand_u32() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed) ^ (std::process::id() as u32)
}

fn write_auth(home: &Path, access_token: &str, account_id: Option<&str>) {
    let tokens = json!({
        "id_token": "id-token",
        "access_token": access_token,
        "refresh_token": "refresh-token",
        "account_id": account_id,
    });
    std::fs::write(
        home.join(".codex/auth.json"),
        json!({"auth_mode": "chatgpt", "tokens": tokens}).to_string(),
    )
    .expect("auth.json");
}

/// A JWT-shaped test token: `{"exp": <exp>}` as the payload segment.
fn jwt(exp: u64) -> String {
    format!(
        "header.{}.signature",
        base64url_encode(format!("{{\"exp\":{exp}}}").as_bytes())
    )
}

fn base64url_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::new();
    for chunk in input.chunks(3) {
        let mut value = 0u32;
        for (i, byte) in chunk.iter().enumerate() {
            value |= (*byte as u32) << (16 - 8 * i);
        }
        for i in 0..chunk.len() + 1 {
            output.push(ALPHABET[(value >> (18 - 6 * i)) as usize & 0x3f] as char);
        }
    }
    output
}

fn adapter(home: &Path, url: String) -> CodexOAuthAdapter {
    CodexOAuthAdapter {
        responses_url: Cow::Owned(url),
        home: Some(home.to_path_buf()),
        client: OnceLock::new(),
    }
}

fn request() -> RunRequest {
    RunRequest {
        integration: IntegrationId::new("codex:oauth"),
        model: Some("gpt-5-codex".to_owned()),
        effort: None,
        external_tools: None,
        account: None,
        prompt: "say hello".to_owned(),
        output: OutputFormat::Text,
        working_directory: None,
        access: AccessMode::ReadOnly,
        resume: None,
        timeout: Some(Duration::from_secs(20)),
        env: Default::default(),
        interaction_handler: None,
    }
}

/// What the stub observed on the wire.
#[derive(Debug)]
struct Recorded {
    method: String,
    path: String,
    authorization: Option<String>,
    session_id: Option<String>,
    account_id: Option<String>,
    originator: Option<String>,
    body: String,
}

/// A canned Codex-backend endpoint: each POST is recorded and answered
/// with the configured status and body.
struct StubResponses {
    base_url: String,
    recorded: Arc<Mutex<Vec<Recorded>>>,
}

impl StubResponses {
    fn start(status: u16, content_type: &'static str, body: &'static str) -> Self {
        let server = Server::http("127.0.0.1:0").expect("stub server binds");
        let base_url = format!("http://{}", server.server_addr());
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&recorded);
        std::thread::Builder::new()
            .name("codex-oauth-stub".to_owned())
            .spawn(move || {
                while let Ok(mut request) = server.recv() {
                    let mut text = String::new();
                    let _ = request
                        .as_reader()
                        .take(1024 * 1024)
                        .read_to_string(&mut text);
                    let header = |name: &'static str| {
                        request
                            .headers()
                            .iter()
                            .find(|header| header.field.equiv(name))
                            .map(|header| header.value.as_str().to_owned())
                    };
                    sink.lock().expect("recorded mutex").push(Recorded {
                        method: request.method().to_string(),
                        path: request.url().to_owned(),
                        authorization: header("authorization"),
                        session_id: header("session_id"),
                        account_id: header("chatgpt-account-id"),
                        originator: header("originator"),
                        body: text,
                    });
                    let response = Response::from_string(body)
                        .with_status_code(tiny_http::StatusCode(status))
                        .with_header(
                            Header::from_str(&format!("Content-Type: {content_type}"))
                                .expect("header parses"),
                        );
                    let _ = request.respond(response);
                }
            })
            .expect("stub server thread spawns");
        Self { base_url, recorded }
    }
}

#[test]
fn credentials_read_access_token_and_account_id() {
    let home = TestHome::new();
    let access = jwt(oauth_http::unix_now() + 3600);
    write_auth(home.path(), &access, Some("acc-123"));
    let creds = adapter(home.path(), String::new())
        .credentials()
        .expect("credentials");
    assert_eq!(creds.access_token, access);
    assert_eq!(creds.account_id.as_deref(), Some("acc-123"));
    assert!(creds.expires_at.is_some());
}

#[test]
fn missing_auth_json_is_a_relogin_error() {
    let home = TestHome::new();
    let error = adapter(home.path(), String::new())
        .credentials()
        .err()
        .expect("no auth.json fails");
    match error {
        AgentRunError::InvalidRequest(message) => {
            assert!(message.contains("codex login"), "{message}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[test]
fn apikey_mode_has_no_oauth_tokens() {
    let home = TestHome::new();
    std::fs::write(
        home.path().join(".codex/auth.json"),
        json!({"auth_mode": "apikey", "OPENAI_API_KEY": "sk-test"}).to_string(),
    )
    .unwrap();
    let error = adapter(home.path(), String::new())
        .credentials()
        .err()
        .expect("apikey mode lacks tokens");
    match error {
        AgentRunError::InvalidRequest(message) => {
            assert!(message.contains("codex login"), "{message}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[test]
fn an_expired_access_token_fails_before_sending() {
    let home = TestHome::new();
    write_auth(home.path(), &jwt(100), Some("acc"));
    // The stub URL is unreachable on purpose: the run must fail at the
    // local expiry check before any request is attempted.
    let url = "http://127.0.0.1:1/unreachable".to_owned();
    let error = adapter(home.path(), url)
        .execute(&request(), &RunCancellationToken::new())
        .err()
        .expect("expired token fails");
    match error {
        AgentRunError::InvalidRequest(message) => {
            assert!(message.contains("expired"), "{message}");
            assert!(message.contains("codex login"), "{message}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[test]
fn request_body_matches_the_backend_strict_shape() {
    let body = request_body("gpt-5-codex", "hi there");
    assert_eq!(body["model"], "gpt-5-codex");
    assert_eq!(body["instructions"], INSTRUCTIONS);
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert_eq!(body["input"][0]["type"], "message");
    assert_eq!(body["input"][0]["role"], "user");
    assert_eq!(
        body["input"][0]["content"][0],
        json!({"type": "input_text", "text": "hi there"})
    );
}

#[test]
fn classify_event_maps_the_responses_protocol() {
    let delta = classify_event(
        Some("response.output_text.delta"),
        r#"{"type":"response.output_text.delta","delta":"hel","item_id":"m1"}"#,
    );
    assert!(matches!(delta, DataVerdict::Delta { ref text, .. } if text == "hel"));

    let completed = classify_event(
        Some("response.completed"),
        r#"{"type":"response.completed","response":{"status":"completed","model":"gpt-5-codex","usage":{"input_tokens":10,"output_tokens":4}}}"#,
    );
    match completed {
        DataVerdict::Complete { model, usage } => {
            assert_eq!(model.as_deref(), Some("gpt-5-codex"));
            assert_eq!(usage.unwrap().input_tokens, Some(10));
        }
        other => panic!("expected Complete, got {other:?}"),
    }

    let failed = classify_event(
        Some("response.failed"),
        r#"{"type":"response.failed","response":{"status":"failed","error":{"message":"boom"}}}"#,
    );
    assert!(matches!(failed, DataVerdict::Failed { ref message } if message == "boom"));

    let chatter = classify_event(
        Some("response.created"),
        r#"{"type":"response.created","response":{"model":"gpt-5-codex"}}"#,
    );
    assert!(matches!(chatter, DataVerdict::Ignored { .. }));
}

#[test]
fn a_401_maps_to_a_relogin_error() {
    let home = TestHome::new();
    write_auth(
        home.path(),
        &jwt(oauth_http::unix_now() + 3600),
        Some("acc"),
    );
    let stub = StubResponses::start(401, "application/json", r#"{"error":"bad token"}"#);
    let error = adapter(home.path(), stub.base_url.clone())
        .execute(&request(), &RunCancellationToken::new())
        .err()
        .expect("401 rejected");
    match error {
        AgentRunError::InvalidRequest(message) => {
            assert!(message.contains("codex login"), "{message}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[test]
fn a_429_maps_to_quota_exhausted() {
    let home = TestHome::new();
    write_auth(
        home.path(),
        &jwt(oauth_http::unix_now() + 3600),
        Some("acc"),
    );
    let stub = StubResponses::start(429, "application/json", r#"{"error":"slow down"}"#);
    let result = adapter(home.path(), stub.base_url.clone())
        .execute(&request(), &RunCancellationToken::new())
        .expect("429 is a failed run, not a thrown error");
    assert_eq!(result.status, aifuel_core::RunStatus::Failed);
    assert!(result.quota_exhausted);
}

#[test]
fn a_streamed_run_succeeds_with_output_usage_and_headers() {
    let home = TestHome::new();
    let access = jwt(oauth_http::unix_now() + 3600);
    write_auth(home.path(), &access, Some("acc-123"));
    let sse = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello \"}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"world\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5-codex\",\"usage\":{\"input_tokens\":7,\"output_tokens\":2}}}\n\n",
    );
    let stub = StubResponses::start(200, "text/event-stream", sse);
    let result = adapter(home.path(), stub.base_url.clone())
        .execute(&request(), &RunCancellationToken::new())
        .expect("stream run");
    assert_eq!(result.status, aifuel_core::RunStatus::Succeeded);
    assert_eq!(result.output, "hello world");
    assert_eq!(result.effective_model.as_deref(), Some("gpt-5-codex"));
    assert_eq!(result.usage.unwrap().input_tokens, Some(7));

    let recorded = stub.recorded.lock().expect("recorded mutex");
    assert_eq!(recorded.len(), 1, "exactly one request, no replay");
    let seen = &recorded[0];
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.path, "/", "the request hits the configured endpoint");
    assert_eq!(
        seen.authorization.as_deref(),
        Some(format!("Bearer {access}").as_str())
    );
    assert_eq!(seen.account_id.as_deref(), Some("acc-123"));
    assert_eq!(seen.originator.as_deref(), Some("codex_cli_rs"));
    let session = seen.session_id.as_deref().expect("session_id sent");
    assert_eq!(session.len(), 36);
    let body: Value = serde_json::from_str(&seen.body).expect("request JSON");
    assert_eq!(body["model"], "gpt-5-codex");
    assert_eq!(body["store"], false);
}

#[test]
fn agent_info_reports_a_compiled_in_process_adapter() {
    let info = adapter(Path::new("/nonexistent"), String::new()).agent_info();
    assert_eq!(info.native_presence.state, AgentPresenceState::Present);
    assert_eq!(info.native_version.version, None);
    assert_eq!(
        info.native_authentication.state,
        aifuel_core::AgentAuthenticationState::Unknown
    );
    assert!(info.setup_guidance.is_some());
}

#[test]
fn validate_rejects_foreign_integrations_and_unsupported_demands() {
    let adapter = adapter(Path::new("/nonexistent"), String::new());
    let mut foreign = request();
    foreign.integration = IntegrationId::new("codex");
    assert!(matches!(
        adapter.validate(&foreign),
        Err(AgentRunError::UnsupportedIntegration(_))
    ));
    let mut writing = request();
    writing.access = AccessMode::WorkspaceWrite;
    assert!(matches!(
        adapter.validate(&writing),
        Err(AgentRunError::InvalidRequest(_))
    ));
}
