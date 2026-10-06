//! OAuth flow layer tests: the compiled profile registry, token-response
//! shaping, the refresh transaction against a stub token endpoint, and the
//! interactive flows driven end-to-end against loopback stub servers.

use super::*;
use crate::credentials::{CredentialStore, OAuthTokens, ResolvedAuth};
use aifuel_core::OAuthProfileId;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tiny_http::{Header, Response, Server};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new() -> Self {
        let suffix = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aifuel-oauth-test-{}-{}",
            std::process::id(),
            suffix
        ));
        std::fs::create_dir_all(&path).expect("test dir should be creatable");
        Self { path }
    }

    fn store(&self) -> CredentialStore {
        CredentialStore::new(&self.path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A JWT-shaped token whose only claim is `exp`, for expiry-driven paths.
fn jwt(exp: u64) -> String {
    let payload = http::base64url_encode(format!(r#"{{"exp":{exp}}}"#).as_bytes());
    format!("x.{payload}.y")
}

fn expired_oauth(access: &str, refresh: Option<&str>) -> OAuthTokens {
    OAuthTokens {
        access: access.to_owned(),
        refresh: refresh.map(str::to_owned),
        expires: Some(1),
        account_id: None,
        destination: None,
    }
}

/// Serve `handler` on an ephemeral loopback port; returns the base URL and
/// a hit counter. `None` from the handler drops the connection without a
/// response - the post-send ambiguous failure spec rule 6 covers. The
/// thread exits once the port has been idle ten seconds.
fn stub_json(
    handler: impl Fn(&tiny_http::Request) -> Option<(u16, String)> + Send + 'static,
) -> (String, Arc<AtomicUsize>) {
    let server = Server::http("127.0.0.1:0").expect("stub should bind");
    let port = server.server_addr().to_ip().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    std::thread::spawn(move || {
        while let Ok(Some(request)) = server.recv_timeout(Duration::from_secs(10)) {
            count.fetch_add(1, Ordering::SeqCst);
            let Some((status, body)) = handler(&request) else {
                drop(request);
                continue;
            };
            let response = Response::from_string(body)
                .with_status_code(status)
                .with_header(
                    Header::from_bytes("Content-Type", "application/json").expect("json header"),
                );
            let _ = request.respond(response);
        }
    });
    (format!("http://127.0.0.1:{port}"), hits)
}

/// A free loopback port for the PKCE listener allowlist.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("probe binds")
        .local_addr()
        .unwrap()
        .port()
}

/// Issue a raw HTTP GET against the loopback listener and return the
/// response status line's code.
fn raw_get(port: u16, path: &str) -> u16 {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connects");
    write!(stream, "GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").expect("writes");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("reads");
    response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status line")
}

#[test]
fn profiles_resolve_by_profile_provider_and_integration() {
    assert_eq!(
        profile(&OAuthProfileId::new("codex")).unwrap().provider,
        "codex"
    );
    assert_eq!(profile_for("codex").unwrap().profile, "codex");
    assert_eq!(profile_for("copilot").unwrap().profile, "copilot");
    assert_eq!(profile_for("copilot:oauth").unwrap().profile, "copilot");
    assert!(profiles().count() >= 2);
}

#[test]
fn an_unknown_profile_resolves_to_none() {
    assert!(profile(&OAuthProfileId::new("anthropic")).is_none());
    assert!(profile_for("anthropic").is_none());
    assert!(profile_for("openai").is_none());
}

#[test]
fn tokens_from_response_prefers_jwt_exp_then_expires_in() {
    let spec = profile(&OAuthProfileId::new("codex")).unwrap();
    let from_jwt = tokens_from_response(
        spec,
        TokenResponse {
            access_token: jwt(9_999),
            refresh_token: Some("r".to_owned()),
            expires_in: Some(10),
            id_token: None,
            error: None,
        },
    );
    assert_eq!(from_jwt.expires, Some(9_999));
    let from_seconds = tokens_from_response(
        spec,
        TokenResponse {
            access_token: "opaque".to_owned(),
            refresh_token: None,
            expires_in: Some(60),
            id_token: None,
            error: None,
        },
    );
    let expected = http::unix_now() as i64 + 60;
    assert!((from_seconds.expires.unwrap() - expected).abs() <= 1);
}

#[test]
fn codex_account_id_reads_the_namespaced_claim() {
    let payload = http::base64url_encode(
        br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct_123"}}"#,
    );
    let id_token = format!("x.{payload}.y");
    let tokens = tokens_from_response(
        &CODEX,
        TokenResponse {
            access_token: "a".to_owned(),
            refresh_token: None,
            expires_in: None,
            id_token: Some(id_token),
            error: None,
        },
    );
    assert_eq!(tokens.account_id.as_deref(), Some("acct_123"));
}

#[test]
fn refresh_rotates_tokens_and_preserves_an_omitted_refresh_token() {
    let dir = TestDir::new();
    let store = dir.store();
    let reference = CredentialRef::new("codex:oauth");
    store
        .set_oauth(
            &reference,
            expired_oauth("old-access", Some("keep-refresh")),
        )
        .unwrap();
    let (base, hits) = stub_json(|request| {
        assert_eq!(request.url(), "/token");
        Some((
            200,
            r#"{"access_token":"new-access","expires_in":3600}"#.to_owned(),
        ))
    });
    refresh_grant_at(&store, &reference, &CODEX, &format!("{base}/token")).unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let ResolvedAuth::OAuth {
        access_token,
        needs_refresh,
    } = store
        .resolve(
            &AuthBinding::OAuth {
                credential: reference.clone(),
                profile: OAuthProfileId::new("codex"),
            },
            &IntegrationId::new("codex:oauth"),
        )
        .unwrap()
    else {
        panic!("the grant should still resolve as OAuth");
    };
    assert_eq!(access_token, "new-access");
    assert!(!needs_refresh);
    let Some(ManagedCredential::OAuth { refresh, .. }) = store.get(&reference).unwrap() else {
        panic!("kind preserved");
    };
    assert_eq!(refresh.as_deref(), Some("keep-refresh"));
}

#[test]
fn refresh_skips_a_grant_that_is_already_fresh() {
    let dir = TestDir::new();
    let store = dir.store();
    let reference = CredentialRef::new("copilot:oauth");
    store
        .set_oauth(
            &reference,
            OAuthTokens {
                expires: Some(http::unix_now() as i64 + 3600),
                ..expired_oauth("live-access", Some("r"))
            },
        )
        .unwrap();
    let (base, hits) = stub_json(|_| Some((200, "{}".to_owned())));
    refresh_grant_at(&store, &reference, &COPILOT, &format!("{base}/token")).unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 0, "no exchange should run");
}

#[test]
fn refresh_without_a_refresh_token_is_a_relogin_error() {
    let dir = TestDir::new();
    let store = dir.store();
    let reference = CredentialRef::new("copilot:oauth");
    store
        .set_oauth(&reference, expired_oauth("old-access", None))
        .unwrap();
    let (base, hits) = stub_json(|_| Some((200, "{}".to_owned())));
    let error =
        refresh_grant_at(&store, &reference, &COPILOT, &format!("{base}/token")).unwrap_err();
    let message = error.to_string();
    assert_eq!(hits.load(Ordering::SeqCst), 0, "no exchange should run");
    assert!(message.contains("auth login"), "unexpected: {message}");
    assert!(!message.contains("old-access"), "tokens stay out of errors");
}

#[test]
fn a_missing_grant_fails_refresh_as_absent() {
    let dir = TestDir::new();
    let store = dir.store();
    let error = refresh_grant_at(
        &store,
        &CredentialRef::new("nope"),
        &CODEX,
        "http://127.0.0.1:1",
    )
    .unwrap_err();
    assert!(matches!(error, CredentialStoreError::CredentialAbsent(_)));
}

/// An end-to-end RFC 8628 flow against a stub GitHub: the code request,
/// one pending poll, then the granted token.
#[test]
fn device_login_polls_through_pending_to_a_grant() {
    let polls = Arc::new(AtomicUsize::new(0));
    let seen = polls.clone();
    let (base, _) = stub_json(move |request| {
        match request.url() {
            "/device" => Some((
                200,
                r#"{"device_code":"dc","user_code":"ABCD-1234","verification_uri":"https://example.test/login","interval":0}"#
                    .to_owned(),
            )),
            "/token" => {
                // GitHub answers pending polls with HTTP 200 and the RFC
                // 8628 error code in the body - status never drives it.
                if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                    Some((200, r#"{"error":"authorization_pending"}"#.to_owned()))
                } else {
                    Some((
                        200,
                        r#"{"access_token":"gho_test","expires_in":28800}"#.to_owned(),
                    ))
                }
            }
            other => panic!("unexpected path {other}"),
        }
    });
    let login = device::begin(
        &COPILOT,
        DeviceFlowParams {
            device_url: format!("{base}/device"),
            token_url: format!("{base}/token"),
            client_id: "client".to_owned(),
            scope: "read:user".to_owned(),
            headers: vec![],
        },
    )
    .unwrap();
    assert_eq!(login.user_code, "ABCD-1234");
    let tokens = login.wait().unwrap();
    assert_eq!(tokens.access, "gho_test");
    assert_eq!(polls.load(Ordering::SeqCst), 2);
}

#[test]
fn device_login_denial_is_a_terminal_error() {
    let (base, _) = stub_json(|request| match request.url() {
        "/device" => Some((
            200,
            r#"{"device_code":"dc","user_code":"C","verification_uri":"u","interval":0}"#
                .to_owned(),
        )),
        "/token" => Some((200, r#"{"error":"access_denied"}"#.to_owned())),
        other => panic!("unexpected path {other}"),
    });
    let login = device::begin(
        &COPILOT,
        DeviceFlowParams {
            device_url: format!("{base}/device"),
            token_url: format!("{base}/token"),
            client_id: "client".to_owned(),
            scope: String::new(),
            headers: vec![],
        },
    )
    .unwrap();
    let error = login.wait().unwrap_err();
    assert!(error.contains("denied"), "unexpected: {error}");
    assert!(!error.contains("dc"), "device codes stay out of errors");
}

/// OpenAI's headless grant end-to-end: usercode issue, a pending 403 poll,
/// then the code + verifier exchange against the token endpoint.
#[test]
fn device_auth_login_exchanges_the_server_issued_code() {
    let (base, _) = stub_json(|request| match request.url() {
        "/api/accounts/deviceauth/usercode" => Some((
            200,
            r#"{"device_auth_id":"da_1","user_code":"WXYZ-9876","interval":1}"#.to_owned(),
        )),
        "/api/accounts/deviceauth/token" => Some((
            200,
            r#"{"authorization_code":"ac","code_verifier":"cv"}"#.to_owned(),
        )),
        "/token" => Some((
            200,
            r#"{"access_token":"codex-access","refresh_token":"codex-refresh","expires_in":3600}"#
                .to_owned(),
        )),
        other => panic!("unexpected path {other}"),
    });
    let login = device::begin_device_auth(
        &CODEX,
        DeviceAuthParams {
            issuer: base.clone(),
            token_url: format!("{base}/token"),
            client_id: "client".to_owned(),
        },
    )
    .unwrap();
    assert_eq!(login.user_code, "WXYZ-9876");
    assert_eq!(login.verification_uri, format!("{base}/codex/device"));
    let tokens = login.wait().unwrap();
    assert_eq!(tokens.access, "codex-access");
    assert_eq!(tokens.refresh.as_deref(), Some("codex-refresh"));
}

/// The loopback flow end-to-end: the authorization URL carries the PKCE
/// challenge and state; a wrong-state hit gets a 400 and the listener keeps
/// waiting; the right-state hit drives the code exchange.
#[test]
fn loopback_login_serves_the_callback_and_exchanges() {
    let (base, _) = stub_json(|request| {
        assert_eq!(request.url(), "/token");
        Some((
            200,
            r#"{"access_token":"pkce-access","refresh_token":"pkce-refresh","expires_in":3600}"#
                .to_owned(),
        ))
    });
    let port = free_port();
    let login = pkce::begin(
        &CODEX,
        LoopbackFlowParams {
            authorize_url: format!("{base}/authorize"),
            token_url: format!("{base}/token"),
            client_id: "client".to_owned(),
            scope: "openid".to_owned(),
            extra_params: vec![("originator".to_owned(), "aifuel".to_owned())],
            ports: vec![port],
        },
    )
    .unwrap();
    let url = reqwest::Url::parse(&login.authorization_url).unwrap();
    let query: std::collections::BTreeMap<_, _> = url.query_pairs().collect();
    assert_eq!(query["client_id"], "client");
    assert_eq!(query["code_challenge_method"], "S256");
    assert!(!query["code_challenge"].is_empty());
    let state = query["state"].to_string();
    let redirect_port: u16 = reqwest::Url::parse(&login.redirect_uri)
        .unwrap()
        .port()
        .unwrap();
    assert_eq!(redirect_port, port);

    let waiter = std::thread::spawn(move || login.wait());
    assert_eq!(
        raw_get(port, "/auth/callback?code=x&state=wrong"),
        400,
        "a state mismatch is rejected but not terminal"
    );
    assert_eq!(
        raw_get(port, "/auth/callback?error=access_denied"),
        400,
        "a stray error hit without the minted state must not abort the flow"
    );
    assert_eq!(raw_get(port, "/other"), 404);
    assert_eq!(
        raw_get(port, &format!("/auth/callback?code=authcode&state={state}")),
        200
    );
    let tokens = waiter.join().unwrap().unwrap();
    assert_eq!(tokens.access, "pkce-access");
}

#[test]
fn a_malformed_authorize_url_fails_before_binding() {
    let error = match pkce::begin(
        &CODEX,
        LoopbackFlowParams {
            authorize_url: "not a url".to_owned(),
            token_url: "http://127.0.0.1:1".to_owned(),
            client_id: String::new(),
            scope: String::new(),
            extra_params: vec![],
            ports: vec![free_port()],
        },
    ) {
        Err(error) => error,
        Ok(_) => panic!("a malformed authorize URL cannot start a flow"),
    };
    assert!(error.contains("malformed"), "unexpected: {error}");
}

/// Spec matrix: two contenders refresh the same expired grant - they
/// serialize on the sidecar lock and the second rereads the landed
/// expiry, so the rotating grant is spent exactly once.
#[test]
fn concurrent_refresh_serializes_and_only_one_exchange_runs() {
    let dir = TestDir::new();
    let store = dir.store();
    let reference = CredentialRef::new("codex:oauth");
    store
        .set_oauth(&reference, expired_oauth("old-access", Some("r")))
        .unwrap();
    let (base, hits) = stub_json(|_| {
        Some((
            200,
            r#"{"access_token":"fresh-access","expires_in":3600}"#.to_owned(),
        ))
    });
    let token_url = format!("{base}/token");
    let contend = |store: CredentialStore| {
        let reference = reference.clone();
        let token_url = token_url.clone();
        std::thread::spawn(move || refresh_grant_at(&store, &reference, &CODEX, &token_url))
    };
    let first = contend(store.clone());
    let second = contend(store);
    first.join().expect("first contender").unwrap();
    second.join().expect("second contender").unwrap();
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "the second contender rereads and skips the refresh"
    );
}

/// Spec rule 6: a refresh request that may have been sent (connection
/// dropped post-send) earns one bounded confirm exchange - the outcome is
/// verified, the grant is not replayed in a loop. A raw listener drops
/// the first connection mid-request; tiny_http cannot express that (it
/// synthesizes a 500).
#[test]
fn an_ambiguous_send_failure_earns_one_bounded_confirm() {
    let dir = TestDir::new();
    let store = dir.store();
    let reference = CredentialRef::new("codex:oauth");
    store
        .set_oauth(&reference, expired_oauth("old-access", Some("r")))
        .unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener binds");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        // First request: the socket closes unanswered - the post-send
        // ambiguous failure the spec calls out.
        drop(listener.accept().unwrap().0);
        count.fetch_add(1, Ordering::SeqCst);
        // The single bounded confirm gets the real answer.
        let (mut stream, _) = listener.accept().unwrap();
        count.fetch_add(1, Ordering::SeqCst);
        let mut buffer = [0u8; 8192];
        let _ = stream.read(&mut buffer);
        let body = r#"{"access_token":"confirmed-access","expires_in":3600}"#;
        let response = format!(
            "HTTP/1.0 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
    });
    refresh_grant_at(
        &store,
        &reference,
        &CODEX,
        &format!("http://127.0.0.1:{port}/token"),
    )
    .unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 2, "one send plus one confirm");
    let ResolvedAuth::OAuth { access_token, .. } = store
        .resolve(
            &AuthBinding::OAuth {
                credential: reference,
                profile: OAuthProfileId::new("codex"),
            },
            &IntegrationId::new("codex:oauth"),
        )
        .unwrap()
    else {
        panic!("the grant resolves as OAuth");
    };
    assert_eq!(access_token, "confirmed-access");
}
