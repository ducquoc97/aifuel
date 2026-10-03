//! The `codex:oauth` execution adapter: one-shot prompt completion over
//! the ChatGPT-subscription Codex backend (`chatgpt.com/backend-api`), the
//! same surface OmniRoute-style gateways drive directly. It exists so a
//! signed-in `~/.codex/auth.json` can execute a run without spawning the
//! `codex` CLI - the CLI integration stays registered separately as `codex`.
//!
//! Credential boundary: `auth.json` is read only inside `execute`, and only
//! the `tokens` object is used. When the stored access token is expired or
//! inside the refresh margin and `tokens.refresh_token` is present, the
//! adapter spends the refresh grant the same way the first-party `codex`
//! CLI does (`POST https://auth.openai.com/oauth/token`) and writes the
//! rotated set back atomically, preserving every unrelated field. A file
//! whose access token is dead with no refresh grant still reads as
//! "run `codex login`". Material never appears in logs, diagnostics, or
//! errors.

use crate::agent_execution::ExecutionCapabilities;
use crate::oauth_http::{self, http};
use crate::wire::stream::{self, DataVerdict};
use aifuel_core::{
    AgentExecutionAdapter, AgentIntegrationInfo, AgentRunError, AgentRunOutputHandler,
    AgentSetupGuidance, IntegrationId, ProviderId, ProviderKey, RunCancellationToken, RunRequest,
    RunResult, TokenUsage,
};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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

/// The codex-family model the subscription backend currently serves: a
/// bare `codex:oauth` selector runs it, `codex:oauth/<model>` overrides.
const DEFAULT_MODEL: &str = "gpt-6-luna";

/// The relogin hint attached to every credential rejection: the file the
/// adapter reads is the one `codex login` writes.
const RELOGIN_HINT: &str = "the stored Codex OAuth credential was rejected or is missing; run `codex login` to re-authenticate";

/// The OpenAI OAuth token endpoint the first-party `codex` CLI posts the
/// `refresh_token` grant to (`codex-rs` login manager `REFRESH_TOKEN_URL`,
/// `https://github.com/openai/codex/blob/main/codex-rs/login/src/auth/manager.rs`).
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// The codex CLI's public OAuth client id (`codex-rs` login manager
/// `CLIENT_ID`): the audience every ChatGPT-subscription grant is minted
/// under. A stored access token that decodes may name its own `client_id`
/// claim and win over this constant.
const OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// The refresh margin: a stored access token this close to expiry (or past
/// it) is rotated through the token endpoint before the run rather than
/// raced to its last second. A still-valid token is never refreshed - the
/// grant consumes rotation and is not free.
const EXPIRY_MARGIN_SECONDS: u64 = 60;

/// The compiled `codex:oauth` adapter.
pub(crate) static ADAPTER: CodexOAuthAdapter = CodexOAuthAdapter {
    responses_url: Cow::Borrowed(RESPONSES_URL),
    token_url: Cow::Borrowed(TOKEN_URL),
    home: None,
    client: OnceLock::new(),
    refresh_lock: OnceLock::new(),
};

/// The OAuth material `~/.codex/auth.json` yields for one run.
struct CodexCredentials {
    access_token: String,
    account_id: Option<String>,
    /// The refresh grant, spent only when the access token is expired or
    /// inside the refresh margin. Never surfaced in logs or errors.
    refresh_token: Option<String>,
    /// The JWT `exp` claim when the access token is JWT-shaped.
    expires_at: Option<u64>,
}

impl CodexCredentials {
    /// Whether the access token is expired or inside the refresh margin:
    /// the only condition under which the rotation-consuming refresh grant
    /// is spent. A token with no decodable `exp` is never refreshed - the
    /// endpoint decides freshness at request time.
    fn needs_refresh(&self) -> bool {
        self.expires_at
            .is_some_and(|exp| oauth_http::unix_now() + EXPIRY_MARGIN_SECONDS >= exp)
    }
}

/// Direct ChatGPT-subscription execution against the Codex backend.
///
/// `home` is `None` in production (resolved per run through the discovery
/// context so an `AIFUEL_HOME` change takes effect without rebuilding) and
/// pinned to a temporary directory in tests. `responses_url` and
/// `token_url` are likewise compile-time constants in production and stub
/// addresses in tests.
pub(crate) struct CodexOAuthAdapter {
    responses_url: Cow<'static, str>,
    token_url: Cow<'static, str>,
    home: Option<PathBuf>,
    client: OnceLock<reqwest::Client>,
    /// Serializes the refresh exchange and write-back so concurrent runs
    /// spend the rotation-consuming grant once: a second waiter re-reads
    /// the file under the lock and runs with what the first landed.
    refresh_lock: OnceLock<tokio::sync::Mutex<()>>,
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

    /// The provider-owned credential file this adapter reads inside
    /// `execute` - and rewrites atomically when a refresh rotates it.
    fn auth_json_path(&self) -> Result<PathBuf, AgentRunError> {
        Ok(self.home()?.join(".codex/auth.json"))
    }

    /// Read the OAuth material the run needs. Presence, JSON shape, and the
    /// `tokens` object are validated here - the credential file's
    /// `auth_mode: "apikey"` layout (an `OPENAI_API_KEY` field, no
    /// `tokens`) is a managed API-key credential, not the subscription one
    /// this integration serves, and reads as "not signed in".
    fn credentials(&self) -> Result<CodexCredentials, AgentRunError> {
        let path = self.auth_json_path()?;
        credentials_from(&path, &read_auth_json(&path)?)
    }

    /// Rotate the stored token set through the OpenAI OAuth token endpoint:
    /// the `refresh_token` grant the first-party `codex` CLI itself runs
    /// when a stored access token expires. The file is re-read under the
    /// adapter's refresh lock so a set another writer already rotated - a
    /// second gateway run or the CLI - is reused rather than spending the
    /// grant again. Only a successful exchange is written back, atomically
    /// and preserving every unrelated field; a failed exchange touches
    /// nothing on disk.
    async fn refresh(
        &self,
        deadline: Option<Instant>,
        cancellation: &RunCancellationToken,
    ) -> Result<CodexCredentials, AgentRunError> {
        let _guard = self
            .refresh_lock
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let path = self.auth_json_path()?;
        let mut file = read_auth_json(&path)?;
        let credentials = credentials_from(&path, &file)?;
        if !credentials.needs_refresh() {
            return Ok(credentials);
        }
        let refresh_token = credentials.refresh_token.clone().ok_or_else(|| {
            AgentRunError::InvalidRequest(
                "the stored Codex OAuth access token is expired and holds no \
                 refresh token; run `codex login` to re-authenticate"
                    .to_owned(),
            )
        })?;
        let body = json!({
            "client_id": oauth_client_id(&credentials.access_token),
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        });
        // The token endpoint answers plain JSON: transport defaults with
        // the SSE accept swapped for a JSON one, and no bearer - the grant
        // in the body is the whole credential.
        let mut headers = http::transport_headers();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        let response = oauth_http::send(
            self.client()?
                .post(self.token_url.as_ref())
                .headers(headers)
                .json(&body)
                .send(),
            deadline,
            cancellation,
        )
        .await?;
        let status = response.status();
        // A 4xx means the stored grant is dead and the credential boundary
        // answers with the re-login hint; anything else non-success is the
        // endpoint's problem, not the credential's.
        if status.is_client_error() {
            return Err(AgentRunError::InvalidRequest(format!(
                "{RELOGIN_HINT} (the OpenAI token endpoint answered HTTP {status})"
            )));
        }
        if !status.is_success() {
            return Err(AgentRunError::InvalidRequest(format!(
                "the OpenAI token endpoint answered HTTP {status}"
            )));
        }
        let response_body: Value = response.json().await.map_err(|error| {
            AgentRunError::InvalidRequest(format!(
                "the OpenAI token endpoint's response is not JSON: {error}"
            ))
        })?;
        let refreshed = refreshed_tokens(&response_body)?;
        write_refreshed(&path, &mut file, &refreshed)?;
        Ok(CodexCredentials {
            expires_at: oauth_http::jwt_exp(&refreshed.access_token),
            account_id: credentials.account_id,
            refresh_token: refreshed.refresh_token.or(credentials.refresh_token),
            access_token: refreshed.access_token,
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
        let mut credentials = self.credentials()?;
        // A locally-decodable expiry inside the refresh margin triggers the
        // same rotation the first-party CLI runs, before the billed
        // request. A dead credential with no refresh grant still fails with
        // the re-login hint the endpoint's own 401 would map to.
        if credentials.needs_refresh() {
            credentials = self.refresh(deadline, cancellation).await?;
        }
        let model = request
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .unwrap_or(DEFAULT_MODEL);
        let body = request_body(
            model,
            &request.prompt,
            request.optimize.caveman_instruction(),
        );
        let headers = Self::request_headers(&credentials)?;
        let secrets = [
            credentials.access_token.as_str(),
            credentials.refresh_token.as_deref().unwrap_or(""),
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

/// Read and parse the credential file: absence and malformed JSON map onto
/// the relogin surface; other read failures are I/O.
fn read_auth_json(path: &Path) -> Result<Value, AgentRunError> {
    let text = std::fs::read_to_string(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => AgentRunError::InvalidRequest(format!(
            "codex:oauth requires {path:?}; run `codex login` to create it"
        )),
        _ => AgentRunError::Io(error),
    })?;
    serde_json::from_str(&text).map_err(|error| {
        AgentRunError::InvalidRequest(format!(
            "{path:?} is not valid JSON ({error}); run `codex login` to repair it"
        ))
    })
}

/// Extract the fields a run needs from a parsed credential file, with the
/// same rejection messages `credentials` has always produced.
fn credentials_from(path: &Path, value: &Value) -> Result<CodexCredentials, AgentRunError> {
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
    let refresh_token = tokens
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned);
    Ok(CodexCredentials {
        expires_at: oauth_http::jwt_exp(&access_token),
        access_token,
        refresh_token,
        account_id,
    })
}

/// The OAuth client id the refresh grant is posted under: the stored
/// access token's `client_id` claim when it decodes (the client the grant
/// was minted for), an `app_`-shaped `aud` entry next, and the codex
/// CLI's compiled id otherwise. The access token's `aud` normally names
/// the API surface (`https://api.openai.com/v1`), not the OAuth client,
/// so only an `app_`-shaped audience qualifies.
fn oauth_client_id(access_token: &str) -> Cow<'static, str> {
    let Some(payload) = oauth_http::jwt_payload(access_token) else {
        return Cow::Borrowed(OAUTH_CLIENT_ID);
    };
    if let Some(client_id) = payload
        .get("client_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        return Cow::Owned(client_id.to_owned());
    }
    let mut audiences = payload
        .get("aud")
        .and_then(Value::as_str)
        .into_iter()
        .chain(
            payload
                .get("aud")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        );
    audiences
        .find(|audience| audience.starts_with("app_"))
        .map(|audience| Cow::Owned(audience.to_owned()))
        .unwrap_or(Cow::Borrowed(OAUTH_CLIENT_ID))
}

/// The token set a successful refresh exchange returns.
struct RefreshedTokens {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
}

/// Parse the token endpoint's response. `access_token` is required - a
/// success response without one is a dead-credential answer; the other
/// fields update the stored file only when present, matching how the
/// first-party CLI persists a rotation (`persist_tokens` in codex-rs).
fn refreshed_tokens(body: &Value) -> Result<RefreshedTokens, AgentRunError> {
    let access_token = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            AgentRunError::InvalidRequest(format!(
                "the OpenAI token endpoint's response held no access token; {RELOGIN_HINT}"
            ))
        })?;
    let optional = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    Ok(RefreshedTokens {
        access_token: access_token.to_owned(),
        refresh_token: optional("refresh_token"),
        id_token: optional("id_token"),
    })
}

/// Persist a rotated token set: update the fields the exchange returned,
/// stamp `last_refresh` the way the first-party CLI writes it (RFC 3339
/// UTC), and replace the file atomically at mode 0600. Every unrelated
/// field rides through untouched. Called only after a successful exchange,
/// so a failed refresh can never corrupt the credential file.
fn write_refreshed(
    path: &Path,
    file: &mut Value,
    refreshed: &RefreshedTokens,
) -> Result<(), AgentRunError> {
    let Some(root) = file.as_object_mut() else {
        return Err(AgentRunError::InvalidRequest(format!(
            "{path:?} is not a JSON object; run `codex login` to repair it"
        )));
    };
    let Some(tokens) = root.get_mut("tokens").and_then(Value::as_object_mut) else {
        return Err(AgentRunError::InvalidRequest(format!(
            "{path:?} holds no writable OAuth token object; run `codex login` to repair it"
        )));
    };
    tokens.insert("access_token".to_owned(), json!(refreshed.access_token));
    if let Some(refresh_token) = &refreshed.refresh_token {
        tokens.insert("refresh_token".to_owned(), json!(refresh_token));
    }
    if let Some(id_token) = &refreshed.id_token {
        tokens.insert("id_token".to_owned(), json!(id_token));
    }
    root.insert(
        "last_refresh".to_owned(),
        Value::String(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)),
    );
    let bytes = serde_json::to_vec_pretty(&file)
        .map_err(|error| AgentRunError::Io(std::io::Error::other(error)))?;
    crate::credentials::lock::atomic_replace(path, &bytes)
        .map_err(|error| AgentRunError::Io(std::io::Error::other(error)))
}

/// The Responses-protocol body the Codex backend accepts: `instructions`
/// and `input` are required by its strict schema, `store: false` keeps the
/// run out of server-side history, and `stream: true` requests the SSE
/// form this adapter reads. An active optimizer plan appends its system
/// instruction to the required `instructions` field.
fn request_body(model: &str, prompt: &str, system: Option<&str>) -> Value {
    let instructions = match system {
        Some(system) => format!("{INSTRUCTIONS}\n\n{system}"),
        None => INSTRUCTIONS.to_owned(),
    };
    json!({
        "model": model,
        "instructions": instructions,
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
