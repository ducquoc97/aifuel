//! Wire-protocol Agent Run adapters: Agent Runs executed as direct HTTP
//! requests against a configured endpoint, per
//! `docs/specs/provider-integrations.md`.
//!
//! The served surface is `WireApi::OpenAiChat` and
//! `WireApi::AnthropicMessages`: one streamed request per run. Honest
//! limits of this path:
//!
//! - AI Fuel executes no provider-side tools, so read-only access is
//!   honestly enforceable and workspace-write access is never claimed.
//! - A request is never auto-replayed: an ambiguous disconnect may already
//!   have been billed, so a transport failure ends the run as failed.
//! - A stream ending (EOF) before a terminal event, `[DONE]` or a finish
//!   reason, is a failure rather than a silent success; partial output is
//!   preserved.

mod anthropic_messages;
mod capabilities;
mod http;
mod openai_chat;
mod pool;
pub(crate) mod sse;
mod stream;

use crate::integrations::InstanceDescriptor;
use crate::{ApiKeyState, CredentialStore, ResolvedAuth};
use aifuel_core::{
    AgentCapability, AgentCapabilityEvidence, AgentExecutionAdapter, AgentRunError,
    AgentRunOutputHandler, AuthBinding, EndpointConfig, ExecutionConfig, ExecutionMode,
    Integration, IntegrationId, ProviderId, RunCancellationToken, RunRequest, RunResult, RunStatus,
    WireApi,
};
use pool::{KeyAttempt, merge_notes};
use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use stream::{StreamEnd, drive_stream, read_bounded_body};

/// The longest a stream may produce no bytes before the run fails. This is
/// independent of the caller's run deadline: a stalled stream is a broken
/// run, and waiting indefinitely lets a dead connection hang the caller.
/// The bound is generous because reasoning models can pause between tokens.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// How often the send and stream loops wake to check cancellation and the
/// run deadline.
const POLL_TICK: Duration = Duration::from_millis(50);

/// Whether this build compiles an execution engine for `protocol`. The
/// served set is `OpenAiChat` and `AnthropicMessages`; config validation
/// rejects other protocols rather than registering an integration that can
/// never execute.
pub fn serves(protocol: WireApi) -> bool {
    matches!(protocol, WireApi::OpenAiChat | WireApi::AnthropicMessages)
}

/// The failures constructing a [`WireExecutionAdapter`] can report. None of
/// them carry credential material.
#[derive(Debug)]
pub enum WireAdapterError {
    /// The integration's execution configuration is not an
    /// `ExecutionConfig::Http` binding for a served Wire Api.
    IncompatibleExecution(String),
    /// The endpoint or authentication configuration cannot produce a
    /// well-formed request: an unparseable base URL, a non-http(s) scheme,
    /// or an invalid header name or value.
    InvalidConfiguration(String),
    /// The HTTP client could not be built.
    HttpClient(reqwest::Error),
}

impl std::fmt::Display for WireAdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IncompatibleExecution(detail) => f.write_str(detail),
            Self::InvalidConfiguration(detail) => {
                write!(f, "invalid wire endpoint configuration: {detail}")
            }
            Self::HttpClient(error) => {
                write!(f, "could not create the HTTP client: {error}")
            }
        }
    }
}

impl std::error::Error for WireAdapterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::HttpClient(error) => Some(error),
            _ => None,
        }
    }
}

/// An [`AgentExecutionAdapter`] serving one HTTP Provider Integration over
/// a compiled Wire Api engine.
///
/// One adapter is constructed per configured integration; it never falls
/// back to another integration and never replays a request whose outcome is
/// ambiguous. Within an `AuthBinding::ApiKey` it rotates through the Key
/// Pool: a `429` cools the key down (`Retry-After` honored, else a modest
/// exponential step) and the request continues with the next healthy
/// member, and a `401`/`403` marks the key invalid. Persisted health makes
/// a cooled key survive restarts; a success clears the member's state.
/// An adapter built by [`Self::for_instance`] serves the instance's
/// selector id with the base integration's endpoint and a rebound
/// Authentication Binding; the pool then rotates under the instance's own
/// Credential Reference.
pub struct WireExecutionAdapter {
    /// The id `integration()` reports and `request.integration` must equal:
    /// the instance's selector id for instance adapters, else the base.
    integration: IntegrationId,
    provider: ProviderId,
    endpoint: EndpointConfig,
    protocol: WireApi,
    auth: AuthBinding,
    /// The identity the credential destination check runs under: the base
    /// integration for an inherited binding, the instance id when the
    /// instance rebinds the credential slot to its own Managed Credential.
    auth_identity: IntegrationId,
    /// The instance overlay this adapter serves, when built for one. The
    /// env spec resolves lazily per run - construction stays side-effect
    /// free.
    instance: Option<InstanceDescriptor>,
    credentials: CredentialStore,
    client: std::sync::OnceLock<reqwest::Client>,
}

impl WireExecutionAdapter {
    /// An adapter for one HTTP integration over a served [`WireApi`].
    /// A protocol with no compiled engine is rejected rather than
    /// approximated.
    ///
    /// `credentials` resolves the [`AuthBinding`] per run; it is a cheap
    /// handle rooted at the AI Fuel config directory, not retained
    /// material.
    pub fn new(
        integration: IntegrationId,
        provider: ProviderId,
        endpoint: EndpointConfig,
        protocol: WireApi,
        auth: AuthBinding,
        credentials: CredentialStore,
    ) -> Result<Self, WireAdapterError> {
        if !serves(protocol) {
            return Err(WireAdapterError::IncompatibleExecution(format!(
                "integration {integration} speaks wire protocol {protocol:?}, which has no \
                 execution engine in this build"
            )));
        }
        http::validate_configuration(&endpoint, &auth)?;
        Ok(Self {
            auth_identity: integration.clone(),
            integration,
            provider,
            endpoint,
            protocol,
            auth,
            instance: None,
            credentials,
            client: std::sync::OnceLock::new(),
        })
    }

    /// The HTTP client, built on first use. Adapter construction also serves
    /// listing, authentication inspection, and validation, none of which
    /// should pay TLS setup for a request that may never be sent.
    fn client(&self) -> Result<&reqwest::Client, AgentRunError> {
        if let Some(client) = self.client.get() {
            return Ok(client);
        }
        let client = http::build_client().map_err(|error| {
            AgentRunError::Io(io::Error::other(WireAdapterError::HttpClient(error)))
        })?;
        Ok(self.client.get_or_init(|| client))
    }

    /// An adapter for the HTTP execution configuration of one Provider
    /// Integration. CLI integrations and Wire Api protocols with no
    /// compiled engine are rejected rather than approximated.
    pub fn from_integration(
        integration: &Integration,
        credentials: CredentialStore,
    ) -> Result<Self, WireAdapterError> {
        match &integration.execution {
            ExecutionConfig::Http {
                endpoint,
                protocol,
                auth,
            } => Self::new(
                integration.id.clone(),
                integration.provider.clone(),
                endpoint.clone(),
                *protocol,
                auth.clone(),
                credentials,
            ),
            ExecutionConfig::Cli { .. } => Err(WireAdapterError::IncompatibleExecution(format!(
                "integration {} is a CLI integration; the provider CLI owns its \
                 credential and execution",
                integration.id
            ))),
        }
    }

    /// An adapter serving one Provider Integration instance over its HTTP
    /// base integration: the endpoint stays the base's, while the
    /// instance's `credential` rebinds the Authentication Binding's
    /// credential slot and its `env` spec resolves into the overlay env
    /// vars are read from. Capabilities stay the compiled wire adapter's -
    /// an instance cannot widen them.
    pub fn for_instance(
        integration: &Integration,
        instance: &InstanceDescriptor,
        credentials: CredentialStore,
    ) -> Result<Self, WireAdapterError> {
        let mut adapter = Self::from_integration(integration, credentials)?;
        adapter.auth = instance
            .bound_auth(&adapter.auth)
            .map_err(WireAdapterError::InvalidConfiguration)?;
        adapter.auth_identity = if instance.credential.is_some() {
            instance.id.clone()
        } else {
            integration.id.clone()
        };
        adapter.integration = instance.id.clone();
        adapter.instance = Some(instance.clone());
        Ok(adapter)
    }

    fn execute_sync(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
        output_handler: Option<&dyn AgentRunOutputHandler>,
    ) -> Result<RunResult, AgentRunError> {
        if request.integration != self.integration {
            return Err(AgentRunError::UnsupportedIntegration(
                request.integration.clone(),
            ));
        }
        if request.prompt.trim().is_empty() {
            return Err(AgentRunError::InvalidRequest(
                "prompt must not be empty".to_owned(),
            ));
        }
        capabilities::reject_unsupported(&self.integration, request)?;
        // Credential resolution is blocking file I/O; this synchronous
        // caller context is a permitted place for it, and the resolved
        // material crosses into the async worker by value, not by reference
        // to the store. Resolving for the auth identity enforces each
        // credential's recorded destination binding.
        //
        // An instance's env spec resolves here and nowhere else: a missing
        // Managed Credential fails the run before a request is built, and
        // the overlay feeds pool and session resolution so an env-sourced
        // binding reads the same variables the provider-facing
        // configuration declared. The resolved values never leave this
        // function's frame.
        let env = match &self.instance {
            Some(instance) => instance
                .resolve_env(&self.credentials)
                .map_err(http::auth_error)?,
            None => BTreeMap::new(),
        };
        let attempts = self.resolve_attempts(&env).map_err(http::auth_error)?;
        let request = request.clone();
        let cancellation = cancellation.clone();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                    .map_err(|error| AgentRunError::Io(io::Error::other(error)))?;
                runtime.block_on(self.execute_async(
                    &request,
                    &cancellation,
                    output_handler,
                    attempts,
                ))
            });
            worker.join().map_err(|_| {
                AgentRunError::InvalidRequest("wire execution worker panicked".to_owned())
            })?
        })
    }

    /// Send the request once with `auth` and return the endpoint's
    /// response, whatever its status. The send outcome is never retried on
    /// ambiguity - the request may already have reached the endpoint and
    /// been billed - and cancellation, the run deadline, and the configured
    /// request timeout are polled while the send is in flight.
    async fn request_response(
        &self,
        prepared: &PreparedRequest,
        deadline: Option<Instant>,
        send_timeout: Option<Duration>,
        cancellation: &RunCancellationToken,
    ) -> Result<reqwest::Response, AgentRunError> {
        let send = self
            .client()?
            .post(&prepared.url)
            .headers(prepared.headers.clone())
            .json(&prepared.body)
            .send();
        let mut send = std::pin::pin!(send);
        let attempt_started = Instant::now();
        let mut ticks = tokio::time::interval(POLL_TICK);
        loop {
            let mut outcome = None;
            tokio::select! {
                result = &mut send => outcome = Some(result),
                _ = ticks.tick() => {
                    if cancellation.is_cancelled() {
                        return Err(AgentRunError::Cancelled);
                    }
                    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        return Err(AgentRunError::Timeout(
                            "the run deadline elapsed before the endpoint responded".to_owned(),
                        ));
                    }
                    if let Some(timeout) = send_timeout
                        && attempt_started.elapsed() >= timeout
                    {
                        return Err(AgentRunError::Timeout(format!(
                            "the endpoint did not respond within the configured {}s request timeout",
                            timeout.as_secs()
                        )));
                    }
                }
            }
            if let Some(result) = outcome {
                return result.map_err(|error| AgentRunError::Io(io::Error::other(error)));
            }
        }
    }

    async fn execute_async(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
        output_handler: Option<&dyn AgentRunOutputHandler>,
        mut attempts: Vec<KeyAttempt>,
    ) -> Result<RunResult, AgentRunError> {
        let started_at = Instant::now();
        let deadline = request.timeout.map(|timeout| started_at + timeout);
        let model = request.model.as_deref().expect("validate requires a model");
        let send_timeout = self
            .endpoint
            .request_timeout_seconds
            .map(Duration::from_secs);

        // Pool bookkeeping that outlives one attempt: rotation notes join
        // the run's diagnostics so `aifuel run` output reports that a
        // credential was rotated and why.
        let mut pool_notes: Vec<String> = Vec::new();

        let (mut response, index, prepared) = loop {
            let Some(index) = attempts.iter().position(KeyAttempt::usable) else {
                // Every member cooled or went invalid before this run (the
                // state persists across restarts): sending would just earn
                // another 429, so the run fails fast instead.
                return Err(AgentRunError::InvalidRequest(
                    self.pool_exhausted_message(&attempts),
                ));
            };
            attempts[index].attempted = true;
            // Each pooled key carries its own resolved material, so the
            // request is prepared per attempt: headers embed that key.
            // Credential material that cannot form a valid header can never
            // authenticate; skip it like a rejection rather than blocking
            // healthier pool members.
            let prepared = match PreparedRequest::for_protocol(
                self.protocol,
                &self.endpoint,
                &attempts[index].auth,
                model,
                &request.prompt,
            ) {
                Ok(prepared) => prepared,
                Err(AgentRunError::InvalidRequest(reason)) => {
                    pool_notes.push(format!("a pooled credential was unusable: {reason}"));
                    continue;
                }
                Err(error) => return Err(error),
            };
            let response = match self
                .request_response(&prepared, deadline, send_timeout, cancellation)
                .await
            {
                Ok(response) => response,
                Err(error) => return Err(error),
            };
            let status = response.status();
            if matches!(status.as_u16(), 401 | 403 | 429) {
                self.record_key_failure(
                    &attempts[index],
                    status.as_u16(),
                    response.headers(),
                    &mut pool_notes,
                );
                if attempts.iter().any(KeyAttempt::usable) {
                    pool_notes.push(format!(
                        "the endpoint answered HTTP {status} for a pooled key; \
                         the run rotated to the next healthy key"
                    ));
                    continue;
                }
            }
            break (response, index, prepared);
        };

        let auth = &attempts[index].auth;
        if !response.status().is_success() {
            let status = response.status();
            // Endpoint error bodies reach run diagnostics only after
            // credential material is scrubbed: a hostile endpoint can echo
            // the Authorization header back into its own error body.
            let diagnostics = read_bounded_body(&mut response)
                .await
                .map(|body| http::redact(body, auth));
            // HTTP 402 (payment/quota) and 429 (rate limit) are
            // provider-reported allowance exhaustion, an explicit signal
            // the run failed on the Quota Pool rather than the request. A
            // pooled run that rotated past every member still ends on the
            // last response, so the flag reflects the final answer.
            let quota_exhausted = matches!(status.as_u16(), 402 | 429)
                || diagnostics
                    .as_deref()
                    .is_some_and(|body| body.contains("insufficient_quota"));
            return Ok(self.build_result(
                request,
                RunStatus::Failed,
                false,
                String::new(),
                None,
                Some(format!("the endpoint returned HTTP {status}")),
                merge_notes(diagnostics, pool_notes),
                None,
                quota_exhausted,
            ));
        }

        // A success is fresher evidence than any recorded failure state:
        // clear it so the member resumes full pool duty.
        if let Some(reference) = &attempts[index].reference
            && attempts[index].state != ApiKeyState::default()
            && let Err(error) = self.credentials.clear_key_state(reference)
        {
            pool_notes.push(format!(
                "the failure state of credential {reference} could not be cleared: {error}"
            ));
        }

        let outcome = drive_stream(
            &mut response,
            prepared.classify,
            deadline,
            STREAM_IDLE_TIMEOUT,
            POLL_TICK,
            cancellation,
            output_handler,
        )
        .await;
        let (status, timed_out, error) = match &outcome.end {
            StreamEnd::Completed => (RunStatus::Succeeded, false, None),
            StreamEnd::TruncatedEof => (
                RunStatus::Failed,
                false,
                Some(
                    "the response stream ended before a terminal completion \
                     event; the request was not replayed"
                        .to_owned(),
                ),
            ),
            StreamEnd::Failed(message) => (
                RunStatus::Failed,
                false,
                Some(http::redact(message.clone(), auth)),
            ),
            StreamEnd::Cancelled => (
                RunStatus::Cancelled,
                false,
                Some("agent run was cancelled".to_owned()),
            ),
            StreamEnd::DeadlineExceeded => (
                RunStatus::Timeout,
                true,
                Some("agent run timed out".to_owned()),
            ),
        };
        let diagnostics = outcome
            .truncated
            .then(|| "the answer exceeded the capture bound and was truncated".to_owned());
        Ok(self.build_result(
            request,
            status,
            timed_out,
            outcome.output,
            outcome.model,
            error,
            merge_notes(diagnostics, pool_notes),
            outcome.usage,
            false,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn build_result(
        &self,
        request: &RunRequest,
        status: RunStatus,
        timed_out: bool,
        output: String,
        effective_model: Option<String>,
        error: Option<String>,
        diagnostics: Option<String>,
        usage: Option<aifuel_core::TokenUsage>,
        quota_exhausted: bool,
    ) -> RunResult {
        let run_id = format!(
            "run-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        RunResult {
            local_session_id: format!("session-{run_id}"),
            run_id,
            // The wire protocol carries no resumable session identity.
            session_id: None,
            resumed_from: request.resume.clone(),
            provider_id: self.provider.clone(),
            integration_id: request.integration.clone(),
            requested_model: request.model.clone(),
            requested_effort: request.effort.clone(),
            effective_model,
            effective_effort: None,
            requested_account_id: request.account.clone(),
            account_id: None,
            execution_mode: if request.working_directory.is_some() {
                ExecutionMode::Project
            } else {
                ExecutionMode::PromptOnly
            },
            permission_profile: request.access,
            status,
            exit_code: None,
            output,
            error,
            diagnostics,
            usage,
            timed_out,
            quota_exhausted,
            working_directory: request
                .working_directory
                .clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
        }
    }
}

/// The pieces of one run's request a served Wire Api supplies: where to
/// send it, the headers and JSON body, and the event classifier the stream
/// driver applies. Construction is the single dispatch point so the run
/// path below reads protocol-agnostic.
struct PreparedRequest {
    url: String,
    headers: reqwest::header::HeaderMap,
    body: serde_json::Value,
    classify: stream::Classifier,
}

impl PreparedRequest {
    fn for_protocol(
        protocol: WireApi,
        endpoint: &EndpointConfig,
        auth: &ResolvedAuth,
        model: &str,
        prompt: &str,
    ) -> Result<Self, AgentRunError> {
        Ok(match protocol {
            WireApi::OpenAiChat => Self {
                url: openai_chat::completions_url(&endpoint.base_url),
                headers: openai_chat::request_headers(endpoint, auth)?,
                body: openai_chat::request_body(model, prompt),
                classify: openai_chat::classify_event,
            },
            WireApi::AnthropicMessages => Self {
                url: anthropic_messages::messages_url(&endpoint.base_url),
                headers: anthropic_messages::request_headers(endpoint, auth)?,
                body: anthropic_messages::request_body(model, prompt),
                classify: anthropic_messages::classify_event,
            },
            // Construction (`serves`) guarantees a served protocol.
            other => unreachable!("wire adapter built for unserved protocol {other:?}"),
        })
    }
}

impl AgentExecutionAdapter for WireExecutionAdapter {
    /// The configured integration this adapter serves.
    fn integration(&self) -> IntegrationId {
        self.integration.clone()
    }

    /// The upstream provider this integration executes against.
    fn provider(&self) -> ProviderId {
        self.provider.clone()
    }

    fn declared_agent_capabilities(&self) -> BTreeMap<AgentCapability, AgentCapabilityEvidence> {
        capabilities::declared_capabilities()
    }

    fn validate(&self, request: &RunRequest) -> Result<(), AgentRunError> {
        capabilities::reject_unsupported(&self.integration, request)
    }

    fn execute(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
    ) -> Result<RunResult, AgentRunError> {
        self.execute_sync(request, cancellation, None)
    }

    fn execute_with_output_handler(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
        output_handler: &dyn AgentRunOutputHandler,
    ) -> Result<RunResult, AgentRunError> {
        self.execute_sync(request, cancellation, Some(output_handler))
    }
}

#[cfg(test)]
mod tests;
