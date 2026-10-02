use crate::wire::openai_chat::{self, request_headers};
use crate::wire::stream::{ChunkStream, StreamEnd, drive_stream};
use crate::wire::{WireAdapterError, WireExecutionAdapter};
use crate::{CredentialStore, ResolvedAuth};
use aifuel_core::{
    AccessMode, AgentCapability, AgentExecutionAdapter, AgentRunError, AgentRunOutputHandler,
    AuthBinding, CapabilityState, EndpointConfig, ExecutionConfig, Integration, IntegrationId,
    KeyDelivery, OutputFormat, ProviderId, RunCancellationToken, RunRequest, TokenUsage, WireApi,
};
use reqwest::header::{AUTHORIZATION, HeaderName};
use std::collections::VecDeque;
use std::io;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One scripted step in a fake response body.
enum FakeChunk {
    Data(Vec<u8>),
    Error(io::Error),
    /// A chunk that never arrives; only the poll tick can end the stream.
    Pending,
}

/// A `ChunkStream` reading scripted bytes; nothing polls a socket.
struct ScriptedStream(VecDeque<FakeChunk>);

impl ScriptedStream {
    fn new(chunks: impl IntoIterator<Item = FakeChunk>) -> Self {
        Self(chunks.into_iter().collect())
    }

    /// Scripted steps never consumed. A replay would have to keep polling.
    fn unconsumed(&self) -> usize {
        self.0.len()
    }
}

impl ChunkStream for ScriptedStream {
    async fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        // `Pending` must not consume the step: the driver drops a pending
        // chunk future when the poll tick wins, then re-polls - exactly
        // like `reqwest::Response::chunk`, which loses no data when dropped
        // mid-poll.
        if matches!(self.0.front(), Some(FakeChunk::Pending)) {
            return std::future::pending().await;
        }
        match self.0.pop_front() {
            Some(FakeChunk::Data(bytes)) => Ok(Some(bytes)),
            Some(FakeChunk::Error(error)) => Err(error),
            Some(FakeChunk::Pending) => unreachable!("checked above"),
            None => Ok(None),
        }
    }
}

#[derive(Debug, Default)]
struct CollectingHandler(Mutex<Vec<String>>);

impl CollectingHandler {
    fn deltas(&self) -> Vec<String> {
        self.0.lock().expect("deltas").clone()
    }
}

impl AgentRunOutputHandler for CollectingHandler {
    fn on_output(&self, delta: &str) {
        self.0.lock().expect("deltas").push(delta.to_owned());
    }
}

fn data(payload: &serde_json::Value) -> Vec<u8> {
    format!("data: {payload}\n\n").into_bytes()
}

fn delta_chunk(text: &str) -> Vec<u8> {
    data(&serde_json::json!({"choices": [{"index": 0, "delta": {"content": text}}]}))
}

fn done_chunk() -> Vec<u8> {
    b"data: [DONE]\n\n".to_vec()
}

fn endpoint() -> EndpointConfig {
    EndpointConfig {
        base_url: "http://localhost:11434/v1".to_owned(),
        extra_headers: Default::default(),
        request_timeout_seconds: None,
    }
}

fn adapter() -> WireExecutionAdapter {
    WireExecutionAdapter::new(
        IntegrationId::new("ollama-local"),
        ProviderId::new("ollama"),
        endpoint(),
        WireApi::OpenAiChat,
        AuthBinding::None,
        // AuthBinding::None never touches the store; the path only has to
        // exist syntactically for construction.
        CredentialStore::new(std::env::temp_dir()),
    )
    .expect("a well-formed endpoint must construct")
}

fn request() -> RunRequest {
    RunRequest {
        integration: IntegrationId::new("ollama-local"),
        model: Some("llama3".to_owned()),
        effort: None,
        external_tools: None,
        account: None,
        prompt: "say hi".to_owned(),
        output: OutputFormat::Text,
        working_directory: None,
        access: AccessMode::ReadOnly,
        resume: None,
        timeout: None,
        env: Default::default(),
        interaction_handler: None,
    }
}

#[test]
fn adapter_reports_its_configured_identity() {
    let adapter = adapter();
    assert_eq!(adapter.integration(), IntegrationId::new("ollama-local"));
    assert_eq!(adapter.provider(), ProviderId::new("ollama"));
}

#[test]
fn capabilities_are_declared_honestly() {
    // Prompt-completion is a real capability here; everything the wire path
    // cannot enforce is declared Unsupported rather than Unknown.
    let capabilities = adapter().declared_agent_capabilities();
    assert_eq!(capabilities.len(), AgentCapability::ALL.len());
    for capability in AgentCapability::ALL {
        assert!(capabilities.contains_key(&capability), "{capability:?}");
    }
    for capability in [
        AgentCapability::PromptCompletion,
        AgentCapability::Streaming,
        AgentCapability::ReadOnly,
    ] {
        assert_eq!(
            capabilities[&capability].state,
            CapabilityState::Supported,
            "{capability:?} should be supported"
        );
    }
    for capability in [
        AgentCapability::WorkspaceWrite,
        AgentCapability::ExternalMcpTools,
        AgentCapability::Resume,
        AgentCapability::OrdinaryInput,
        AgentCapability::PermissionApproval,
        AgentCapability::AccountSelection,
        AgentCapability::Effort,
        AgentCapability::ModelCatalog,
        AgentCapability::StructuredOutput,
    ] {
        assert_eq!(
            capabilities[&capability].state,
            CapabilityState::Unsupported,
            "{capability:?} must not be claimed"
        );
    }
}

#[test]
fn unsupported_requirements_are_rejected_before_any_request() {
    let adapter = adapter();
    for mutate in [
        (|request: &mut RunRequest| request.access = AccessMode::WorkspaceWrite)
            as fn(&mut RunRequest),
        |request| request.external_tools = Some(vec!["gateway/tool".to_owned()]),
        |request| request.effort = Some("high".to_owned()),
        |request| request.resume = Some("session".to_owned()),
        |request| request.account = Some("account".to_owned()),
        |request| request.output = OutputFormat::Jsonl,
        |request| request.model = None,
        |request| request.model = Some("  ".to_owned()),
    ] {
        let mut request = request();
        mutate(&mut request);
        assert!(
            matches!(
                adapter.validate(&request),
                Err(AgentRunError::InvalidRequest(_))
            ),
            "request must be refused before transport"
        );
    }
    assert!(adapter.validate(&request()).is_ok());
}

#[test]
fn execute_rejects_before_reaching_transport() {
    // The endpoint is unroutable; an InvalidRequest result proves rejection
    // happened before the request touched the network.
    let adapter = WireExecutionAdapter::new(
        IntegrationId::new("ollama-local"),
        ProviderId::new("ollama"),
        EndpointConfig {
            base_url: "http://192.0.0.1:1/".to_owned(),
            ..endpoint()
        },
        WireApi::OpenAiChat,
        AuthBinding::None,
        CredentialStore::new(std::env::temp_dir()),
    )
    .unwrap();
    let mut request = request();
    request.access = AccessMode::WorkspaceWrite;
    let error = adapter
        .execute(&request, &RunCancellationToken::new())
        .expect_err("unsupported access mode must fail");
    assert!(matches!(error, AgentRunError::InvalidRequest(_)));
}

#[test]
fn foreign_integrations_are_rejected() {
    let mut request = request();
    request.integration = IntegrationId::new("someone-else");
    assert!(matches!(
        adapter().execute(&request, &RunCancellationToken::new()),
        Err(AgentRunError::UnsupportedIntegration(_))
    ));
}

#[test]
fn construction_rejects_unusable_endpoints() {
    for base_url in ["not a url", "ftp://localhost/v1"] {
        assert!(matches!(
            WireExecutionAdapter::new(
                IntegrationId::new("x"),
                ProviderId::new("x"),
                EndpointConfig {
                    base_url: base_url.to_owned(),
                    ..endpoint()
                },
                WireApi::OpenAiChat,
                AuthBinding::None,
                CredentialStore::new(std::env::temp_dir()),
            ),
            Err(WireAdapterError::InvalidConfiguration(_))
        ));
    }
}

#[test]
fn from_integration_serves_only_protocols_with_a_compiled_engine() {
    // OpenAiChat and AnthropicMessages have engines; OpenAiResponses is a
    // compiled enum variant with none, and a CLI integration is a different
    // execution contract entirely. Both are rejected, never approximated.
    let mut integration = Integration {
        id: IntegrationId::new("work"),
        provider: ProviderId::new("anthropic"),
        name: "work".to_owned(),
        execution: ExecutionConfig::Http {
            endpoint: endpoint(),
            protocol: WireApi::OpenAiResponses,
            auth: AuthBinding::None,
        },
        monitoring: None,
    };
    let credentials = CredentialStore::new(std::env::temp_dir());
    assert!(matches!(
        WireExecutionAdapter::from_integration(&integration, credentials.clone()),
        Err(WireAdapterError::IncompatibleExecution(_))
    ));
    integration.execution = ExecutionConfig::Cli {
        adapter: aifuel_core::CliAdapterId::new("claude"),
    };
    assert!(matches!(
        WireExecutionAdapter::from_integration(&integration, credentials.clone()),
        Err(WireAdapterError::IncompatibleExecution(_))
    ));
    for protocol in [WireApi::OpenAiChat, WireApi::AnthropicMessages] {
        integration.execution = ExecutionConfig::Http {
            endpoint: endpoint(),
            protocol,
            auth: AuthBinding::None,
        };
        let adapter = WireExecutionAdapter::from_integration(&integration, credentials.clone())
            .unwrap_or_else(|error| panic!("{protocol:?} integrations are served: {error}"));
        assert_eq!(adapter.integration(), IntegrationId::new("work"));
    }
}

#[test]
fn new_rejects_a_protocol_without_a_compiled_engine() {
    assert!(matches!(
        WireExecutionAdapter::new(
            IntegrationId::new("x"),
            ProviderId::new("x"),
            endpoint(),
            WireApi::OpenAiResponses,
            AuthBinding::None,
            CredentialStore::new(std::env::temp_dir()),
        ),
        Err(WireAdapterError::IncompatibleExecution(_))
    ));
}

#[test]
fn managed_auth_is_applied_after_configured_headers() {
    // The spec forbids an endpoint's configured headers from overriding
    // managed auth; the binding is applied last.
    let mut endpoint = endpoint();
    endpoint
        .extra_headers
        .insert("authorization".to_owned(), "Bearer smuggled".to_owned());
    let auth = ResolvedAuth::ApiKey {
        key: "managed-key".to_owned(),
        delivery: KeyDelivery::Bearer,
    };
    let headers = request_headers(&endpoint, &auth).unwrap();
    assert_eq!(headers.get_all(AUTHORIZATION).iter().count(), 1);
    assert_eq!(headers[AUTHORIZATION], "Bearer managed-key");
}

#[test]
fn api_key_header_delivery_uses_the_named_header() {
    let auth = ResolvedAuth::ApiKey {
        key: "k".to_owned(),
        delivery: KeyDelivery::Header {
            name: "x-api-key".to_owned(),
        },
    };
    let headers = request_headers(&endpoint(), &auth).unwrap();
    assert_eq!(headers["x-api-key"], "k");
    assert!(!headers.contains_key(AUTHORIZATION));
}

#[test]
fn cookie_delivery_sends_the_normalized_cookie_header() {
    // A session credential travels as the Cookie header, never
    // Authorization: a bare token is named `name=value`, and a pasted
    // Cookie header line passes through as its value.
    for (material, expected) in [
        ("abc123", "sessionKey=abc123"),
        ("Cookie: sessionKey=abc123; a=b", "sessionKey=abc123; a=b"),
    ] {
        let auth = ResolvedAuth::ApiKey {
            key: material.to_owned(),
            delivery: KeyDelivery::Cookie {
                name: "sessionKey".to_owned(),
            },
        };
        let headers = request_headers(&endpoint(), &auth).unwrap();
        assert_eq!(headers[reqwest::header::COOKIE], expected);
        assert!(
            !headers.contains_key(AUTHORIZATION),
            "session material must never become a Bearer token"
        );
    }
}

#[test]
fn configuration_validation_rejects_a_malformed_cookie_name() {
    // The delivery name heads `name=value` at send time; a config-declared
    // name that cannot is a construction error, not a run-time surprise.
    assert!(matches!(
        WireExecutionAdapter::new(
            IntegrationId::new("x"),
            ProviderId::new("x"),
            endpoint(),
            WireApi::OpenAiChat,
            AuthBinding::ApiKey {
                source: aifuel_core::ApiKeySource::Env {
                    var: "V".to_owned(),
                },
                delivery: KeyDelivery::Cookie {
                    name: "not a name;".to_owned(),
                },
            },
            CredentialStore::new(std::env::temp_dir()),
        ),
        Err(WireAdapterError::InvalidConfiguration(_))
    ));
}

#[test]
fn oauth_resolution_applies_a_bearer_token() {
    let auth = ResolvedAuth::OAuth {
        access_token: "tok".to_owned(),
        needs_refresh: false,
    };
    let headers = request_headers(&endpoint(), &auth).unwrap();
    assert_eq!(headers[AUTHORIZATION], "Bearer tok");
}

#[test]
fn no_auth_binding_sends_no_credential() {
    let headers = request_headers(&endpoint(), &ResolvedAuth::None).unwrap();
    assert!(!headers.contains_key(AUTHORIZATION));
    assert!(!headers.contains_key(HeaderName::from_str("x-api-key").unwrap()));
}

#[tokio::test(flavor = "current_thread")]
async fn deltas_stream_to_the_owner_and_terminal_completes() {
    let mut stream = ScriptedStream::new([
        FakeChunk::Data(delta_chunk("Hel")),
        FakeChunk::Data(delta_chunk("lo")),
        FakeChunk::Data(done_chunk()),
    ]);
    let handler = CollectingHandler::default();
    let outcome = drive_stream(
        &mut stream,
        openai_chat::classify_event,
        None,
        Duration::from_secs(60),
        Duration::from_millis(10),
        &RunCancellationToken::new(),
        Some(&handler),
    )
    .await;
    assert_eq!(outcome.end, StreamEnd::Completed);
    assert_eq!(outcome.output, "Hello");
    assert_eq!(handler.deltas(), ["Hel", "lo"]);
}

#[tokio::test(flavor = "current_thread")]
async fn usage_chunk_after_finish_reason_still_lands_on_the_outcome() {
    // With `include_usage`, real endpoints send `finish_reason` BEFORE the
    // usage-only chunk. Ending the stream at the finish verdict would drop
    // the accounting, so the driver must keep reading until `[DONE]` or EOF.
    let finish =
        serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]});
    let usage =
        serde_json::json!({"choices": [], "usage": {"prompt_tokens": 7, "completion_tokens": 3}});
    let mut stream = ScriptedStream::new([
        FakeChunk::Data(delta_chunk("answer")),
        FakeChunk::Data(data(&finish)),
        FakeChunk::Data(data(&usage)),
        FakeChunk::Data(done_chunk()),
    ]);
    let outcome = drive_stream(
        &mut stream,
        openai_chat::classify_event,
        None,
        Duration::from_secs(60),
        Duration::from_millis(10),
        &RunCancellationToken::new(),
        None,
    )
    .await;
    assert_eq!(outcome.end, StreamEnd::Completed);
    assert_eq!(outcome.output, "answer");
    assert_eq!(
        outcome.usage,
        Some(TokenUsage {
            input_tokens: Some(7),
            output_tokens: Some(3),
        })
    );
}

#[tokio::test(flavor = "current_thread")]
async fn eof_after_a_finish_reason_is_completed_not_truncated() {
    // A peer may omit `[DONE]` entirely: once a finish verdict arrived, EOF
    // closes a complete answer rather than a truncated one.
    let finish =
        serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]});
    let mut stream = ScriptedStream::new([
        FakeChunk::Data(delta_chunk("answer")),
        FakeChunk::Data(data(&finish)),
    ]);
    let outcome = drive_stream(
        &mut stream,
        openai_chat::classify_event,
        None,
        Duration::from_secs(60),
        Duration::from_millis(10),
        &RunCancellationToken::new(),
        None,
    )
    .await;
    assert_eq!(outcome.end, StreamEnd::Completed);
    assert_eq!(outcome.output, "answer");
}

#[tokio::test(flavor = "current_thread")]
async fn mid_stream_eof_fails_and_preserves_partial_output() {
    // A stream that closes without `[DONE]` or a finish reason is a
    // truncated answer, and truncated is failure - never a quiet success.
    let mut stream = ScriptedStream::new([
        FakeChunk::Data(delta_chunk("partial ")),
        FakeChunk::Data(delta_chunk("answer")),
    ]);
    let outcome = drive_stream(
        &mut stream,
        openai_chat::classify_event,
        None,
        Duration::from_secs(60),
        Duration::from_millis(10),
        &RunCancellationToken::new(),
        None,
    )
    .await;
    assert_eq!(outcome.end, StreamEnd::TruncatedEof);
    assert_eq!(outcome.output, "partial answer");
}

#[tokio::test(flavor = "current_thread")]
async fn transport_error_fails_without_replaying() {
    // An ambiguous disconnect may have been billed; the run fails with the
    // partial answer and the stream is never drained for a second attempt.
    let mut stream = ScriptedStream::new([
        FakeChunk::Data(delta_chunk("kept")),
        FakeChunk::Error(io::Error::new(io::ErrorKind::BrokenPipe, "dropped")),
        FakeChunk::Data(delta_chunk("must not be read")),
    ]);
    let outcome = drive_stream(
        &mut stream,
        openai_chat::classify_event,
        None,
        Duration::from_secs(60),
        Duration::from_millis(10),
        &RunCancellationToken::new(),
        None,
    )
    .await;
    assert!(matches!(outcome.end, StreamEnd::Failed(_)));
    assert_eq!(outcome.output, "kept");
    assert_eq!(stream.unconsumed(), 1, "no replay means no further poll");
}

#[tokio::test(flavor = "current_thread")]
async fn provider_error_payload_fails_the_stream() {
    let mut stream = ScriptedStream::new([FakeChunk::Data(data(
        &serde_json::json!({"error": {"message": "quota exhausted"}}),
    ))]);
    let outcome = drive_stream(
        &mut stream,
        openai_chat::classify_event,
        None,
        Duration::from_secs(60),
        Duration::from_millis(10),
        &RunCancellationToken::new(),
        None,
    )
    .await;
    assert_eq!(outcome.end, StreamEnd::Failed("quota exhausted".to_owned()));
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_wins_over_a_hung_stream() {
    let cancellation = RunCancellationToken::new();
    cancellation.cancel();
    let mut stream = ScriptedStream::new([FakeChunk::Pending]);
    let outcome = drive_stream(
        &mut stream,
        openai_chat::classify_event,
        None,
        Duration::from_secs(60),
        Duration::from_millis(10),
        &cancellation,
        None,
    )
    .await;
    assert_eq!(outcome.end, StreamEnd::Cancelled);
}

#[tokio::test(flavor = "current_thread")]
async fn run_deadline_ends_a_hung_stream() {
    let deadline = Instant::now() + Duration::from_millis(60);
    let mut stream = ScriptedStream::new([FakeChunk::Pending]);
    let outcome = drive_stream(
        &mut stream,
        openai_chat::classify_event,
        Some(deadline),
        Duration::from_secs(60),
        Duration::from_millis(10),
        &RunCancellationToken::new(),
        None,
    )
    .await;
    assert_eq!(outcome.end, StreamEnd::DeadlineExceeded);
}

#[tokio::test(flavor = "current_thread")]
async fn idle_timeout_ends_a_silent_stream() {
    // Independent of any caller deadline, a stream that stops producing
    // bytes fails rather than hanging the run.
    let mut stream = ScriptedStream::new([FakeChunk::Pending]);
    let outcome = drive_stream(
        &mut stream,
        openai_chat::classify_event,
        None,
        Duration::from_millis(60),
        Duration::from_millis(10),
        &RunCancellationToken::new(),
        None,
    )
    .await;
    assert!(matches!(outcome.end, StreamEnd::Failed(_)));
}

#[tokio::test(flavor = "current_thread")]
async fn the_model_observed_on_the_wire_is_reported() {
    let mut stream = ScriptedStream::new([
        FakeChunk::Data(data(
            &serde_json::json!({"model": "llama3:70b", "choices": [{"index": 0, "delta": {"content": "hi"}}]}),
        )),
        FakeChunk::Data(done_chunk()),
    ]);
    let outcome = drive_stream(
        &mut stream,
        openai_chat::classify_event,
        None,
        Duration::from_secs(60),
        Duration::from_millis(10),
        &RunCancellationToken::new(),
        None,
    )
    .await;
    assert_eq!(outcome.end, StreamEnd::Completed);
    assert_eq!(outcome.model.as_deref(), Some("llama3:70b"));
}
