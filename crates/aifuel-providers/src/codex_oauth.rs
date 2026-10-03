//! The `codex:oauth` execution adapter: one-shot prompt completion over
//! the ChatGPT-subscription Codex backend (`chatgpt.com/backend-api`), the
//! same surface OmniRoute-style gateways drive directly. It exists so a
//! signed-in `~/.codex/auth.json` can execute a run without spawning the
//! `codex` CLI - the CLI integration stays registered separately as `codex`.
//!
//! Credential boundary: `auth.json` is read only inside `execute`, only the
//! `tokens.access_token` and `tokens.account_id` fields are used, and the
//! refresh token is never touched - a rotation-consuming refresh flow is a
//! monitoring concern, not this adapter's. Material never appears in logs,
//! diagnostics, or errors.

use crate::agent_execution::ExecutionCapabilities;
use crate::oauth_http::{self, http};
use crate::wire::stream::{self, DataVerdict};
use aifuel_core::{
    AgentExecutionAdapter, AgentIntegrationInfo, AgentRunError, AgentRunOutputHandler,
    AgentSetupGuidance, IntegrationId, ProviderId, ProviderKey, RunCancellationToken, RunRequest,
    RunResult, TokenUsage,
};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

/// The Codex backend's Responses surface: the endpoint the first-party CLI
/// itself posts to for a ChatGPT-subscription run. It is not the public
/// `api.openai.com` shape - the `instructions`/`input`/`store`/`stream`
/// fields below are what this endpoint's strict schema accepts.
const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

/// The system instruction the backend's strict schema requires. A neutral
/// one is sent rather than none, matching what subscription gateways send.
const INSTRUCTIONS: &str = "You are a ChatGPT agent.";

/// The relogin hint attached to every credential rejection: the file the
/// adapter reads is the one `codex login` writes.
const RELOGIN_HINT: &str = "the stored Codex OAuth credential was rejected or is missing; run `codex login` to re-authenticate";

/// The compiled `codex:oauth` adapter.
pub(crate) static ADAPTER: CodexOAuthAdapter = CodexOAuthAdapter {
    responses_url: Cow::Borrowed(RESPONSES_URL),
    home: None,
    client: OnceLock::new(),
};

/// The OAuth material `~/.codex/auth.json` yields for one run. The
/// `refresh_token` and `id_token` fields are deliberately not read: this
/// adapter consumes what the CLI already resolved and never drives a
/// rotation-consuming refresh itself.
struct CodexCredentials {
    access_token: String,
    account_id: Option<String>,
    /// The JWT `exp` claim when the access token is JWT-shaped.
    expires_at: Option<u64>,
}

/// Direct ChatGPT-subscription execution against the Codex backend.
///
/// `home` is `None` in production (resolved per run through the discovery
/// context so an `AIFUEL_HOME` change takes effect without rebuilding) and
/// pinned to a temporary directory in tests. `responses_url` is likewise a
/// compile-time constant in production and a stub address in tests.
pub(crate) struct CodexOAuthAdapter {
    responses_url: Cow<'static, str>,
    home: Option<PathBuf>,
    client: OnceLock<reqwest::Client>,
}

impl CodexOAuthAdapter {
    fn client(&self) -> Result<&reqwest::Client, AgentRunError> {
        if let Some(client) = self.client.get() {
            return Ok(client);
        }
        let client = http::build_client()
            .map_err(|error| AgentRunError::Io(std::io::Error::other(error)))?;
        Ok(self.client.get_or_init(|| client))
    }

    fn home(&self) -> Result<PathBuf, AgentRunError> {
        match &self.home {
            Some(home) => Ok(home.clone()),
            None => oauth_http::resolve_home(),
        }
    }

    /// Read the OAuth material the run needs. Presence, JSON shape, and the
    /// `tokens` object are validated here - the credential file's
    /// `auth_mode: "apikey"` layout (an `OPENAI_API_KEY` field, no
    /// `tokens`) is a managed API-key credential, not the subscription one
    /// this integration serves, and reads as "not signed in".
    fn credentials(&self) -> Result<CodexCredentials, AgentRunError> {
        let path = self.home()?.join(".codex/auth.json");
        let text = std::fs::read_to_string(&path).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => AgentRunError::InvalidRequest(format!(
                "codex:oauth requires {path:?}; run `codex login` to create it"
            )),
            _ => AgentRunError::Io(error),
        })?;
        let value: Value = serde_json::from_str(&text).map_err(|error| {
            AgentRunError::InvalidRequest(format!(
                "{path:?} is not valid JSON ({error}); run `codex login` to repair it"
            ))
        })?;
        let tokens = value.get("tokens").ok_or_else(|| {
            AgentRunError::InvalidRequest(format!(
                "{path:?} holds no OAuth tokens (auth_mode apikey uses a \
                 managed key instead); run `codex login` for the \
                 subscription flow this integration serves"
            ))
        })?;
        let access_token = tokens
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                AgentRunError::InvalidRequest(format!(
                    "{path:?} holds no access token; run `codex login` to re-authenticate"
                ))
            })?
            .to_owned();
        let account_id = tokens
            .get("account_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        Ok(CodexCredentials {
            expires_at: oauth_http::jwt_exp(&access_token),
            access_token,
            account_id,
        })
    }

    /// The request headers for one run: transport defaults, the OAuth
    /// bearer marked sensitive, and the Codex-subscription metadata the
    /// backend expects from a first-party client - account id when the
    /// token file carries one, the CLI originator claim, the Responses beta
    /// flag, and a fresh `session_id` conversation id per run.
    fn request_headers(credentials: &CodexCredentials) -> Result<HeaderMap, AgentRunError> {
        let mut headers = http::transport_headers();
        http::insert_sensitive(
            &mut headers,
            AUTHORIZATION,
            &format!("Bearer {}", credentials.access_token),
        )?;
        if let Some(account_id) = &credentials.account_id {
            let value = HeaderValue::from_str(account_id).map_err(|_| {
                AgentRunError::InvalidRequest(
                    "the stored Codex account id is not a valid header value".to_owned(),
                )
            })?;
            headers.insert(HeaderName::from_static("chatgpt-account-id"), value);
        }
        headers.insert(
            HeaderName::from_static("originator"),
            HeaderValue::from_static("codex_cli_rs"),
        );
        headers.insert(
            HeaderName::from_static("openai-beta"),
            HeaderValue::from_static("responses=experimental"),
        );
        let session_id = HeaderValue::from_str(&oauth_http::mint_session_id()).map_err(|_| {
            AgentRunError::InvalidRequest("the minted session id is not a header value".to_owned())
        })?;
        headers.insert(HeaderName::from_static("session_id"), session_id);
        Ok(headers)
    }

    async fn execute_async(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
        output_handler: Option<&dyn AgentRunOutputHandler>,
    ) -> Result<RunResult, AgentRunError> {
        let started_at = Instant::now();
        let deadline = request.timeout.map(|timeout| started_at + timeout);
        let credentials = self.credentials()?;
        // A locally-decodable expiry fails before the billed request, with
        // the same re-login hint the endpoint's own 401 would map to.
        if credentials
            .expires_at
            .is_some_and(|exp| oauth_http::unix_now() >= exp)
        {
            return Err(AgentRunError::InvalidRequest(
                "the stored Codex OAuth access token is expired; run \
                 `codex login` to re-authenticate"
                    .to_owned(),
            ));
        }
        let model = request.model.as_deref().expect("validate requires a model");
        let body = request_body(model, &request.prompt);
        let headers = Self::request_headers(&credentials)?;
        let secrets = [
            credentials.access_token.as_str(),
            credentials.account_id.as_deref().unwrap_or(""),
        ];
        let mut response = oauth_http::send(
            self.client()?
                .post(self.responses_url.as_ref())
                .headers(headers)
                .json(&body)
                .send(),
            deadline,
            cancellation,
        )
        .await?;
        if !response.status().is_success() {
            return oauth_http::http_failure(
                request,
                &self.provider(),
                &mut response,
                &secrets,
                RELOGIN_HINT,
            )
            .await;
        }
        let outcome = stream::drive_stream(
            &mut response,
            classify_event,
            deadline,
            oauth_http::STREAM_IDLE_TIMEOUT,
            oauth_http::POLL_TICK,
            cancellation,
            output_handler,
        )
        .await;
        Ok(oauth_http::stream_result(
            request,
            &self.provider(),
            &secrets,
            outcome,
        ))
    }
}

/// The Responses-protocol body the Codex backend accepts: `instructions`
/// and `input` are required by its strict schema, `store: false` keeps the
/// run out of server-side history, and `stream: true` requests the SSE
/// form this adapter reads.
fn request_body(model: &str, prompt: &str) -> Value {
    json!({
        "model": model,
        "instructions": INSTRUCTIONS,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": prompt}],
        }],
        "store": false,
        "stream": true,
    })
}

/// Classify one SSE payload on the Codex backend's Responses stream.
/// `response.output_text.delta` events carry answer text; the terminal
/// `response.completed`/`response.incomplete` event carries the turn's
/// usage and the effective model; `response.failed` and `error` report a
/// mid-stream failure; every other event type (`response.created`,
/// `response.output_item.*`, `response.reasoning_*`, rate-limit notices)
/// is protocol chatter.
fn classify_event(event: Option<&str>, data: &str) -> DataVerdict {
    let trimmed = data.trim();
    if trimmed.is_empty() {
        return DataVerdict::Ignored { model: None };
    }
    let value: Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(error) => {
            return DataVerdict::Failed {
                message: format!("unparseable stream payload: {error}"),
            };
        }
    };
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .or(event)
        .unwrap_or("");
    let response = value.get("response").unwrap_or(&value);
    let model = response
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match kind {
        "response.output_text.delta" => match response.get("delta").and_then(Value::as_str) {
            Some(text) => DataVerdict::Delta {
                text: text.to_owned(),
                terminal: false,
                model,
                usage: None,
            },
            None => DataVerdict::Ignored { model },
        },
        "response.completed" | "response.incomplete" => DataVerdict::Complete {
            model,
            usage: response.get("usage").and_then(token_usage),
        },
        "response.failed" | "error" => DataVerdict::Failed {
            message: failure_message(response),
        },
        _ => DataVerdict::Ignored { model },
    }
}

/// The token accounting a terminal `response.*` event carries, mapped onto
/// the shared usage shape.
fn token_usage(usage: &Value) -> Option<TokenUsage> {
    let usage = TokenUsage {
        input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
    };
    (usage.input_tokens.is_some() || usage.output_tokens.is_some()).then_some(usage)
}

/// The failure detail of a `response.failed` or `error` event: the error
/// object's message when present, else a generic statement.
fn failure_message(response: &Value) -> String {
    response
        .get("error")
        .and_then(|error| error.get("message").and_then(Value::as_str))
        .or_else(|| response.get("message").and_then(Value::as_str))
        .map(str::to_owned)
        .unwrap_or_else(|| "the provider reported a failure mid-stream".to_owned())
}

impl AgentExecutionAdapter for CodexOAuthAdapter {
    fn integration(&self) -> IntegrationId {
        IntegrationId::new("codex:oauth")
    }

    fn provider(&self) -> ProviderId {
        ProviderId::from(ProviderKey::Codex)
    }

    fn setup_guidance(&self) -> Option<AgentSetupGuidance> {
        Some(AgentSetupGuidance {
            install: "npm install -g @openai/codex",
            login: "Run `codex login` and complete the browser sign-in flow.",
            check: "Run `codex --version` to check the install. To inspect local sign-in state, run `codex login status` yourself; AI Fuel does not run auth commands.",
            documentation_url: "https://developers.openai.com/codex/auth",
        })
    }

    fn declared_agent_capabilities(
        &self,
    ) -> BTreeMap<aifuel_core::AgentCapability, aifuel_core::AgentCapabilityEvidence> {
        // Direct HTTP execution runs no provider-side tools: read-only is
        // honestly enforceable, streaming and prompt completion are what
        // the endpoint does, and everything else is unsupported.
        ExecutionCapabilities::new(false, false, false, false)
            .with_read_only()
            .with_streaming()
            .with_prompt_completion()
            .evidence()
    }

    fn agent_info(&self) -> AgentIntegrationInfo {
        AgentIntegrationInfo::from_inspection(
            self.provider(),
            self.integration(),
            oauth_http::compiled_presence(),
            oauth_http::compiled_version(),
            oauth_http::unprobed_authentication(),
            self.declared_agent_capabilities(),
        )
        .with_setup_guidance(self.setup_guidance())
    }

    fn validate(&self, request: &RunRequest) -> Result<(), AgentRunError> {
        if request.integration != self.integration() {
            return Err(AgentRunError::UnsupportedIntegration(
                request.integration.clone(),
            ));
        }
        if request.prompt.trim().is_empty() {
            return Err(AgentRunError::InvalidRequest(
                "prompt must not be empty".to_owned(),
            ));
        }
        oauth_http::reject_unsupported(&self.integration(), request)
    }

    fn execute(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
    ) -> Result<RunResult, AgentRunError> {
        self.validate(request)?;
        oauth_http::block_on_run(|| self.execute_async(request, cancellation, None))
    }

    fn execute_with_output_handler(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
        output_handler: &dyn AgentRunOutputHandler,
    ) -> Result<RunResult, AgentRunError> {
        self.validate(request)?;
        oauth_http::block_on_run(|| self.execute_async(request, cancellation, Some(output_handler)))
    }
}

#[cfg(test)]
mod tests;
