//! Shared run machinery for the provider-owned OAuth HTTP execution
//! adapters (`codex:oauth`, `copilot:oauth`, ...).
//!
//! These adapters serve one-shot prompt completion over the provider's own
//! subscription HTTP surface - the same surface OmniRoute-style gateways
//! drive directly instead of spawning the provider CLI. They deliberately
//! reuse the wire adapter's bounded client, SSE driver, and stream verdict
//! vocabulary: the differences are where the credential comes from (a
//! provider-owned file, not the Credential Store) and which upstream
//! protocol the endpoint speaks.
//!
//! Credential boundary, restated per adapter module:
//!
//! - The provider-owned credential file is read only inside `execute`, only
//!   for the serving integration id, and only the fields the request needs.
//!   Listing surfaces report file presence through discovery evidence and
//!   never read contents.
//! - Monitoring reads the same files through its own collectors; the
//!   shared machinery here persists nothing and writes no credential
//!   material back. A rotation-consuming refresh flow is an adapter's own
//!   boundary decision - `codex:oauth` runs one against its provider-owned
//!   file inside `execute`; this module never starts one.
//! - Resolved material is marked sensitive on the wire, scrubbed from
//!   endpoint-controlled error text via [`redact_secrets`], and never
//!   appears in logs, diagnostics, or errors.
//! - The `RunRequest` env overlay is never applied: direct HTTP execution
//!   spawns no provider process for it to affect.

use aifuel_core::{
    AccessMode, AgentAuthenticationEvidence, AgentAuthenticationState, AgentPresenceEvidence,
    AgentPresenceState, AgentRunError, AgentVersionEvidence, ExecutionMode, IntegrationId,
    OutputFormat, ProviderId, RunCancellationToken, RunRequest, RunResult, RunStatus,
};
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) use crate::wire::http;
pub(crate) use crate::wire::stream::{self, StreamEnd};

/// The longest a stream may produce no bytes before the run fails. Same
/// bound the wire adapters use: reasoning models can pause between tokens.
pub(crate) const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// How often the send and stream loops wake to check cancellation and the
/// run deadline.
pub(crate) const POLL_TICK: Duration = Duration::from_millis(50);

/// The bound on sending a request and receiving the response head. A slow
/// endpoint fails the run here rather than holding the whole deadline;
/// streamed body bytes have their own idle bound in `drive_stream`.
pub(crate) const SEND_TIMEOUT: Duration = Duration::from_secs(60);

/// The synchronous `execute` shape the compiled OAuth adapters share: a
/// scoped worker thread owns a single-threaded tokio runtime so the
/// provider async path can borrow the request, the adapter, and the output
/// handler without lifetime gymnastics. Mirrors `wire`'s `execute_sync`.
pub(crate) fn block_on_run<Fut>(
    build: impl FnOnce() -> Fut + Send,
) -> Result<RunResult, AgentRunError>
where
    Fut: std::future::Future<Output = Result<RunResult, AgentRunError>> + Send,
{
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .map_err(|error| AgentRunError::Io(io::Error::other(error)))?;
            runtime.block_on(build())
        });
        worker.join().map_err(|_| {
            AgentRunError::InvalidRequest("oauth execution worker panicked".to_owned())
        })?
    })
}

/// The user's home directory for provider-owned credential files, resolved
/// the same way the discovery context resolves it (`AIFUEL_HOME`, then the
/// platform home). OAuth adapters resolve it at execute time - never at
/// construction - so listing stays side-effect free.
pub(crate) fn resolve_home() -> Result<PathBuf, AgentRunError> {
    crate::DiscoveryContext::from_environment()
        .map(|context| context.home_dir().to_path_buf())
        .map_err(|error| AgentRunError::InvalidRequest(error.to_string()))
}

/// The shared `agent_info` evidence for a compiled OAuth adapter: presence
/// is `Present` because the adapter executes in-process and needs no native
/// executable (credential presence is the discovery layer's evidence, not
/// this field's); version stays `None`; authentication stays `Unknown`
/// because listing never reads credential material or starts an auth flow.
/// Adapters with no executable surface report `Absent` themselves instead.
pub(crate) fn compiled_presence() -> AgentPresenceEvidence {
    AgentPresenceEvidence {
        state: AgentPresenceState::Present,
        reason: "the compiled adapter executes in-process; no native executable is required"
            .to_owned(),
    }
}

/// The version evidence a compiled OAuth adapter reports.
pub(crate) fn compiled_version() -> AgentVersionEvidence {
    AgentVersionEvidence {
        version: None,
        reason:
            "there is no native executable to version; the compiled adapter executes in-process"
                .to_owned(),
    }
}

/// The authentication evidence a compiled OAuth adapter reports: the same
/// reason the compiled CLI adapters give for not probing at listing time.
pub(crate) fn unprobed_authentication() -> AgentAuthenticationEvidence {
    AgentAuthenticationEvidence {
        state: AgentAuthenticationState::Unknown,
        reason: "authentication is not inspected during listing to avoid reading local credentials or starting an auth flow; use provider setup guidance for manual login and checks".to_owned(),
    }
}

/// Request-side capability pre-rejection, mirroring
/// `wire::capabilities::reject_unsupported`: direct HTTP execution runs no
/// provider-side tools, so read-only access is the only honest boundary and
/// every demand beyond a streamed prompt completion fails here, before any
/// credential file is read or request built.
pub(crate) fn reject_unsupported(
    integration: &IntegrationId,
    request: &RunRequest,
) -> Result<(), AgentRunError> {
    if matches!(
        request.access,
        AccessMode::WorkspaceWrite | AccessMode::Full
    ) {
        return Err(AgentRunError::InvalidRequest(format!(
            "{integration} cannot enforce {} access; direct HTTP \
             execution performs no provider-side tools",
            request.access.as_str()
        )));
    }
    if request.external_tools.is_some() {
        return Err(AgentRunError::InvalidRequest(format!(
            "{integration} cannot enforce an exact external MCP tool selection"
        )));
    }
    if request.effort.is_some() {
        return Err(AgentRunError::InvalidRequest(format!(
            "{integration} cannot report a verified effort setting"
        )));
    }
    if request.resume.is_some() {
        return Err(AgentRunError::InvalidRequest(format!(
            "{integration} does not support explicit session continuation"
        )));
    }
    if request.account.is_some() {
        return Err(AgentRunError::InvalidRequest(format!(
            "{integration} does not expose provider account selection"
        )));
    }
    if request.output == OutputFormat::Jsonl {
        return Err(AgentRunError::InvalidRequest(format!(
            "{integration} cannot provide verified JSONL output"
        )));
    }
    Ok(())
}

/// The model requirement for direct endpoints that carry no provider-side
/// default. Adapters with a compiled default (`codex:oauth` serves
/// `DEFAULT_MODEL`) do not call this; endpoints without one (`copilot:oauth`)
/// do, so a bare selector fails before the credential file is read.
pub(crate) fn require_model(
    integration: &IntegrationId,
    request: &RunRequest,
) -> Result<(), AgentRunError> {
    if request
        .model
        .as_deref()
        .is_none_or(|model| model.trim().is_empty())
    {
        return Err(AgentRunError::InvalidRequest(format!(
            "{integration} requires a model: the direct endpoint has no \
             provider-side default"
        )));
    }
    Ok(())
}

/// Send one request and return the response head, polling cancellation,
/// the run deadline, and the send bound while the send is in flight. The
/// request is never replayed: an ambiguous disconnect may already have
/// been billed. Mirrors `wire`'s `request_response`.
pub(crate) async fn send(
    send: impl std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
    deadline: Option<Instant>,
    cancellation: &RunCancellationToken,
) -> Result<reqwest::Response, AgentRunError> {
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
                if attempt_started.elapsed() >= SEND_TIMEOUT {
                    return Err(AgentRunError::Timeout(format!(
                        "the endpoint did not respond within {}s",
                        SEND_TIMEOUT.as_secs()
                    )));
                }
            }
        }
        if let Some(result) = outcome {
            return result.map_err(|error| AgentRunError::Io(io::Error::other(error)));
        }
    }
}

/// Scrub credential material out of text that will surface in diagnostics.
/// An endpoint-controlled error body can reflect the sent `Authorization`
/// value verbatim; the credential boundary holds even against a hostile
/// endpoint. Same rule `wire::http::redact` applies to managed bindings.
pub(crate) fn redact_secrets(mut text: String, secrets: &[&str]) -> String {
    for secret in secrets {
        if !secret.is_empty() {
            text = text.replace(secret, "<redacted>");
        }
    }
    text
}

/// Map a non-success HTTP status onto the run surface.
///
/// `401`/`403` mean the stored OAuth credential was rejected: that is a
/// request-level problem reported as `InvalidRequest` with the caller's
/// re-authentication hint, never a retry and never a fabricated refresh.
/// `402`/`429` are provider-reported allowance exhaustion and land on
/// `RunResult::quota_exhausted`. Any other status ends the run as failed
/// with a bounded, secret-scrubbed diagnostic body.
pub(crate) async fn http_failure(
    request: &RunRequest,
    provider: &ProviderId,
    response: &mut reqwest::Response,
    secrets: &[&str],
    reauth_hint: &str,
) -> Result<RunResult, AgentRunError> {
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        return Err(AgentRunError::InvalidRequest(format!(
            "{reauth_hint} (the provider answered HTTP {status})"
        )));
    }
    let diagnostics = stream::read_bounded_body(response)
        .await
        .map(|body| redact_secrets(body, secrets));
    let quota_exhausted = matches!(status.as_u16(), 402 | 429);
    Ok(run_result(
        request,
        provider,
        RunStatus::Failed,
        false,
        String::new(),
        None,
        Some(format!("the endpoint returned HTTP {status}")),
        diagnostics,
        None,
        quota_exhausted,
    ))
}

/// The [`RunResult`] assembly the OAuth adapters share, mirroring
/// `WireExecutionAdapter::build_result`: a run minted locally, no
/// provider-side session identity, and working-directory semantics the
/// same as every local adapter.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_result(
    request: &RunRequest,
    provider: &ProviderId,
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
        session_id: None,
        resumed_from: request.resume.clone(),
        provider_id: provider.clone(),
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

/// Map a [`stream::StreamOutcome`]'s end state onto the run surface, the
/// same verdict-to-status mapping `wire` applies.
pub(crate) fn stream_result(
    request: &RunRequest,
    provider: &ProviderId,
    secrets: &[&str],
    outcome: stream::StreamOutcome,
) -> RunResult {
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
            Some(redact_secrets(message.clone(), secrets)),
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
    run_result(
        request,
        provider,
        status,
        timed_out,
        outcome.output,
        outcome.model,
        error,
        diagnostics,
        outcome.usage,
        false,
    )
}

/// The decoded JSON payload of a JWT-shaped token, when the token is one
/// and the payload parses. An unreadable token yields `None` - the
/// endpoint then decides freshness at request time instead of the adapter
/// guessing.
pub(crate) fn jwt_payload(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64url_decode(payload)?;
    serde_json::from_slice(&decoded).ok()
}

/// The `exp` claim of a JWT-shaped access token, when the token is one and
/// the claim parses. An unreadable token yields `None` - the endpoint then
/// decides freshness at request time instead of the adapter guessing.
pub(crate) fn jwt_exp(token: &str) -> Option<u64> {
    jwt_payload(token)?.get("exp")?.as_u64()
}

/// Decode base64url (RFC 4648 section 5, no padding required) without a
/// base64 dependency. Returns `None` on any character outside the alphabet.
pub(crate) fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut value = 0u32;
    let mut bits = 0u32;
    let mut output = Vec::with_capacity(input.len() * 3 / 4);
    for byte in input.bytes() {
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => continue,
            _ => return None,
        } as u32;
        value = (value << 6) | digit;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((value >> bits) as u8);
        }
    }
    Some(output)
}

/// A fresh UUID-shaped session id (v4 layout from process id, time, and a
/// counter; uniqueness is all the header needs - no cryptographic
/// requirement).
pub(crate) fn mint_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut high = (nanos as u64) ^ ((std::process::id() as u64) << 32);
    let mut low = (nanos >> 64) as u64 ^ counter.wrapping_mul(0x9e3779b97f4a7c15);
    // Force the version-4 and variant-10 bits so the id reads as a UUID.
    high = (high & !0xf000) | 0x4000;
    low = (low & !(3u64 << 62)) | (2u64 << 62);
    let (a, b, c, d, e) = (
        (high >> 32) as u32,
        (high >> 16) as u16,
        high as u16,
        (low >> 48) as u16,
        low & 0xffffffffffff,
    );
    format!("{a:08x}-{b:04x}-{c:04x}-{d:04x}-{e:012x}")
}

/// The current Unix timestamp in seconds.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests;
