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
    write_auth_value(home, json!({"auth_mode": "chatgpt", "tokens": tokens}));
}

/// Write a credential file verbatim: refresh tests control the full shape,
/// including unknown fields the write-back must preserve.
fn write_auth_value(home: &Path, value: Value) {
    std::fs::write(home.join(".codex/auth.json"), value.to_string()).expect("auth.json");
}

fn read_auth_file(home: &Path) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(home.join(".codex/auth.json")).expect("auth.json reads"),
    )
    .expect("auth.json parses")
}

/// A JWT-shaped test token: `{"exp": <exp>}` as the payload segment.
fn jwt(exp: u64) -> String {
    jwt_claims(json!({"exp": exp}))
}

/// A JWT-shaped test token carrying arbitrary claims.
fn jwt_claims(claims: Value) -> String {
    format!(
        "header.{}.signature",
        base64url_encode(claims.to_string().as_bytes())
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
    adapter_urls(home, url, String::new())
}

/// An adapter pinned to a temp home with both endpoints stubbed: refresh
/// tests point `token_url` at a stub serving the token grant.
fn adapter_urls(home: &Path, responses_url: String, token_url: String) -> CodexOAuthAdapter {
    CodexOAuthAdapter {
        responses_url: Cow::Owned(responses_url),
        token_url: Cow::Owned(token_url),
        home: Some(home.to_path_buf()),
        client: OnceLock::new(),
        refresh_lock: OnceLock::new(),
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

/// One canned answer on the stub: an exact request path (or `*` for a
/// catch-all), the status, the content type, and the body.
type StubRoute = (&'static str, u16, &'static str, String);

impl StubResponses {
    fn start(status: u16, content_type: &'static str, body: &'static str) -> Self {
        Self::start_routing(vec![("*", status, content_type, body.to_owned())])
    }

    /// A stub that answers per request path so one server can play the
    /// token endpoint and the responses endpoint together; an unmatched
    /// path gets a bare 404.
    fn start_routing(routes: Vec<StubRoute>) -> Self {
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
                    let url = request.url().to_owned();
                    sink.lock().expect("recorded mutex").push(Recorded {
                        method: request.method().to_string(),
                        path: url.clone(),
                        authorization: header("authorization"),
                        session_id: header("session_id"),
                        account_id: header("chatgpt-account-id"),
                        originator: header("originator"),
                        body: text,
                    });
                    let (status, content_type, body) = routes
                        .iter()
                        .find(|(path, ..)| *path == "*" || *path == url)
                        .map(|(_, status, content_type, body)| {
                            (*status, *content_type, body.clone())
                        })
                        .unwrap_or((404, "text/plain", "no stub route".to_owned()));
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
fn an_expired_token_without_a_refresh_grant_is_a_relogin_error() {
    let home = TestHome::new();
    write_auth_value(
        home.path(),
        json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": "id-token",
                "access_token": jwt(100),
                "account_id": "acc",
            },
        }),
    );
    // Both URLs are unreachable on purpose: the run must fail on the
    // missing refresh grant before any request is attempted.
    let url = "http://127.0.0.1:1/unreachable".to_owned();
    let error = adapter_urls(home.path(), url.clone(), url)
        .execute(&request(), &RunCancellationToken::new())
        .err()
        .expect("expired token without a refresh grant fails");
    match error {
        AgentRunError::InvalidRequest(message) => {
            assert!(message.contains("expired"), "{message}");
            assert!(message.contains("codex login"), "{message}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[test]
fn needs_refresh_covers_expired_and_in_margin_but_not_valid_or_opaque() {
    let credentials = |token: &str| CodexCredentials {
        access_token: token.to_owned(),
        account_id: None,
        refresh_token: Some("refresh".to_owned()),
        expires_at: oauth_http::jwt_exp(token),
    };
    let now = oauth_http::unix_now();
    assert!(credentials(&jwt(100)).needs_refresh(), "expired refreshes");
    assert!(
        credentials(&jwt(now + EXPIRY_MARGIN_SECONDS)).needs_refresh(),
        "expiry inside the margin refreshes"
    );
    assert!(
        !credentials(&jwt(now + EXPIRY_MARGIN_SECONDS + 300)).needs_refresh(),
        "a valid token is never refreshed"
    );
    assert!(
        !credentials("opaque-token").needs_refresh(),
        "a token with no decodable exp is never refreshed"
    );
}

#[test]
fn oauth_client_id_prefers_the_claim_then_app_audience_then_the_constant() {
    assert_eq!(oauth_client_id("not-a-jwt"), OAUTH_CLIENT_ID);
    assert_eq!(oauth_client_id(&jwt(100)), OAUTH_CLIENT_ID);
    assert_eq!(
        oauth_client_id(&jwt_claims(
            json!({"exp": 100, "client_id": "app_other123"})
        )),
        "app_other123"
    );
    // The access token's `aud` names the API surface, not the OAuth client.
    assert_eq!(
        oauth_client_id(&jwt_claims(json!({
            "exp": 100,
            "aud": ["https://api.openai.com/v1"],
        }))),
        OAUTH_CLIENT_ID
    );
    // An `app_`-shaped audience - the shape the id token carries - wins.
    assert_eq!(
        oauth_client_id(&jwt_claims(json!({
            "exp": 100,
            "aud": ["https://api.openai.com/v1", "app_audience456"],
        }))),
        "app_audience456"
    );
    assert_eq!(
        oauth_client_id(&jwt_claims(json!({"exp": 100, "aud": "app_solo"}))),
        "app_solo"
    );
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
fn an_expired_token_is_refreshed_and_the_rotated_set_is_written_back() {
    let home = TestHome::new();
    let old_access = jwt(100);
    let new_access = jwt(oauth_http::unix_now() + 3600);
    write_auth_value(
        home.path(),
        json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "custom_field": "keep-me",
            "tokens": {
                "id_token": "old-id",
                "access_token": old_access,
                "refresh_token": "stored-refresh",
                "account_id": "acc-123",
            },
            "last_refresh": "2025-01-01T00:00:00Z",
        }),
    );
    let sse = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5-codex\",\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
    );
    let token_body = json!({
        "access_token": new_access,
        "refresh_token": "rotated-refresh",
        "id_token": "new-id",
    })
    .to_string();
    let stub = StubResponses::start_routing(vec![
        ("/oauth/token", 200, "application/json", token_body),
        ("/responses", 200, "text/event-stream", sse.to_owned()),
    ]);
    let result = adapter_urls(
        home.path(),
        format!("{}/responses", stub.base_url),
        format!("{}/oauth/token", stub.base_url),
    )
    .execute(&request(), &RunCancellationToken::new())
    .expect("a refreshed run succeeds");
    assert_eq!(result.status, aifuel_core::RunStatus::Succeeded);
    assert_eq!(result.output, "hi");

    // The refresh POST ran first, sent the grant, and carried no bearer;
    // the billed request then ran with the rotated access token.
    let recorded = stub.recorded.lock().expect("recorded mutex");
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0].path, "/oauth/token");
    assert_eq!(recorded[0].method, "POST");
    assert_eq!(recorded[0].authorization, None);
    let grant: Value = serde_json::from_str(&recorded[0].body).expect("grant JSON");
    assert_eq!(grant["grant_type"], "refresh_token");
    assert_eq!(grant["refresh_token"], "stored-refresh");
    assert_eq!(grant["client_id"], OAUTH_CLIENT_ID);
    assert_eq!(recorded[1].path, "/responses");
    assert_eq!(
        recorded[1].authorization.as_deref(),
        Some(format!("Bearer {new_access}").as_str())
    );
    drop(recorded);

    // The rotated set landed on disk: exchanged fields updated, every
    // unrelated field preserved, `last_refresh` re-stamped, mode 0600.
    let written = read_auth_file(home.path());
    assert_eq!(written["tokens"]["access_token"], new_access);
    assert_eq!(written["tokens"]["refresh_token"], "rotated-refresh");
    assert_eq!(written["tokens"]["id_token"], "new-id");
    assert_eq!(written["tokens"]["account_id"], "acc-123");
    assert_eq!(written["auth_mode"], "chatgpt");
    assert_eq!(written["custom_field"], "keep-me");
    assert!(written["OPENAI_API_KEY"].is_null());
    let last_refresh = written["last_refresh"]
        .as_str()
        .expect("last_refresh is a string");
    assert_ne!(last_refresh, "2025-01-01T00:00:00Z");
    assert!(
        chrono::DateTime::parse_from_rfc3339(last_refresh).is_ok(),
        "last_refresh is RFC 3339: {last_refresh}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.path().join(".codex/auth.json"))
            .expect("auth.json metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "auth.json stays mode 0600");
    }
}

#[test]
fn a_refresh_rejection_is_a_relogin_error_and_leaves_auth_json_untouched() {
    let home = TestHome::new();
    let file = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "old-id",
            "access_token": jwt(100),
            "refresh_token": "dead-refresh",
            "account_id": "acc",
        },
        "last_refresh": "2025-01-01T00:00:00Z",
    });
    write_auth_value(home.path(), file);
    let before =
        std::fs::read_to_string(home.path().join(".codex/auth.json")).expect("auth.json reads");
    let sse =
        "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5-codex\"}}\n\n";
    let stub = StubResponses::start_routing(vec![
        (
            "/oauth/token",
            401,
            "application/json",
            r#"{"error":"invalid_grant"}"#.to_owned(),
        ),
        ("/responses", 200, "text/event-stream", sse.to_owned()),
    ]);
    let error = adapter_urls(
        home.path(),
        format!("{}/responses", stub.base_url),
        format!("{}/oauth/token", stub.base_url),
    )
    .execute(&request(), &RunCancellationToken::new())
    .err()
    .expect("a rejected grant fails");
    match error {
        AgentRunError::InvalidRequest(message) => {
            assert!(message.contains("codex login"), "{message}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
    let recorded = stub.recorded.lock().expect("recorded mutex");
    assert_eq!(recorded.len(), 1, "the billed request is never attempted");
    assert_eq!(recorded[0].path, "/oauth/token");
    drop(recorded);
    let after =
        std::fs::read_to_string(home.path().join(".codex/auth.json")).expect("auth.json reads");
    assert_eq!(after, before, "a failed refresh rewrites nothing");
}

#[test]
fn a_valid_access_token_is_never_refreshed() {
    let home = TestHome::new();
    let access = jwt(oauth_http::unix_now() + 3600);
    write_auth(home.path(), &access, Some("acc"));
    let before =
        std::fs::read_to_string(home.path().join(".codex/auth.json")).expect("auth.json reads");
    let sse = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5-codex\"}}\n\n",
    );
    // `/oauth/token` is intentionally unlisted: a refresh attempt would
    // 404 and fail the run on its own.
    let stub = StubResponses::start_routing(vec![(
        "/responses",
        200,
        "text/event-stream",
        sse.to_owned(),
    )]);
    let result = adapter_urls(
        home.path(),
        format!("{}/responses", stub.base_url),
        format!("{}/oauth/token", stub.base_url),
    )
    .execute(&request(), &RunCancellationToken::new())
    .expect("a valid token runs without refreshing");
    assert_eq!(result.status, aifuel_core::RunStatus::Succeeded);
    let recorded = stub.recorded.lock().expect("recorded mutex");
    assert_eq!(recorded.len(), 1, "exactly one request, no refresh grant");
    assert_eq!(recorded[0].path, "/responses");
    drop(recorded);
    let after =
        std::fs::read_to_string(home.path().join(".codex/auth.json")).expect("auth.json reads");
    assert_eq!(after, before, "a valid token is never rewritten");
}

#[test]
fn a_bare_selector_runs_the_compiled_default_model() {
    let home = TestHome::new();
    write_auth(
        home.path(),
        &jwt(oauth_http::unix_now() + 3600),
        Some("acc"),
    );
    let sse = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-6-luna\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
    );
    let stub = StubResponses::start(200, "text/event-stream", sse);
    let mut bare = request();
    bare.model = None;
    let result = adapter(home.path(), stub.base_url.clone())
        .execute(&bare, &RunCancellationToken::new())
        .expect("a bare codex:oauth selector runs");
    assert_eq!(result.status, aifuel_core::RunStatus::Succeeded);

    let recorded = stub.recorded.lock().expect("recorded mutex");
    let body: Value = serde_json::from_str(&recorded[0].body).expect("request JSON");
    assert_eq!(body["model"], DEFAULT_MODEL);
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
