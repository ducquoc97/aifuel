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
use crate::route_planner::{RouteCandidate, provider_quota_wording};
use aifuel_core::{ManagedRunResult, RunState, RunStatus};

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

/// The routing decision a `run --provider auto` or `run --chain NAME`
/// invocation made, emitted as `routing` on structured output. `selected`
/// names the Provider whose attempt produced the reported outcome;
/// `candidates` is the ordered list the chain was allowed to try, each
/// with the basis that placed it.
#[derive(Debug, Default, serde::Serialize)]
pub(super) struct RouteReport {
    pub requested: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<RouteCandidateReport>,
    pub resumed: bool,
    #[serde(default)]
    pub attempts: Vec<RouteAttempt>,
}

impl RouteReport {
    fn new(requested: String) -> Self {
        Self {
            requested,
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
    let mut routing = RouteReport::new(aifuel_core::AUTO_PROVIDER.to_owned());

    if request.resume.is_some() {
        return resume_pin(request, selected, routing);
    }

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
    let candidates = crate::route_planner::provider_candidates(
        request.model.as_deref(),
        &report,
        &registry,
        &credentials,
    )?;
    routing.candidates = candidates.iter().map(RouteCandidateReport::from).collect();
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
    walk(request, selected, routing, &candidates)
}

/// `aifuel run --chain NAME`: walk the named `providers.json` chain's
/// steps in declared order. The candidates are the file's own ordering -
/// no evidence ranks them - and each step may pin a model. The shared
/// verdicts apply: a failure before provider execution (unsupported
/// integration, launch error, reported quota exhaustion) tries the next
/// step; a run that reached its provider ends the chain.
pub(super) fn run_chain(selected: &ParsedRunRequest, chain_name: &str) -> Result<u8, String> {
    let request = &selected.request;
    let mut routing = RouteReport::new(format!(
        "{}{chain_name}",
        aifuel_core::CHAIN_PROVIDER_PREFIX
    ));

    if request.resume.is_some() {
        return resume_pin(request, selected, routing);
    }

    let registry = crate::integration_registry()?;
    let Some(chain) = registry.chain(chain_name) else {
        let known = registry
            .chains()
            .map(|chain| chain.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(if known.is_empty() {
            format!("chain '{chain_name}' is not defined in providers.json")
        } else {
            format!("chain '{chain_name}' is not defined in providers.json; known chains: {known}")
        });
    };
    let candidates: Vec<RouteCandidate> = chain
        .steps
        .iter()
        .map(|step| {
            // `check_chain` validated the reference at load; `serving`
            // reports the step's provider for the routing surface - an
            // instance step reports its base integration's provider.
            let provider = registry
                .serving(&step.integration)
                .map(|(descriptor, _)| descriptor.provider().clone())
                .unwrap_or_else(|| aifuel_core::ProviderId::new(step.integration.as_str()));
            RouteCandidate {
                provider,
                integration: step.integration.clone(),
                basis: crate::route_planner::RouteBasis::Chain,
                remaining_percent: None,
                model: step.model.clone(),
            }
        })
        .collect();
    routing.candidates = candidates.iter().map(RouteCandidateReport::from).collect();
    walk(request, selected, routing, &candidates)
}

/// A resumed session is Provider-scoped: `--provider auto --resume` and
/// `--chain NAME --resume` resolve the stored owner and run there - one
/// attempt, never a fallback.
fn resume_pin(
    request: &launcher::RunRequest,
    selected: &ParsedRunRequest,
    mut routing: RouteReport,
) -> Result<u8, String> {
    let session_id = request.resume.as_deref().expect("resume is set");
    let manager = crate::execution_run_manager()?;
    let stored = manager
        .session_selection(session_id)
        .map_err(|error| error.to_string())?;
    let mut request = request.clone();
    request.integration = stored.integration.clone();
    routing.resumed = true;
    routing.selected = Some(stored.provider.as_str().to_owned());
    routing.candidates = vec![RouteCandidateReport {
        provider: stored.provider.as_str().to_owned(),
        integration: stored.integration.as_str().to_owned(),
        basis: "session",
        remaining_percent: None,
    }];
    eprintln!(
        "aifuel: {} resolved the session to {}",
        routing.requested, stored.provider
    );
    finish(
        launcher::execute(&request),
        &request,
        selected,
        &mut routing,
    )
}

/// The ordered attempt loop `auto` and named chains share. Each
/// candidate's integration - and for chain steps, its model override -
/// fills the request; the verdicts decide stop versus next candidate.
fn walk(
    request: &launcher::RunRequest,
    selected: &ParsedRunRequest,
    mut routing: RouteReport,
    candidates: &[RouteCandidate],
) -> Result<u8, String> {
    // The chain's last attempt is kept so an exhausted chain still reports
    // its final outcome the way a single-provider run would.
    let mut last_result: Option<ManagedRunResult> = None;
    let mut last_error: Option<(u8, String)> = None;
    for candidate in candidates {
        if let Some(previous) = routing.attempts.last() {
            eprintln!(
                "aifuel: {} could not run ({}); {} trying {}",
                previous.provider,
                previous.detail.as_deref().unwrap_or(previous.outcome),
                routing.requested,
                candidate.provider
            );
        } else {
            eprintln!(
                "aifuel: {} selected {}",
                routing.requested, candidate.provider
            );
        }
        let mut request = request.clone();
        request.integration = candidate.integration.clone();
        if let Some(model) = &candidate.model {
            request.model = Some(model.clone());
        }
        match launcher::execute(&request) {
            Ok(result) => {
                let verdict = attempt_verdict(&result);
                routing.attempts.push(RouteAttempt {
                    provider: candidate.provider.as_str().to_owned(),
                    integration: candidate.integration.as_str().to_owned(),
                    outcome: attempt_outcome(&result),
                    detail: result.error.clone().or_else(|| result.diagnostics.clone()),
                });
                match verdict {
                    Verdict::Stop(code) => {
                        routing.selected = Some(candidate.provider.as_str().to_owned());
                        print!("{}", render(&result, selected, &routing)?);
                        return Ok(code);
                    }
                    Verdict::Retry => {
                        last_result = Some(result);
                    }
                }
            }
            Err(error) => {
                let verdict = error_verdict(&error);
                routing.attempts.push(RouteAttempt {
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
                        routing.selected = Some(candidate.provider.as_str().to_owned());
                        eprintln!("aifuel: {error}");
                        emit_routing_failure(selected, &routing, &error.to_string());
                        return Ok(code);
                    }
                }
            }
        }
    }

    // Every candidate failed on a retriable cause; the chain reports the
    // last attempt's own terminal outcome.
    if let Some(result) = last_result {
        routing.selected = candidates
            .last()
            .map(|candidate| candidate.provider.as_str().to_owned());
        print!("{}", render(&result, selected, &routing)?);
        return Ok(4);
    }
    let (code, message) = last_error.expect("an exhausted chain recorded its last failure");
    eprintln!("aifuel: {message}");
    emit_routing_failure(selected, &routing, &message);
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

/// Classify a completed attempt: provider-reported quota exhaustion
/// retries on the next candidate, and so do adapter rejections recorded
/// as failed runs (`invalid_request`, `agent_unavailable`) - nothing
/// reached the provider, so the chain can still answer. A run that
/// reached execution and failed on its own merits stops the chain.
fn attempt_verdict(result: &ManagedRunResult) -> Verdict {
    if result.state == RunState::TimedOut || result.status == Some(RunStatus::Timeout) {
        Verdict::Stop(5)
    } else if result.status == Some(RunStatus::Succeeded) {
        Verdict::Stop(0)
    } else if result.status == Some(RunStatus::Failed) && never_executed(result) {
        Verdict::Retry
    } else {
        Verdict::Stop(4)
    }
}

/// Whether a failed run record is an adapter rejection rather than a
/// provider outcome: quota exhaustion the provider itself reported, or a
/// launch-stage failure - validation, credential resolution, transport
/// setup - where execution never began.
fn never_executed(result: &ManagedRunResult) -> bool {
    reported_quota_exhaustion(result)
        || matches!(
            result.closed_reason.as_deref(),
            Some("invalid_request" | "agent_unavailable")
        )
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
    } else if result.status == Some(RunStatus::Failed)
        && matches!(
            result.closed_reason.as_deref(),
            Some("invalid_request" | "agent_unavailable")
        )
    {
        "launch_error"
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
    use aifuel_core::{IntegrationId, ProviderId, RunState};

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
    fn adapter_rejections_recorded_as_failures_are_retriable() {
        // A run the adapter rejected before execution - credential
        // resolution, request validation, transport setup - is recorded
        // as Failed with a launch-stage closed_reason; the chain retries
        // because nothing reached the provider.
        let credential = result(
            RunStatus::Failed,
            Some("invalid_request"),
            Some("environment variable 'MISSING' is unset or empty"),
        );
        assert!(matches!(attempt_verdict(&credential), Verdict::Retry));
        assert_eq!(attempt_outcome(&credential), "launch_error");

        let unavailable = result(
            RunStatus::Failed,
            Some("agent_unavailable"),
            Some("connection refused"),
        );
        assert!(matches!(attempt_verdict(&unavailable), Verdict::Retry));
        assert_eq!(attempt_outcome(&unavailable), "launch_error");
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
}
