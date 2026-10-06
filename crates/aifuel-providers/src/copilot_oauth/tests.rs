use super::*;
use aifuel_core::{AccessMode, AgentPresenceState, OutputFormat};
use std::io::Read;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tiny_http::{Header, Response, Server};

/// A unique temp home directory that removes itself on drop.
struct TestHome(PathBuf);

impl TestHome {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "aifuel-copilot-oauth-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("temp home");
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

fn adapter(home: &Path, base: &str) -> CopilotOAuthAdapter {
    CopilotOAuthAdapter {
        token_url: Cow::Owned(format!("{base}/copilot_internal/v2/token")),
        user_url: Cow::Owned(format!("{base}/copilot_internal/user")),
        api_fallback: Cow::Owned(base.to_owned()),
        home: Some(home.to_path_buf()),
        store: Some(CredentialStore::new(home.join("credential-store"))),
        client: OnceLock::new(),
        session: Mutex::new(None),
    }
}

fn request() -> RunRequest {
    RunRequest {
        integration: IntegrationId::new("copilot:oauth"),
        model: Some("gpt-4.1".to_owned()),
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
        optimize: Default::default(),
        interaction_handler: None,
    }
}

/// What the stub observed on the wire.
#[derive(Debug)]
struct Recorded {
    method: String,
    path: String,
    authorization: Option<String>,
    editor_version: Option<String>,
    integration_id: Option<String>,
    body: String,
}

/// A canned GitHub/Copilot pair: `GET /copilot_internal/v2/token` (or
/// `404` when `exchange_status` is one), `GET /copilot_internal/user`,
/// and `POST /chat/completions` on the reported API base.
struct StubCopilot {
    base_url: String,
    recorded: Arc<Mutex<Vec<Recorded>>>,
}

impl StubCopilot {
    fn start(exchange_status: u16) -> Self {
        Self::start_with(exchange_status, 200)
    }

    fn start_with(exchange_status: u16, user_status: u16) -> Self {
        let server = Server::http("127.0.0.1:0").expect("stub server binds");
        let base_url = format!("http://{}", server.server_addr());
        let api_base = base_url.clone();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&recorded);
        std::thread::Builder::new()
                .name("copilot-oauth-stub".to_owned())
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
                        let path = request.url().to_owned();
                        sink.lock().expect("recorded mutex").push(Recorded {
                            method: request.method().to_string(),
                            path: path.clone(),
                            authorization: header("authorization"),
                            editor_version: header("editor-version"),
                            integration_id: header("copilot-integration-id"),
                            body: text,
                        });
                        let (status, content_type, body): (u16, &str, String) = match path.as_str() {
                            "/copilot_internal/v2/token" if exchange_status == 200 => (
                                200,
                                "application/json",
                                serde_json::json!({
                                    "token": "tid=stub-session",
                                    "expires_at": oauth_http::unix_now() + 3600,
                                    "endpoints": {"api": api_base.as_str()},
                                })
                                .to_string(),
                            ),
                            "/copilot_internal/v2/token" => (
                                exchange_status,
                                "application/json",
                                r#"{"error":"no v2 token for this account"}"#.to_owned(),
                            ),
                            "/copilot_internal/user" if user_status == 200 => (
                                200,
                                "application/json",
                                serde_json::json!({
                                    "login": "stub-user",
                                    "endpoints": {"api": api_base.as_str()},
                                })
                                .to_string(),
                            ),
                            "/copilot_internal/user" => (
                                user_status,
                                "application/json",
                                r#"{"error":"the account endpoint rejects this token"}"#
                                    .to_owned(),
                            ),
                            "/chat/completions" => (
                                200,
                                "text/event-stream",
                                concat!(
                                    "data: {\"choices\":[{\"delta\":{\"content\":\"hi \"}}]}\n\n",
                                    "data: {\"choices\":[{\"delta\":{\"content\":\"there\"}}]}\n\n",
                                    "data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":3}}\n\n",
                                    "data: [DONE]\n\n",
                                )
                                .to_owned(),
                            ),
                            _ => (404, "application/json", r#"{"error":"unknown path"}"#.to_owned()),
                        };
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

fn write_copilot_config(home: &Path, json: &str) {
    let dir = home.join(".copilot");
    std::fs::create_dir_all(&dir).expect("config dir");
    std::fs::write(dir.join("config.json"), json).expect("config.json");
}

fn write_github_store(home: &Path, name: &str, json: &str) {
    let dir = home.join(".config/github-copilot");
    std::fs::create_dir_all(&dir).expect("store dir");
    std::fs::write(dir.join(name), json).expect("store file");
}

#[test]
fn the_cli_config_yields_the_logged_in_users_token() {
    let home = TestHome::new();
    write_copilot_config(
        home.path(),
        r#"{
                // The copilot CLI writes JSONC.
                // Comments head the file it manages.
                "lastLoggedInUser": {"host": "https://github.com", "login": "quoc"},
                "copilotTokens": {
                    "https://github.enterprise.example:svc": "gho_other",
                    "https://github.com:quoc": "gho_quoc"
                }
            }"#,
    );
    assert_eq!(
        copilot_config_token(&home.path().join(".copilot/config.json")).as_deref(),
        Some("gho_quoc")
    );
}

#[test]
fn hosts_and_apps_stores_yield_oauth_tokens() {
    let home = TestHome::new();
    write_github_store(
        home.path(),
        "hosts.json",
        r#"{"github.com": {"oauth_token": "gho_editor", "user": "quoc"}}"#,
    );
    assert_eq!(
        github_store_token(&home.path().join(".config/github-copilot/hosts.json")).as_deref(),
        Some("gho_editor")
    );
    write_github_store(
        home.path(),
        "apps.json",
        r#"{"github.com": {"oauth_token": "gho_apps"}}"#,
    );
    assert_eq!(
        github_store_token(&home.path().join(".config/github-copilot/apps.json")).as_deref(),
        Some("gho_apps")
    );
}

#[test]
fn the_cli_config_wins_over_editor_stores() {
    let home = TestHome::new();
    write_copilot_config(
        home.path(),
        r#"{"copilotTokens": {"https://github.com:quoc": "gho_cli"}}"#,
    );
    write_github_store(
        home.path(),
        "hosts.json",
        r#"{"github.com": {"oauth_token": "gho_editor"}}"#,
    );
    let adapter = adapter(home.path(), "http://unused");
    assert_eq!(adapter.oauth_token().expect("a token"), "gho_cli");
}

#[test]
fn no_token_anywhere_is_a_relogin_error() {
    let home = TestHome::new();
    let adapter = adapter(home.path(), "http://unused");
    match adapter.oauth_token().err().expect("no source fails") {
        AgentRunError::InvalidRequest(message) => {
            assert!(
                message.contains("/login") || message.contains("sign in"),
                "{message}"
            );
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[test]
fn an_exchanged_session_drives_a_streamed_run_and_is_cached() {
    let home = TestHome::new();
    write_copilot_config(
        home.path(),
        r#"{"copilotTokens": {"https://github.com:quoc": "gho_raw"}}"#,
    );
    let stub = StubCopilot::start(200);
    let adapter = adapter(home.path(), &stub.base_url);

    for run in 0..2 {
        let result = adapter
            .execute(&request(), &RunCancellationToken::new())
            .expect("run succeeds");
        assert_eq!(
            result.status,
            aifuel_core::RunStatus::Succeeded,
            "run {run}"
        );
        assert_eq!(result.output, "hi there");
    }

    let recorded = stub.recorded.lock().expect("recorded mutex");
    let exchanges = recorded
        .iter()
        .filter(|seen| seen.path == "/copilot_internal/v2/token")
        .count();
    assert_eq!(exchanges, 1, "the session token is cached across runs");
    let chats: Vec<_> = recorded
        .iter()
        .filter(|seen| seen.path == "/chat/completions")
        .collect();
    assert_eq!(chats.len(), 2);
    let exchange = recorded
        .iter()
        .find(|seen| seen.path == "/copilot_internal/v2/token")
        .expect("the exchange happened");
    assert_eq!(exchange.method, "GET");
    assert_eq!(
        exchange.authorization.as_deref(),
        Some("token gho_raw"),
        "the exchange authenticates with the raw OAuth token"
    );
    let chat = chats[0];
    assert_eq!(chat.method, "POST");
    assert_eq!(
        chat.authorization.as_deref(),
        Some("Bearer tid=stub-session"),
        "the chat call uses the exchanged session token"
    );
    assert!(
        chat.editor_version
            .as_deref()
            .is_some_and(|v| v.starts_with("aifuel/"))
    );
    assert_eq!(
        chat.integration_id.as_deref(),
        Some("copilot-developer-cli")
    );
    let body: Value = serde_json::from_str(&chat.body).expect("request JSON");
    assert_eq!(body["model"], "gpt-4.1");
    assert_eq!(body["stream"], true);
}

#[test]
fn a_404_on_the_exchange_uses_the_user_endpoint_fallback() {
    let home = TestHome::new();
    write_copilot_config(
        home.path(),
        r#"{"copilotTokens": {"https://github.com:quoc": "gho_raw"}}"#,
    );
    let stub = StubCopilot::start(404);
    let adapter = adapter(home.path(), &stub.base_url);
    let result = adapter
        .execute(&request(), &RunCancellationToken::new())
        .expect("fallback run succeeds");
    assert_eq!(result.status, aifuel_core::RunStatus::Succeeded);
    assert_eq!(result.output, "hi there");

    let recorded = stub.recorded.lock().expect("recorded mutex");
    assert!(
        recorded
            .iter()
            .any(|seen| seen.path == "/copilot_internal/user"),
        "the /user fallback was consulted"
    );
    let chat = recorded
        .iter()
        .find(|seen| seen.path == "/chat/completions")
        .expect("the chat call happened");
    assert_eq!(
        chat.authorization.as_deref(),
        Some("Bearer gho_raw"),
        "the fallback bearer is the raw OAuth token"
    );
}

#[test]
fn a_401_on_the_exchange_is_a_relogin_error() {
    let home = TestHome::new();
    write_copilot_config(
        home.path(),
        r#"{"copilotTokens": {"https://github.com:quoc": "gho_raw"}}"#,
    );
    // The token must fail the authoritative auth check at `/user` too;
    // otherwise the reconciliation fallback would still serve the run.
    let stub = StubCopilot::start_with(401, 401);
    let adapter = adapter(home.path(), &stub.base_url);
    match adapter
        .execute(&request(), &RunCancellationToken::new())
        .err()
        .expect("401 rejected")
    {
        AgentRunError::InvalidRequest(message) => {
            assert!(message.contains("sign in"), "{message}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[test]
fn a_403_on_the_exchange_reconciles_through_the_user_endpoint() {
    // Business/enterprise seats are rejected at `v2/token` yet answer
    // `/user` - the CLI flow serves those accounts with the OAuth token
    // as the bearer on the reported API base.
    let home = TestHome::new();
    write_copilot_config(
        home.path(),
        r#"{"copilotTokens": {"https://github.com:quoc": "gho_raw"}}"#,
    );
    let stub = StubCopilot::start(403);
    let adapter = adapter(home.path(), &stub.base_url);
    let result = adapter
        .execute(&request(), &RunCancellationToken::new())
        .expect("reconciled run succeeds");
    assert_eq!(result.status, aifuel_core::RunStatus::Succeeded);
    assert_eq!(result.output, "hi there");

    let recorded = stub.recorded.lock().expect("recorded mutex");
    assert!(
        recorded
            .iter()
            .any(|seen| seen.path == "/copilot_internal/user"),
        "the /user reconciliation was consulted"
    );
    let chat = recorded
        .iter()
        .find(|seen| seen.path == "/chat/completions")
        .expect("the chat call happened");
    assert_eq!(
        chat.authorization.as_deref(),
        Some("Bearer gho_raw"),
        "the reconciled bearer is the raw OAuth token"
    );
}

#[test]
fn agent_info_reports_a_compiled_in_process_adapter() {
    let info = adapter(Path::new("/nonexistent"), "http://unused").agent_info();
    assert_eq!(info.native_presence.state, AgentPresenceState::Present);
    assert!(info.setup_guidance.is_some());
}
