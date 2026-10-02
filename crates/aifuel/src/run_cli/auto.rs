//! `aifuel run --provider auto`: rank the Discovered Providers by quota
//! headroom - the shared `StatusReport::route_candidates` ordering, most
//! remaining allowance in the authoritative window first and soonest reset
//! as tie-break - and run the prompt on the best one. Fallback starts a
//! fresh Agent Run on the next ranked Provider only while a failure stayed
//! before execution: no verified Agent Integration, a launch/request
//! error, or provider-reported quota exhaustion. A run that reached its
//! provider - a timeout or an ordinary mid-run failure - ends the chain,
//! and a resumed session is pinned to its owning Provider so `auto` never
//! falls back away from it.
//!
//! API-key integrations join the chain too: one whose credential is
//! present - a declared environment variable or a stored managed
//! credential - is a valid candidate even when it carries no quota
//! evidence. They rank between the headroom-positive quota providers and
//! the no-headroom tail, and a provider whose catalog entry documents a
//! free tier outranks a paid key.

use super::{ParsedRunRequest, render_run_result_with_sources};
use crate::launcher;
use aifuel_core::{
    ApiKeySource, AuthBinding, ExecutionConfig, IntegrationId, ManagedRunResult, ProviderId,
    ProviderKey, RunState, RunStatus,
};
use std::collections::BTreeSet;

/// The evidence a chain candidate's position rests on, reported on the
/// `routing` object so the ranking is explainable rather than positional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouteBasis {
    /// Ranked by a quota observation - `remaining_percent` records the
    /// measured headroom (absent when collection reported nothing).
    Quota,
    /// A keyed API-key integration whose catalog provider documents a
    /// free tier.
    FreeTier,
    /// A keyed API-key integration with no documented free tier.
    ApiKey,
}

impl RouteBasis {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Quota => "quota",
            Self::FreeTier => "free_tier",
            Self::ApiKey => "api_key",
        }
    }
}

/// One ranked chain entry: the Provider its attempt reports, the
/// Integration Identity it binds, and the basis that placed it.
struct RouteCandidate {
    provider: ProviderId,
    integration: IntegrationId,
    basis: RouteBasis,
    /// The measured remaining-allowance percent a `quota` candidate
    /// ranked on; `None` for unmeasured quota providers and for
    /// credential-ranked candidates.
    remaining_percent: Option<f64>,
}

/// One chain candidate as emitted on the `routing` object: provider and
/// integration plus the rank basis (`quota`, `free_tier`, `api_key`, or
/// `session` when a resume pins the session's owner without ranking).
#[derive(Debug, serde::Serialize)]
pub(super) struct RouteCandidateReport {
    provider: String,
    integration: String,
    basis: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    remaining_percent: Option<f64>,
}

impl From<&RouteCandidate> for RouteCandidateReport {
    fn from(candidate: &RouteCandidate) -> Self {
        Self {
            provider: candidate.provider.as_str().to_owned(),
            integration: candidate.integration.as_str().to_owned(),
            basis: candidate.basis.as_str(),
            remaining_percent: candidate.remaining_percent,
        }
    }
}

/// How one attempt in the chain ended, recorded on the result's `routing`
/// object so scripts can see every fallback.
#[derive(Debug, serde::Serialize)]
pub(super) struct RouteAttempt {
    provider: String,
    integration: String,
    /// `succeeded`, `failed`, `timed_out`, `cancelled`,
    /// `unsupported_integration`, `launch_error`, or `quota_exhausted`.
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

/// The routing decision a `run --provider auto` invocation made, emitted
/// as `routing` on structured output. `selected` names the Provider whose
/// attempt produced the reported outcome; `candidates` is the ranked list
/// the chain was allowed to try, each with the basis that placed it.
#[derive(Debug, Default, serde::Serialize)]
pub(super) struct RouteReport {
    pub requested: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<RouteCandidateReport>,
    pub resumed: bool,
    #[serde(default)]
    pub attempts: Vec<RouteAttempt>,
}

impl RouteReport {
    fn new() -> Self {
        Self {
            requested: aifuel_core::AUTO_PROVIDER,
            ..Self::default()
        }
    }
}

/// How an attempt closed for routing purposes. `Stop` renders the outcome
/// and exits with the code a single-provider run maps to it; `Retry`
/// records the failure and moves to the next ranked Provider.
enum Verdict {
    Stop(u8),
    Retry,
}

pub(super) fn run(selected: &ParsedRunRequest) -> Result<u8, String> {
    let request = &selected.request;
    let mut report = RouteReport::new();

    if let Some(session_id) = request.resume.as_deref() {
        // A session is Provider-scoped: `auto --resume` resolves the
        // stored owner and runs there - one attempt, never a fallback.
        let manager = crate::execution_run_manager()?;
        let stored = manager
            .session_selection(session_id)
            .map_err(|error| error.to_string())?;
        let mut request = request.clone();
        request.integration = stored.integration.clone();
        report.resumed = true;
        report.selected = Some(stored.provider.as_str().to_owned());
        report.candidates = vec![RouteCandidateReport {
            provider: stored.provider.as_str().to_owned(),
            integration: stored.integration.as_str().to_owned(),
            basis: "session",
            remaining_percent: None,
        }];
        eprintln!("aifuel: auto resolved the session to {}", stored.provider);
        return finish(launcher::execute(&request), &request, selected, &mut report);
    }

    let candidates = provider_candidates(request.model.as_deref())?;
    report.candidates = candidates.iter().map(RouteCandidateReport::from).collect();
    if candidates.is_empty() {
        return Err(match request.model.as_deref() {
            Some(model) => format!(
                "'auto' found no Discovered Provider advertising model {model:?}; refresh evidence with `aifuel model refresh` or select a provider explicitly"
            ),
            None => {
                "'auto' found no Discovered Provider or keyed API-key integration; `aifuel status` shows which credential sources are present and `aifuel auth list` shows integration credentials"
                    .to_owned()
            }
        });
    }

    // The chain's last attempt is kept so an exhausted chain still reports
    // its final outcome the way a single-provider run would.
    let mut last_result: Option<ManagedRunResult> = None;
    let mut last_error: Option<(u8, String)> = None;
    for candidate in &candidates {
        if let Some(previous) = report.attempts.last() {
            eprintln!(
                "aifuel: {} could not run ({}); auto trying {}",
                previous.provider,
                previous.detail.as_deref().unwrap_or(previous.outcome),
                candidate.provider
            );
        } else {
            eprintln!("aifuel: auto selected {}", candidate.provider);
        }
        let mut request = request.clone();
        request.integration = candidate.integration.clone();
        match launcher::execute(&request) {
            Ok(result) => {
                let verdict = attempt_verdict(&result);
                report.attempts.push(RouteAttempt {
                    provider: candidate.provider.as_str().to_owned(),
                    integration: candidate.integration.as_str().to_owned(),
                    outcome: attempt_outcome(&result),
                    detail: result.error.clone().or_else(|| result.diagnostics.clone()),
                });
                match verdict {
                    Verdict::Stop(code) => {
                        report.selected = Some(candidate.provider.as_str().to_owned());
                        print!("{}", render(&result, selected, &report)?);
                        return Ok(code);
                    }
                    Verdict::Retry => {
                        last_result = Some(result);
                    }
                }
            }
            Err(error) => {
                let verdict = error_verdict(&error);
                report.attempts.push(RouteAttempt {
                    provider: candidate.provider.as_str().to_owned(),
                    integration: candidate.integration.as_str().to_owned(),
                    outcome: error_outcome(&error),
                    detail: Some(error.to_string()),
                });
                match verdict {
                    Verdict::Retry => {
                        last_error = Some((exit_for_error(&error), error.to_string()));
                    }
                    Verdict::Stop(code) => {
                        report.selected = Some(candidate.provider.as_str().to_owned());
                        eprintln!("aifuel: {error}");
                        emit_routing_failure(selected, &report, &error.to_string());
                        return Ok(code);
                    }
                }
            }
        }
    }

    // Every candidate failed on a retriable cause; the chain reports the
    // last attempt's own terminal outcome.
    if let Some(result) = last_result {
        report.selected = candidates
            .last()
            .map(|candidate| candidate.provider.as_str().to_owned());
        print!("{}", render(&result, selected, &report)?);
        return Ok(4);
    }
    let (code, message) = last_error.expect("an exhausted chain recorded its last failure");
    eprintln!("aifuel: {message}");
    emit_routing_failure(selected, &report, &message);
    Ok(code)
}

/// Render an attempt's result through the shared result formatters with
/// the routing report attached.
fn render(
    result: &ManagedRunResult,
    selected: &ParsedRunRequest,
    report: &RouteReport,
) -> Result<String, String> {
    render_run_result_with_sources(
        result,
        selected.request.output,
        selected.model_evidence,
        Some(&selected.selection_sources),
        (
            selected.request.resume.is_some() && selected.request.model.is_none(),
            selected.request.resume.is_some() && selected.request.effort.is_none(),
        ),
        Some(report),
    )
}

/// Finish a single-attempt path (the resume pin): one result, no
/// fallback - `request` is the resolved request actually attempted.
fn finish(
    outcome: Result<ManagedRunResult, launcher::LaunchError>,
    request: &launcher::RunRequest,
    selected: &ParsedRunRequest,
    report: &mut RouteReport,
) -> Result<u8, String> {
    match outcome {
        Ok(result) => {
            report.attempts.push(RouteAttempt {
                provider: result.provider.as_str().to_owned(),
                integration: result.integration.as_str().to_owned(),
                outcome: attempt_outcome(&result),
                detail: result.error.clone().or_else(|| result.diagnostics.clone()),
            });
            let code = match attempt_verdict(&result) {
                Verdict::Stop(code) => code,
                // A quota-exhausted resumed run cannot move: the session is
                // pinned to its owning Provider, so it reports like a
                // single-provider failure.
                Verdict::Retry => 4,
            };
            print!("{}", render(&result, selected, report)?);
            Ok(code)
        }
        Err(error) => {
            report.attempts.push(RouteAttempt {
                provider: report
                    .selected
                    .clone()
                    .unwrap_or_else(|| aifuel_core::AUTO_PROVIDER.to_owned()),
                integration: request.integration.as_str().to_owned(),
                outcome: error_outcome(&error),
                detail: Some(error.to_string()),
            });
            eprintln!("aifuel: {error}");
            emit_routing_failure(selected, report, &error.to_string());
            Ok(exit_for_error(&error))
        }
    }
}

/// The routing chain for `auto`, ordered by the evidence each candidate
/// rests on:
///
/// 1. Discovered Providers whose quota observation reports positive
///    headroom, in `StatusReport::route_candidates` order.
/// 2. API-key integrations whose credential is present - the declared
///    environment variable set, or a managed credential (or pool member)
///    stored - ranked catalog order, providers with a documented free
///    tier ahead of paid keys.
/// 3. Discovered Providers with no usable headroom (exhausted or
///    unmeasured quota evidence) last; stale evidence may still resolve
///    to a working run.
///
/// `--model` keeps only providers whose cached model catalog advertises
/// the model - API-key candidates carry no catalog advertisement, so a
/// model filter narrows to the quota-ranked catalog providers.
fn provider_candidates(model: Option<&str>) -> Result<Vec<RouteCandidate>, String> {
    let facade = crate::monitoring_facade()?;
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| format!("could not start the status collection runtime: {error}"))?;
    let report = runtime.block_on(facade.status(false));
    let registry = crate::integration_registry()?;
    let credentials = aifuel_providers::CredentialStore::new(crate::aifuel_config_dir()?);
    // A store that cannot be read suppresses every store-backed binding at
    // once; surface that once instead of silently narrowing the chain to
    // environment-provided keys.
    if let Err(error) = credentials.list() {
        eprintln!(
            "aifuel: could not read the credential store ({error}); only environment-provided API keys can be `auto` candidates"
        );
    }

    // `route_candidates` already orders headroom-positive providers ahead
    // of the no-headroom tail; the measured remaining percentage re-exposes
    // that boundary so keyed integrations can slot between the two groups.
    let remaining_of = |key: ProviderKey| {
        report
            .providers
            .iter()
            .find(|usage| usage.key == key)
            .map(|usage| usage.effective_remaining())
            .filter(|remaining| *remaining >= 0.0)
    };
    let (measured, depleted): (Vec<ProviderKey>, Vec<ProviderKey>) = report
        .route_candidates()
        .into_iter()
        .partition(|key| remaining_of(*key).is_some_and(|remaining| remaining > 0.0));

    let mut candidates: Vec<RouteCandidate> = Vec::new();
    candidates.extend(
        measured
            .into_iter()
            .filter_map(|key| bind_provider(&registry, key, remaining_of(key))),
    );
    let (free_tier, paid): (Vec<RouteCandidate>, Vec<RouteCandidate>) = registry
        .list()
        .filter_map(|descriptor| keyed_api_key(descriptor, &credentials))
        .partition(|candidate| candidate.basis == RouteBasis::FreeTier);
    candidates.extend(free_tier);
    candidates.extend(paid);
    candidates.extend(
        depleted
            .into_iter()
            .filter_map(|key| bind_provider(&registry, key, remaining_of(key))),
    );

    if let Some(model) = model {
        let advertised = advertised_providers(model)?;
        candidates.retain(|candidate| {
            candidate
                .provider
                .as_str()
                .parse::<ProviderKey>()
                .is_ok_and(|key| advertised.contains(&key))
        });
    }
    Ok(candidates)
}

/// Bind a quota-ranked Provider to its first registered Integration, in
/// deterministic registry order (built-ins precede configured entries).
fn bind_provider(
    registry: &aifuel_providers::IntegrationRegistry,
    key: ProviderKey,
    remaining_percent: Option<f64>,
) -> Option<RouteCandidate> {
    let provider = ProviderId::from(key);
    registry
        .list()
        .find(|descriptor| *descriptor.provider() == provider)
        .map(|descriptor| RouteCandidate {
            provider,
            integration: descriptor.id().clone(),
            basis: RouteBasis::Quota,
            remaining_percent,
        })
}

/// A registered API-key integration whose credential is present is a
/// candidate on credential evidence alone: it has no quota observation,
/// but its key can run. The basis is `free_tier` when the catalog
/// documents a free allowance for the provider, else `api_key`.
///
/// A cookie-delivered binding is never a candidate: it holds a
/// browser-session credential whose integration exists to monitor, and a
/// `*:web` protocol has no execution engine - candidacy would only
/// manufacture a doomed attempt.
fn keyed_api_key(
    descriptor: &aifuel_providers::IntegrationDescriptor,
    credentials: &aifuel_providers::CredentialStore,
) -> Option<RouteCandidate> {
    let ExecutionConfig::Http {
        auth: AuthBinding::ApiKey { source, delivery },
        ..
    } = &descriptor.integration.execution
    else {
        return None;
    };
    if matches!(delivery, aifuel_core::KeyDelivery::Cookie { .. }) {
        return None;
    }
    if !credential_present(source, credentials) {
        return None;
    }
    let basis = if aifuel_providers::free_tier_note(descriptor.provider().as_str()).is_some() {
        RouteBasis::FreeTier
    } else {
        RouteBasis::ApiKey
    };
    Some(RouteCandidate {
        provider: descriptor.integration.provider.clone(),
        integration: descriptor.integration.id.clone(),
        basis,
        remaining_percent: None,
    })
}

/// Whether an API-key source resolves to present material without reading
/// it - the declared environment variable is set, or a managed credential
/// (or any pool member under its Credential Reference) is stored. Mirrors
/// the evidence check `EvidenceSource::EnvVar`/`ManagedEntry` performs for
/// discovery: presence only, never content, and store errors read as
/// absent (the caller surfaces an unreadable store once).
fn credential_present(
    source: &ApiKeySource,
    credentials: &aifuel_providers::CredentialStore,
) -> bool {
    let stored = |credential: &aifuel_core::CredentialRef| {
        credentials.contains_credential(credential).unwrap_or(false)
    };
    match source {
        ApiKeySource::Env { var } => aifuel_providers::env_override(var).is_some(),
        ApiKeySource::Store { credential } => stored(credential),
        ApiKeySource::EnvOrStore { var, credential } => {
            aifuel_providers::env_override(var).is_some() || stored(credential)
        }
    }
}

/// Providers whose cached model catalog advertises `model`: the catalog
/// marks `advertisement` Supported only for a model the provider itself
/// listed, so a missing cache narrows to no provider rather than guessing.
fn advertised_providers(model: &str) -> Result<BTreeSet<ProviderKey>, String> {
    Ok(crate::run_selection::load_picker_models()?
        .into_iter()
        .filter(|entry| {
            entry.model_id == model
                && entry.advertisement == aifuel_core::CapabilityState::Supported
        })
        .map(|entry| entry.provider)
        .collect())
}

/// Classify a completed attempt: only provider-reported quota exhaustion
/// is retriable after a run was accepted; timeouts and ordinary failures
/// reached execution and stop the chain.
fn attempt_verdict(result: &ManagedRunResult) -> Verdict {
    if result.state == RunState::TimedOut || result.status == Some(RunStatus::Timeout) {
        Verdict::Stop(5)
    } else if result.status == Some(RunStatus::Succeeded) {
        Verdict::Stop(0)
    } else if result.status == Some(RunStatus::Failed) && reported_quota_exhaustion(result) {
        Verdict::Retry
    } else {
        Verdict::Stop(4)
    }
}

/// Classify a failed attempt: a missing Agent Integration and
/// launch/request errors are pre-execution failures the next Provider can
/// still answer; timeouts and owner cancellation are not.
fn error_verdict(error: &launcher::LaunchError) -> Verdict {
    match error {
        launcher::LaunchError::UnsupportedIntegration(_) => Verdict::Retry,
        launcher::LaunchError::Timeout(_) | launcher::LaunchError::Cancelled => {
            Verdict::Stop(exit_for_error(error))
        }
        _ => Verdict::Retry,
    }
}

/// The exit code a single-provider run maps this error to.
fn exit_for_error(error: &launcher::LaunchError) -> u8 {
    match error {
        launcher::LaunchError::UnsupportedIntegration(_) => 3,
        launcher::LaunchError::Timeout(_) => 5,
        _ => 2,
    }
}

/// The attempt outcome label recorded on the routing report.
fn attempt_outcome(result: &ManagedRunResult) -> &'static str {
    if result.state == RunState::TimedOut || result.status == Some(RunStatus::Timeout) {
        "timed_out"
    } else if result.status == Some(RunStatus::Succeeded) {
        "succeeded"
    } else if result.status == Some(RunStatus::Failed) && reported_quota_exhaustion(result) {
        "quota_exhausted"
    } else if result.status == Some(RunStatus::Cancelled) {
        "cancelled"
    } else {
        "failed"
    }
}

/// The outcome label for an attempt that never produced a run.
fn error_outcome(error: &launcher::LaunchError) -> &'static str {
    match error {
        launcher::LaunchError::UnsupportedIntegration(_) => "unsupported_integration",
        launcher::LaunchError::Timeout(_) => "timed_out",
        launcher::LaunchError::Cancelled => "cancelled",
        _ => "launch_error",
    }
}

/// Whether the provider itself reported quota or rate-limit exhaustion:
/// the structured `quota_exhausted` failure category, or exhaustion
/// wording in the run's error surface for providers that report it only
/// as prose.
fn reported_quota_exhaustion(result: &ManagedRunResult) -> bool {
    result.closed_reason.as_deref() == Some("quota_exhausted")
        || result.error.as_deref().is_some_and(provider_quota_wording)
        || result
            .diagnostics
            .as_deref()
            .is_some_and(provider_quota_wording)
}

/// A bounded phrase list matching the ways providers spell quota and
/// rate-limit exhaustion in error text.
fn provider_quota_wording(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    [
        "rate limit",
        "rate-limit",
        "ratelimit",
        "usage limit",
        "quota",
        "too many requests",
        "insufficient_quota",
        "insufficient quota",
        "insufficient credits",
        "http 429",
        "status 429",
        " 429 ",
        "exceeded your current",
    ]
    .iter()
    .any(|phrase| text.contains(phrase))
}

/// When the chain ends without a run to render, structured output modes
/// still report the routing decision on stdout.
fn emit_routing_failure(selected: &ParsedRunRequest, report: &RouteReport, message: &str) {
    match selected.request.output {
        launcher::OutputFormat::Json => println!(
            "{}",
            serde_json::json!({"error": message, "routing": report})
        ),
        launcher::OutputFormat::Jsonl => println!(
            "{}",
            serde_json::json!({"type": "run_error", "error": message, "routing": report})
        ),
        launcher::OutputFormat::Text => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aifuel_core::{ProviderId, RunState};

    fn result(
        status: RunStatus,
        closed_reason: Option<&str>,
        error: Option<&str>,
    ) -> ManagedRunResult {
        ManagedRunResult {
            schema_version: aifuel_core::RUN_MANAGEMENT_SCHEMA_VERSION,
            run_id: "run".to_owned(),
            state: RunState::from(status),
            integration: IntegrationId::new("codex"),
            provider: ProviderId::new("codex"),
            requested_model: None,
            requested_effort: None,
            effective_model: None,
            effective_effort: None,
            local_session_id: None,
            session_id: None,
            status: Some(status),
            closed_reason: closed_reason.map(str::to_owned),
            account_id: None,
            exit_code: Some(1),
            output: None,
            error: error.map(str::to_owned),
            diagnostics: None,
            usage: None,
            content_available: false,
            output_truncated: false,
            diagnostics_truncated: false,
            output_bytes: 0,
            diagnostics_bytes: 0,
        }
    }

    #[test]
    fn provider_reported_quota_failure_is_retriable() {
        // A depleted pool the provider announced is pre-execution
        // evidence: the next ranked provider still has untested allowance.
        let exhausted = result(RunStatus::Failed, Some("quota_exhausted"), None);
        assert!(matches!(attempt_verdict(&exhausted), Verdict::Retry));
        assert_eq!(attempt_outcome(&exhausted), "quota_exhausted");

        let wording = result(
            RunStatus::Failed,
            Some("provider_failed"),
            Some("You have hit your usage limit, try again after reset"),
        );
        assert!(matches!(attempt_verdict(&wording), Verdict::Retry));
        assert_eq!(attempt_outcome(&wording), "quota_exhausted");
    }

    #[test]
    fn ordinary_run_failures_and_timeouts_end_the_chain() {
        // Execution reached the provider: neither a mid-run failure nor a
        // deadline can move to another provider's session.
        let failed = result(
            RunStatus::Failed,
            Some("provider_failed"),
            Some("exit code 1"),
        );
        assert!(matches!(attempt_verdict(&failed), Verdict::Stop(4)));

        let timed_out = result(RunStatus::Timeout, None, Some("agent run timed out"));
        assert!(matches!(attempt_verdict(&timed_out), Verdict::Stop(5)));

        let succeeded = result(RunStatus::Succeeded, None, None);
        assert!(matches!(attempt_verdict(&succeeded), Verdict::Stop(0)));
    }

    #[test]
    fn launch_failures_are_retriable_and_timeouts_are_not() {
        assert!(matches!(
            error_verdict(&launcher::LaunchError::UnsupportedIntegration(
                IntegrationId::new("claude")
            )),
            Verdict::Retry
        ));
        assert!(matches!(
            error_verdict(&launcher::LaunchError::InvalidRequest(
                "the claude binary is missing".to_owned()
            )),
            Verdict::Retry
        ));
        assert!(matches!(
            error_verdict(&launcher::LaunchError::Timeout(
                "the run deadline elapsed".to_owned()
            )),
            Verdict::Stop(5)
        ));
    }

    #[test]
    fn quota_wording_matches_only_exhaustion_phrasing() {
        assert!(provider_quota_wording("the endpoint returned HTTP 429"));
        assert!(provider_quota_wording("You exceeded your current quota"));
        assert!(!provider_quota_wording(
            "the provider exited with exit code 1"
        ));
        assert!(!provider_quota_wording("authentication failed"));
    }

    fn test_store(name: &str) -> (std::path::PathBuf, aifuel_providers::CredentialStore) {
        let dir =
            std::env::temp_dir().join(format!("aifuel-auto-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test dir should be creatable");
        let store = aifuel_providers::CredentialStore::new(&dir);
        (dir, store)
    }

    fn api_key_descriptor(
        provider: &str,
        source: ApiKeySource,
    ) -> aifuel_providers::IntegrationDescriptor {
        aifuel_providers::IntegrationDescriptor::builtin(
            aifuel_core::Integration {
                id: IntegrationId::new(format!("{provider}:api-key")),
                provider: ProviderId::new(provider),
                name: provider.to_owned(),
                execution: ExecutionConfig::Http {
                    endpoint: aifuel_core::EndpointConfig {
                        base_url: "https://endpoint.test".to_owned(),
                        extra_headers: std::collections::BTreeMap::new(),
                        request_timeout_seconds: None,
                    },
                    protocol: aifuel_core::WireApi::OpenAiChat,
                    auth: AuthBinding::ApiKey {
                        source,
                        delivery: aifuel_core::KeyDelivery::Bearer,
                    },
                },
                monitoring: None,
            },
            Vec::new(),
        )
    }

    #[test]
    fn credential_presence_is_metadata_only_over_env_and_store() {
        // Candidacy mirrors the discovery evidence: a set env var or a
        // stored Managed Credential - including a pool member under the
        // bound Credential Reference - counts; nothing reads key material.
        let (dir, store) = test_store("presence");
        let reference = aifuel_core::CredentialRef::new("test:api-key");
        let source = ApiKeySource::EnvOrStore {
            var: "AIFUEL_TEST_CANDIDATE_KEY".to_owned(),
            credential: reference.clone(),
        };

        unsafe { std::env::remove_var("AIFUEL_TEST_CANDIDATE_KEY") };
        assert!(!credential_present(&source, &store));

        unsafe { std::env::set_var("AIFUEL_TEST_CANDIDATE_KEY", "sk-test") };
        assert!(credential_present(&source, &store));
        unsafe { std::env::remove_var("AIFUEL_TEST_CANDIDATE_KEY") };

        store
            .set_api_key(&reference, "sk-stored")
            .expect("store should accept the key");
        assert!(credential_present(&source, &store));

        // A pool member alone - no root credential - still reads as a
        // present credential, matching `contains_credential`.
        let pooled = aifuel_core::CredentialRef::new("test:pooled-key");
        store
            .set_api_key(
                &aifuel_core::CredentialRef::new("test:pooled-key/2"),
                "sk-2",
            )
            .expect("store should accept the pool member");
        assert!(credential_present(
            &ApiKeySource::Store { credential: pooled },
            &store
        ));

        std::fs::remove_dir_all(&dir).expect("test dir should be removable");
    }

    #[test]
    fn keyed_api_key_basis_follows_the_catalog_free_tier() {
        // A documented free tier outranks a paid key because free headroom
        // beats a billed call; an unkeyed integration is no candidate.
        let (dir, store) = test_store("basis");
        unsafe {
            std::env::set_var("AIFUEL_TEST_BASIS_KEY", "sk-test");
        }
        let source = || ApiKeySource::Env {
            var: "AIFUEL_TEST_BASIS_KEY".to_owned(),
        };

        let groq = keyed_api_key(&api_key_descriptor("groq", source()), &store)
            .expect("a keyed integration is a candidate");
        assert_eq!(groq.basis, RouteBasis::FreeTier);
        assert_eq!(groq.provider.as_str(), "groq");
        assert_eq!(groq.integration.as_str(), "groq:api-key");

        let paid = keyed_api_key(&api_key_descriptor("xai", source()), &store)
            .expect("a keyed integration is a candidate");
        assert_eq!(paid.basis, RouteBasis::ApiKey);

        unsafe {
            std::env::remove_var("AIFUEL_TEST_BASIS_KEY");
        }
        assert!(keyed_api_key(&api_key_descriptor("groq", source()), &store).is_none());
        std::fs::remove_dir_all(&dir).expect("test dir should be removable");
    }

    #[test]
    fn cookie_delivered_integrations_are_not_credential_candidates() {
        // A `*:web` session binding holds material but has no execution
        // engine; letting it into `run --provider auto` would produce a
        // doomed attempt, so presence alone must not make it a candidate.
        let (dir, store) = test_store("cookie");
        unsafe {
            std::env::set_var("AIFUEL_TEST_SESSION_KEY", "session-material");
        }
        let mut descriptor = api_key_descriptor(
            "claude-web",
            ApiKeySource::EnvOrStore {
                var: "AIFUEL_TEST_SESSION_KEY".to_owned(),
                credential: aifuel_core::CredentialRef::new("claude-web:web"),
            },
        );
        let ExecutionConfig::Http { auth, .. } = &mut descriptor.integration.execution else {
            panic!("the fixture builds an Http integration");
        };
        *auth = AuthBinding::ApiKey {
            source: match auth {
                AuthBinding::ApiKey { source, .. } => source.clone(),
                _ => unreachable!(),
            },
            delivery: aifuel_core::KeyDelivery::Cookie {
                name: "sessionKey".to_owned(),
            },
        };
        assert!(keyed_api_key(&descriptor, &store).is_none());
        unsafe {
            std::env::remove_var("AIFUEL_TEST_SESSION_KEY");
        }
        std::fs::remove_dir_all(&dir).expect("test dir should be removable");
    }

    #[test]
    fn non_api_key_integrations_are_not_credential_candidates() {
        // CLI integrations keep their quota/discovery candidacy; a binding
        // without an API key never enters the credential tier.
        let (_dir, store) = test_store("non-api-key");
        let cli = aifuel_providers::IntegrationDescriptor::builtin(
            aifuel_core::Integration {
                id: IntegrationId::new("codex"),
                provider: ProviderId::new("codex"),
                name: "codex".to_owned(),
                execution: ExecutionConfig::Cli {
                    adapter: aifuel_core::CliAdapterId::new("codex"),
                },
                monitoring: None,
            },
            Vec::new(),
        );
        assert!(keyed_api_key(&cli, &store).is_none());
        std::fs::remove_dir_all(&_dir).expect("test dir should be removable");
    }
}
