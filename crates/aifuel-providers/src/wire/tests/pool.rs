//! End-to-end Key Pool rotation over a scripted HTTP endpoint: `429` cools
//! a key down and the run continues on the next healthy member, `401`/`403`
//! mark a key invalid, and a fully cooled or invalid pool fails fast
//! without touching the endpoint.

use crate::wire::WireExecutionAdapter;
use crate::{CredentialStore, KeyHealth};
use aifuel_core::{
    AccessMode, AgentExecutionAdapter, AgentRunError, ApiKeySource, AuthBinding, CredentialRef,
    EndpointConfig, IntegrationId, KeyDelivery, OutputFormat, ProviderId, RunCancellationToken,
    RunRequest, RunStatus, WireApi,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use tiny_http::{Header, Response, Server};

/// The response one request gets, keyed by the bearer token it carried.
enum Answer {
    /// A complete `data:`-framed answer ending in `[DONE]`.
    Stream,
    /// An HTTP failure status plus an optional `Retry-After` seconds value.
    Status(u16, Option<u64>),
}

/// A `POST …/chat/completions` stand-in that answers per the bearer token
/// seen on the request. Every request's `Authorization` header is recorded
/// so tests assert which pool member served each attempt.
struct FakeEndpoint {
    url: String,
    seen: Arc<Mutex<Vec<String>>>,
    hits: Arc<AtomicU64>,
}

impl FakeEndpoint {
    fn start(script: BTreeMap<&'static str, Answer>) -> Self {
        let server = Server::http("127.0.0.1:0").expect("fake endpoint binds");
        let url = format!("http://{}", server.server_addr());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicU64::new(0));
        let worker = {
            let seen = Arc::clone(&seen);
            let hits = Arc::clone(&hits);
            let script = Arc::new(script);
            thread::Builder::new()
                .name("wire-pool-fake".to_owned())
                .spawn(move || {
                    while let Ok(mut request) = server.recv() {
                        let mut body = String::new();
                        request.as_reader().read_to_string(&mut body).ok();
                        let auth = request
                            .headers()
                            .iter()
                            .find(|h| h.field.equiv("authorization"))
                            .map(|h| h.value.as_str().to_owned())
                            .unwrap_or_default();
                        seen.lock().expect("seen mutex").push(auth.clone());
                        hits.fetch_add(1, Ordering::SeqCst);
                        let key = auth.strip_prefix("Bearer ").unwrap_or("").to_owned();
                        match script.get(key.as_str()) {
                            Some(Answer::Stream) => {
                                let body = "data: {\"choices\": [{\"delta\": {\"content\": \"answer\"}}]}\n\ndata: [DONE]\n\n";
                                let response = Response::from_string(body).with_header(
                                    Header::from_bytes("Content-Type", "text/event-stream")
                                        .expect("header"),
                                );
                                let _ = request.respond(response);
                            }
                            Some(Answer::Status(status, retry_after)) => {
                                let mut response =
                                    Response::empty(*status as u32).with_header(
                                        Header::from_bytes("Content-Type", "application/json")
                                            .expect("header"),
                                    );
                                if let Some(seconds) = retry_after {
                                    response = response.with_header(
                                        Header::from_bytes(
                                            "Retry-After",
                                            seconds.to_string(),
                                        )
                                        .expect("header"),
                                    );
                                }
                                let _ = request.respond(response);
                            }
                            None => {
                                let _ = request.respond(Response::empty(500));
                            }
                        }
                    }
                })
                .expect("fake endpoint thread spawns")
        };
        std::mem::forget(worker);
        Self { url, seen, hits }
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("seen mutex").clone()
    }

    fn hits(&self) -> u64 {
        self.hits.load(Ordering::SeqCst)
    }
}

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(std::path::PathBuf);

impl TestDir {
    fn new() -> Self {
        let suffix = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aifuel-wire-pool-test-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("test dir should be creatable");
        Self(path)
    }

    fn store(&self) -> CredentialStore {
        CredentialStore::new(&self.0)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const INTEGRATION: &str = "openai:api-key";

fn pool_store(dir: &TestDir, keys: &[&str]) -> CredentialStore {
    let store = dir.store();
    for key in keys {
        store
            .add_pool_api_key(
                &CredentialRef::new(INTEGRATION),
                key,
                &IntegrationId::new(INTEGRATION),
            )
            .expect("pool member stores");
    }
    store
}

fn adapter(url: &str, store: CredentialStore) -> WireExecutionAdapter {
    WireExecutionAdapter::new(
        IntegrationId::new(INTEGRATION),
        ProviderId::new("openai"),
        EndpointConfig {
            base_url: url.to_owned(),
            extra_headers: BTreeMap::new(),
            request_timeout_seconds: None,
        },
        WireApi::OpenAiChat,
        AuthBinding::ApiKey {
            source: ApiKeySource::Store {
                credential: CredentialRef::new(INTEGRATION),
            },
            delivery: KeyDelivery::Bearer,
        },
        store,
    )
    .expect("endpoint config is well-formed")
}

fn request() -> RunRequest {
    RunRequest {
        integration: IntegrationId::new(INTEGRATION),
        model: Some("test-model".to_owned()),
        effort: None,
        external_tools: None,
        account: None,
        prompt: "say hi".to_owned(),
        output: OutputFormat::Text,
        working_directory: None,
        access: AccessMode::ReadOnly,
        resume: None,
        timeout: None,
        interaction_handler: None,
        env: BTreeMap::new(),
    }
}

fn health(store: &CredentialStore, reference: &str) -> KeyHealth {
    store
        .metadata(&CredentialRef::new(reference))
        .expect("metadata reads")
        .expect("the member record exists")
        .key_health
        .expect("API-key records carry health")
}

#[test]
fn a_429_cools_the_key_and_the_run_rotates_to_the_next_member() {
    let endpoint = FakeEndpoint::start(BTreeMap::from([
        ("key-a", Answer::Status(429, Some(120))),
        ("key-b", Answer::Stream),
    ]));
    let dir = TestDir::new();
    let store = pool_store(&dir, &["key-a", "key-b"]);
    let adapter = adapter(&endpoint.url, store.clone());

    let result = adapter
        .execute(&request(), &RunCancellationToken::new())
        .expect("the run itself executes");
    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(result.output, "answer");
    // The first key answered 429; the second pool member served the run.
    assert_eq!(endpoint.seen(), ["Bearer key-a", "Bearer key-b"]);
    // Rotation is reported in diagnostics, and key material never appears.
    let diagnostics = result.diagnostics.unwrap_or_default();
    assert!(diagnostics.contains("429"), "{diagnostics}");
    assert!(!diagnostics.contains("key-a"));

    // The cooled key's state persisted; the working key stayed clean.
    assert!(matches!(
        health(&store, INTEGRATION),
        KeyHealth::Cooling { .. }
    ));
    assert_eq!(health(&store, "openai:api-key/2"), KeyHealth::Healthy);

    // The next run skips the cooling member entirely - no extra request.
    let before = endpoint.hits();
    let result = adapter
        .execute(&request(), &RunCancellationToken::new())
        .expect("the run itself executes");
    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(endpoint.hits(), before + 1);
    assert_eq!(endpoint.seen().last().unwrap(), "Bearer key-b");
}

#[test]
fn a_401_marks_the_key_invalid_and_the_run_rotates() {
    let endpoint = FakeEndpoint::start(BTreeMap::from([
        ("key-a", Answer::Status(401, None)),
        ("key-b", Answer::Stream),
    ]));
    let dir = TestDir::new();
    let store = pool_store(&dir, &["key-a", "key-b"]);
    let adapter = adapter(&endpoint.url, store.clone());

    let result = adapter
        .execute(&request(), &RunCancellationToken::new())
        .expect("the run itself executes");
    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(endpoint.seen(), ["Bearer key-a", "Bearer key-b"]);
    assert!(matches!(
        health(&store, INTEGRATION),
        KeyHealth::Invalid { .. }
    ));
}

#[test]
fn a_cooled_or_invalid_pool_fails_fast_without_a_request() {
    let endpoint = FakeEndpoint::start(BTreeMap::from([("key-a", Answer::Status(429, None))]));
    let dir = TestDir::new();
    let store = pool_store(&dir, &["key-a"]);
    let adapter = adapter(&endpoint.url, store.clone());

    // Single-member pool: the 429 fails the run and marks the key cooling.
    let result = adapter
        .execute(&request(), &RunCancellationToken::new())
        .expect("the run itself executes");
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(endpoint.hits(), 1);

    // The persisted cooldown makes the next run fail before any request.
    let error = adapter
        .execute(&request(), &RunCancellationToken::new())
        .expect_err("a fully cooled pool refuses to send");
    assert!(matches!(error, AgentRunError::InvalidRequest(_)));
    assert!(error.to_string().contains("cooling"));
    assert_eq!(endpoint.hits(), 1);
}
