//! A configured `anthropic-messages` endpoint driven end to end:
//! `providers.json` validation, registry resolution, managed-credential
//! resolution, and one real HTTP request against a stub server.

use crate::wire::WireExecutionAdapter;
use crate::{CredentialStore, IntegrationDescriptor, IntegrationRegistry, ProvidersConfig};
use aifuel_core::{
    AccessMode, AgentExecutionAdapter, CredentialRef, IntegrationId, OutputFormat, RunRequest,
    RunStatus, TokenUsage,
};
use std::io::Read;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tiny_http::{Header, Response, Server};

/// A unique temp directory that removes itself on drop.
struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "aifuel-wire-endpoint-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("test dir should be creatable");
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What the stub observed on the wire.
#[derive(Debug)]
struct Recorded {
    method: String,
    path: String,
    api_key: Option<String>,
    version: Option<String>,
    body: String,
}

/// A canned Anthropic Messages SSE endpoint. One request is recorded; every
/// POST answers the same documented event sequence.
struct StubMessages {
    base_url: String,
    recorded: Arc<Mutex<Vec<Recorded>>>,
}

impl StubMessages {
    fn start() -> Self {
        let server = Server::http("127.0.0.1:0").expect("stub server binds");
        let base_url = format!("http://{}", server.server_addr());
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&recorded);
        std::thread::Builder::new()
            .name("wire-stub-messages".to_owned())
            .spawn(move || {
                while let Ok(mut request) = server.recv() {
                    let mut body = String::new();
                    let _ = request
                        .as_reader()
                        .take(1024 * 1024)
                        .read_to_string(&mut body);
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
                        api_key: header("x-api-key"),
                        version: header("anthropic-version"),
                        body,
                    });
                    let sse = concat!(
                        "event: message_start\n",
                        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-stub-4\",\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n",
                        "event: content_block_start\n",
                        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                        "event: content_block_delta\n",
                        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
                        "event: content_block_delta\n",
                        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" from Anthropic\"}}\n\n",
                        "event: content_block_stop\n",
                        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                        "event: message_delta\n",
                        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":15}}\n\n",
                        "event: message_stop\n",
                        "data: {\"type\":\"message_stop\"}\n\n",
                    );
                    let response = Response::from_string(sse).with_header(
                        Header::from_str("Content-Type: text/event-stream")
                            .expect("header parses"),
                    );
                    let _ = request.respond(response);
                }
            })
            .expect("stub server thread spawns");
        Self { base_url, recorded }
    }
}

fn request(integration: &str) -> RunRequest {
    RunRequest {
        integration: IntegrationId::new(integration),
        model: Some("claude-stub-4".to_owned()),
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
    }
}

#[test]
fn a_configured_anthropic_messages_endpoint_executes_a_run() {
    // The whole path a `providers.json` entry takes: validate, register,
    // resolve, build the adapter, resolve the bound credential, send one
    // streamed request.
    let stub = StubMessages::start();
    let dir = TestDir::new();
    let store = CredentialStore::new(dir.path());
    let integration_id = IntegrationId::new("stub-anthropic");
    store
        .set_api_key_for(
            &CredentialRef::new("stub-key"),
            "sk-ant-stub",
            &integration_id,
        )
        .expect("credential writes");

    let config_path = dir.path().join("providers.json");
    std::fs::write(
        &config_path,
        format!(
            r#"{{"schema_version": 1, "integrations": [{{
                "id": "stub-anthropic",
                "provider_id": "anthropic",
                "endpoint": {{"base_url": "{}"}},
                "wire_api": "anthropic-messages",
                "auth": {{
                    "kind": "api-key-ref",
                    "credential": "stub-key",
                    "delivery": {{"header": {{"name": "x-api-key"}}}}
                }}
            }}]}}"#,
            stub.base_url
        ),
    )
    .expect("providers.json writes");

    let config = ProvidersConfig::load(&config_path).expect("config loads");
    let registry =
        IntegrationRegistry::build(Vec::<IntegrationDescriptor>::new(), config.into_entries())
            .expect("registry builds");
    let descriptor = registry
        .resolve("anthropic")
        .expect("the bare provider id resolves to the one integration");
    assert_eq!(descriptor.id(), &integration_id);

    let adapter = WireExecutionAdapter::from_integration(&descriptor.integration, store)
        .expect("anthropic-messages is a served protocol");
    let result = adapter
        .execute(
            &request("stub-anthropic"),
            &aifuel_core::RunCancellationToken::new(),
        )
        .expect("the run executes");

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(result.output, "Hello from Anthropic");
    assert_eq!(result.effective_model.as_deref(), Some("claude-stub-4"));
    assert_eq!(
        result.usage,
        Some(TokenUsage {
            input_tokens: Some(25),
            output_tokens: Some(15),
        })
    );

    let recorded = stub.recorded.lock().expect("recorded mutex");
    let request = recorded
        .first()
        .expect("the stub observed one request")
        .body
        .clone();
    let only = &recorded[0];
    assert_eq!(only.method, "POST");
    assert_eq!(only.path, "/v1/messages");
    assert_eq!(only.api_key.as_deref(), Some("sk-ant-stub"));
    assert_eq!(only.version.as_deref(), Some("2023-06-01"));
    let body: serde_json::Value = serde_json::from_str(&request).expect("JSON body");
    assert_eq!(body["model"], "claude-stub-4");
    assert_eq!(body["stream"], true);
    assert!(body["max_tokens"].as_u64().is_some());
    assert_eq!(
        body["messages"],
        serde_json::json!([{"role": "user", "content": "say hi"}])
    );
    assert_eq!(recorded.len(), 1, "exactly one request, no replay");
}
