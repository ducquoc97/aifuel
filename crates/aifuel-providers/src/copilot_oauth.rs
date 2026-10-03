//! The `copilot:oauth` execution adapter: one-shot prompt completion over
//! the GitHub Copilot API (`api.githubcopilot.com`), the same surface
//! OmniRoute-style gateways drive directly. It exists so the OAuth token
//! the `copilot` CLI or an editor plugin already minted can execute a run
//! without spawning `copilot` - the CLI integration stays registered
//! separately as `copilot`.
//!
//! Credential boundary: the provider-owned files (`~/.copilot/config.json`,
//! `~/.config/github-copilot/{hosts,apps}.json`) are read only inside
//! `execute`, and only the GitHub OAuth token they hold is used. The OAuth
//! token is exchanged in memory for a short-lived Copilot session token,
//! which is cached until its `expires_at`; nothing is written back to the
//! credential files and no refresh grant is persisted. Material never
//! appears in logs, diagnostics, or errors.

mod credentials;

use crate::agent_execution::ExecutionCapabilities;
use crate::oauth_http::{self, http};
use crate::wire::{openai_chat, stream};
use aifuel_core::{
    AgentExecutionAdapter, AgentIntegrationInfo, AgentRunError, AgentRunOutputHandler,
    AgentSetupGuidance, IntegrationId, ProviderId, ProviderKey, RunCancellationToken, RunRequest,
    RunResult,
};
use credentials::{copilot_config_token, github_store_token};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// The documented GitHub Copilot token-exchange endpoint: a GitHub OAuth
/// token in, a short-lived Copilot API session token out.
const TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";

/// The account-info endpoint used when `v2/token` is not served for the
/// account (some individual accounts answer `404` there): it reports the
/// Copilot API base URL the account's OAuth token may call directly.
const USER_URL: &str = "https://api.github.com/copilot_internal/user";

/// The Copilot API base when neither endpoint reports `endpoints.api`.
const API_FALLBACK: &str = "https://api.githubcopilot.com";

/// How long a direct-OAuth session (the `/user` fallback path, which has
/// no server-reported expiry) may be reused before re-checking.
const FALLBACK_SESSION_SECONDS: u64 = 900;

/// The margin subtracted from a session's `expires_at` so a token is never
/// used up to its last possible second.
const SESSION_EXPIRY_MARGIN_SECONDS: u64 = 60;

/// The relogin hint attached to every credential rejection.
const RELOGIN_HINT: &str = "the stored GitHub OAuth credential was rejected or is missing; sign in again through the copilot CLI or a Copilot editor plugin";

/// The compiled `copilot:oauth` adapter.
pub(crate) static ADAPTER: CopilotOAuthAdapter = CopilotOAuthAdapter {
    token_url: Cow::Borrowed(TOKEN_URL),
    user_url: Cow::Borrowed(USER_URL),
    api_fallback: Cow::Borrowed(API_FALLBACK),
    home: None,
    client: OnceLock::new(),
    session: Mutex::new(None),
};

/// A usable Copilot API session: the bearer the chat endpoint accepts, the
/// account's API base URL, and the second it stops being safe to reuse.
#[derive(Clone)]
struct CopilotSession {
    bearer: String,
    api_base: String,
    expires_at: u64,
}

/// Direct subscription execution against the GitHub Copilot API.
///
/// `home`, the endpoint URLs, and the client follow the same compile-time-
/// constant/test-stub pattern as `codex:oauth`. `session` holds the
/// exchanged token in memory for its `expires_at` so consecutive runs do
/// not re-hit `api.github.com`.
pub(crate) struct CopilotOAuthAdapter {
    token_url: Cow<'static, str>,
    user_url: Cow<'static, str>,
    api_fallback: Cow<'static, str>,
    home: Option<PathBuf>,
    client: OnceLock<reqwest::Client>,
    session: Mutex<Option<CopilotSession>>,
}

impl CopilotOAuthAdapter {
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

    /// The GitHub OAuth token the provider already minted, read from the
    /// provider-owned files in priority order:
    ///
    /// 1. `~/.copilot/config.json` - the first-party CLI's own store
    ///    (`copilotTokens`, keyed `<host>:<login>`; the
    ///    `lastLoggedInUser` entry wins when several are present).
    /// 2. `~/.config/github-copilot/hosts.json` - the editor plugins'
    ///    account store (`<host>: {oauth_token}`).
    /// 3. `~/.config/github-copilot/apps.json` - same shape, the other
    ///    plugin store.
    ///
    /// A file that is absent, unreadable, or holds no token is skipped so
    /// the next source can still yield one.
    fn oauth_token(&self) -> Result<String, AgentRunError> {
        let home = self.home()?;
        if let Some(token) = copilot_config_token(&home.join(".copilot/config.json")) {
            return Ok(token);
        }
        for relative in [
            ".config/github-copilot/hosts.json",
            ".config/github-copilot/apps.json",
        ] {
            if let Some(token) = github_store_token(&home.join(relative)) {
                return Ok(token);
            }
        }
        Err(AgentRunError::InvalidRequest(
            "copilot:oauth found no GitHub OAuth token in \
             ~/.copilot/config.json or ~/.config/github-copilot/{hosts,apps}.json; \
             sign in through the copilot CLI (`copilot`, then `/login`) or a \
             Copilot editor plugin"
                .to_owned(),
        ))
    }

    /// The headers `api.github.com` expects on the token exchange and the
    /// `/user` fallback: transport defaults with a JSON accept (not the
    /// SSE accept a chat call sends), the OAuth token under the `token`
    /// scheme marked sensitive, and the GitHub API version the Copilot
    /// endpoints honor.
    fn github_headers(&self, oauth_token: &str) -> Result<HeaderMap, AgentRunError> {
        let mut headers = http::transport_headers();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        http::insert_sensitive(&mut headers, AUTHORIZATION, &format!("token {oauth_token}"))?;
        headers.insert(
            HeaderName::from_static("x-github-api-version"),
            HeaderValue::from_static("2025-04-01"),
        );
        headers.insert(
            HeaderName::from_static("editor-version"),
            HeaderValue::from_static(concat!("aifuel/", env!("CARGO_PKG_VERSION"))),
        );
        Ok(headers)
    }

    /// The cached session, or a fresh exchange. The cache lives for the
    /// process so back-to-back runs do not burn a `v2/token` call each.
    async fn session(
        &self,
        client: &reqwest::Client,
        oauth_token: &str,
        deadline: Option<Instant>,
        cancellation: &RunCancellationToken,
    ) -> Result<CopilotSession, AgentRunError> {
        if let Some(session) = self.session.lock().expect("session mutex").clone()
            && session.expires_at > oauth_http::unix_now() + SESSION_EXPIRY_MARGIN_SECONDS
        {
            return Ok(session);
        }
        let session = self
            .exchange(client, oauth_token, deadline, cancellation)
            .await?;
        *self.session.lock().expect("session mutex") = Some(session.clone());
        Ok(session)
    }

    /// Exchange the GitHub OAuth token for a Copilot session token. An
    /// auth-class rejection (`401`/`403`) or a missing route (`404`/`410`)
    /// means this account may not be served by `v2/token` - business and
    /// enterprise seats reconcile through the `/user` endpoint instead:
    /// its `endpoints.api` names the Copilot API base and the OAuth token
    /// itself is the bearer. `/user` performs the real auth check, so a
    /// genuinely dead credential still maps to the re-login hint there.
    /// Other non-success statuses map onto the shared error surface.
    async fn exchange(
        &self,
        client: &reqwest::Client,
        oauth_token: &str,
        deadline: Option<Instant>,
        cancellation: &RunCancellationToken,
    ) -> Result<CopilotSession, AgentRunError> {
        let response = oauth_http::send(
            client
                .get(self.token_url.as_ref())
                .headers(self.github_headers(oauth_token)?)
                .send(),
            deadline,
            cancellation,
        )
        .await?;
        let status = response.status();
        if status.is_success() {
            let body = response.json::<Value>().await.map_err(|error| {
                AgentRunError::InvalidRequest(format!(
                    "the Copilot token endpoint's response is not JSON: {error}"
                ))
            })?;
            let bearer = body
                .get("token")
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty())
                .ok_or_else(|| {
                    AgentRunError::InvalidRequest(
                        "the Copilot token endpoint's response holds no token".to_owned(),
                    )
                })?
                .to_owned();
            return Ok(CopilotSession {
                bearer,
                api_base: api_base(&body, &self.api_fallback),
                expires_at: body
                    .get("expires_at")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(|| oauth_http::unix_now() + FALLBACK_SESSION_SECONDS),
            });
        }
        if matches!(status.as_u16(), 401 | 403 | 404 | 410) {
            return self
                .user_fallback(client, oauth_token, deadline, cancellation)
                .await;
        }
        Err(AgentRunError::InvalidRequest(format!(
            "the Copilot token endpoint answered HTTP {status}"
        )))
    }

    /// The `/copilot_internal/user` fallback for accounts `v2/token` does
    /// not serve: the response's `endpoints.api` names the Copilot API
    /// base, and the OAuth token itself is the bearer. The session gets a
    /// conservative fixed lifetime since no `expires_at` is reported.
    async fn user_fallback(
        &self,
        client: &reqwest::Client,
        oauth_token: &str,
        deadline: Option<Instant>,
        cancellation: &RunCancellationToken,
    ) -> Result<CopilotSession, AgentRunError> {
        let response = oauth_http::send(
            client
                .get(self.user_url.as_ref())
                .headers(self.github_headers(oauth_token)?)
                .send(),
            deadline,
            cancellation,
        )
        .await?;
        let status = response.status();
        if matches!(status.as_u16(), 401 | 403) {
            return Err(AgentRunError::InvalidRequest(format!(
                "{RELOGIN_HINT} (the account endpoint answered HTTP {status})"
            )));
        }
        if !status.is_success() {
            return Err(AgentRunError::InvalidRequest(format!(
                "the Copilot account endpoint answered HTTP {status}"
            )));
        }
        let body = response.json::<Value>().await.map_err(|error| {
            AgentRunError::InvalidRequest(format!(
                "the Copilot account endpoint's response is not JSON: {error}"
            ))
        })?;
        Ok(CopilotSession {
            bearer: oauth_token.to_owned(),
            api_base: api_base(&body, &self.api_fallback),
            expires_at: oauth_http::unix_now() + FALLBACK_SESSION_SECONDS,
        })
    }

    /// The headers the Copilot chat endpoint expects from a first-party
    /// client: the session bearer marked sensitive, the editor identity
    /// fields GitHub's routing checks, the integration id of the CLI whose
    /// credential file is being read, the intent classifier, and the
    /// request's initiator. The fingerprint-completeness fields some
    /// gateways synthesize (`x-client-machine-id`, `x-vscode-user`,
    /// repository sentinels) are deliberately absent - a missing field is
    /// an honest signal, a forged one is not.
    fn chat_headers(bearer: &str) -> Result<HeaderMap, AgentRunError> {
        let mut headers = http::transport_headers();
        http::insert_sensitive(&mut headers, AUTHORIZATION, &format!("Bearer {bearer}"))?;
        headers.insert(
            HeaderName::from_static("editor-version"),
            HeaderValue::from_static(concat!("aifuel/", env!("CARGO_PKG_VERSION"))),
        );
        headers.insert(
            HeaderName::from_static("copilot-integration-id"),
            HeaderValue::from_static("copilot-developer-cli"),
        );
        headers.insert(
            HeaderName::from_static("x-github-api-version"),
            HeaderValue::from_static("2025-04-01"),
        );
        headers.insert(
            HeaderName::from_static("openai-intent"),
            HeaderValue::from_static("conversation-agent"),
        );
        headers.insert(
            HeaderName::from_static("x-initiator"),
            HeaderValue::from_static("user"),
        );
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
        let oauth_token = self.oauth_token()?;
        let client = self.client()?;
        let session = self
            .session(client, &oauth_token, deadline, cancellation)
            .await?;
        let model = request.model.as_deref().expect("validate requires a model");
        let url = openai_chat::completions_url(&session.api_base);
        let body = openai_chat::request_body(model, &request.prompt);
        let headers = Self::chat_headers(&session.bearer)?;
        let secrets = [session.bearer.as_str(), oauth_token.as_str()];
        let mut response = oauth_http::send(
            client.post(&url).headers(headers).json(&body).send(),
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
            openai_chat::classify_event,
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

/// The API base a Copilot endpoint reports through `endpoints.api`, or the
/// public default when absent.
fn api_base(body: &Value, fallback: &str) -> String {
    body.get("endpoints")
        .and_then(|endpoints| endpoints.get("api"))
        .and_then(Value::as_str)
        .filter(|base| !base.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

impl AgentExecutionAdapter for CopilotOAuthAdapter {
    fn integration(&self) -> IntegrationId {
        IntegrationId::new("copilot:oauth")
    }

    fn provider(&self) -> ProviderId {
        ProviderId::from(ProviderKey::Copilot)
    }

    fn setup_guidance(&self) -> Option<AgentSetupGuidance> {
        Some(AgentSetupGuidance {
            install: "npm install -g @github/copilot",
            login: "Start `copilot`, then enter `/login` in its interactive UI.",
            check: "Run `copilot --version` to check the installed version; AI Fuel does not inspect Copilot sign-in state.",
            documentation_url: "https://docs.github.com/en/copilot/get-started/cli-quickstart",
        })
    }

    fn declared_agent_capabilities(
        &self,
    ) -> BTreeMap<aifuel_core::AgentCapability, aifuel_core::AgentCapabilityEvidence> {
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
