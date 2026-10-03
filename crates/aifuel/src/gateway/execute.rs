//! The shared execution machinery every `/v1` surface drives: resolve the
//! inbound `model` selector to an attempt chain, run the first candidate
//! that accepts the request, and feed answer deltas to the caller's sink,
//! which formats them into its own wire protocol (OpenAI chat, Anthropic
//! messages, Responses events).
//!
//! Failover only happens before the response commits: an attempt that
//! ends without emitting a delta lets the next ranked candidate try the
//! same request. Once the first delta is delivered the attempt owns the
//! response and a mid-run failure is terminal, matching how
//! `run --provider auto` stops the chain once a run reached its provider.

use super::Gateway;
use aifuel_core::{
    AccessMode, AgentExecutionAdapter, AgentRunError, AgentRunOutputHandler, IntegrationId,
    OutputFormat, RunCancellationToken, RunRequest, RunResult, RunStatus, StatusReport,
};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

/// Heartbeat interval on an open SSE stream, so a dead client is detected
/// without waiting for the next token.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// One ranked execution target: the integration to attempt and the model
/// override the inbound selector pinned, if any.
pub(crate) struct Attempt {
    pub(crate) integration: IntegrationId,
    pub(crate) model: Option<String>,
}

/// What the run feeds the sink: an answer delta, or a keepalive tick the
/// sink may turn into an SSE comment when a stream is already committed.
pub(crate) enum Feed<'a> {
    Delta(&'a str),
    Keepalive,
}

/// The sink's verdict on the last feed: `Stop` means the client is gone
/// and the run should be cancelled.
pub(crate) enum Flow {
    Continue,
    Stop,
}

/// How the attempt chain ended. `committed` records whether any delta
/// reached the sink - `false` means the caller may still answer with a
/// normal error response, `true` means the failure must surface on the
/// already-open stream.
pub(crate) enum Outcome {
    /// One attempt succeeded. `text` is the whole answer - the collected
    /// deltas when the adapter streamed, else its terminal output.
    Success { text: String, result: RunResult },
    /// The chain ended in failure, committed or still answerable.
    Failed {
        committed: bool,
        status: u16,
        message: String,
    },
    /// The sink reported a gone client; the in-flight run was cancelled.
    Aborted,
}

/// The two messages a run worker can post back: one answer delta, or the
/// finished run outcome.
enum Event {
    Delta(String),
    Done(Result<RunResult, AgentRunError>),
}

/// Normalized answer deltas from the adapter into the attempt channel.
#[derive(Debug)]
struct DeltaSink(mpsc::Sender<Event>);

impl AgentRunOutputHandler for DeltaSink {
    fn on_output(&self, delta: &str) {
        let _ = self.0.send(Event::Delta(delta.to_owned()));
    }
}

/// Deepest configured-name expansion `resolve_attempts` follows: an
/// alias may point at a combo or another alias, so cycles resolve as a
/// loud 400 instead of recursing forever.
const MAX_ROUTE_DEPTH: usize = 8;

/// Resolve the inbound `model` string to the attempt chain - see the
/// `crate::gateway` module docs for the addressing convention.
pub(crate) fn resolve_attempts(
    gateway: &Gateway,
    model: &str,
    status: &dyn Fn() -> StatusReport,
) -> Result<Vec<Attempt>, (u16, String)> {
    resolve_attempts_at(gateway, model, status, 0)
}

fn resolve_attempts_at(
    gateway: &Gateway,
    model: &str,
    status: &dyn Fn() -> StatusReport,
    depth: usize,
) -> Result<Vec<Attempt>, (u16, String)> {
    if let Some(resolution) = super::routes::resolve(model).map_err(|error| (500, error))? {
        if depth >= MAX_ROUTE_DEPTH {
            return Err((
                400,
                format!("route {model:?} resolves too deeply; check gateway.json for alias cycles"),
            ));
        }
        return match resolution {
            super::routes::Resolution::Alias(substituted) => {
                resolve_attempts_at(gateway, &substituted, status, depth + 1)
            }
            super::routes::Resolution::Combo(selectors) => {
                let mut attempts = Vec::new();
                for selector in &selectors {
                    attempts.extend(resolve_attempts_at(gateway, selector, status, depth + 1)?);
                }
                if attempts.is_empty() {
                    Err((404, format!("combo {model:?} resolved no candidates")))
                } else {
                    Ok(attempts)
                }
            }
        };
    }
    if model == aifuel_core::AUTO_PROVIDER {
        return plan(model, None, status);
    }
    if let Some(filter) = model.strip_prefix("auto/") {
        return plan(model, Some(filter.to_owned()), status);
    }
    if let Some((selector, pinned)) = model.split_once('/') {
        return match gateway.resolve(selector) {
            Ok(integration) => Ok(vec![Attempt {
                integration,
                model: Some(pinned.to_owned()),
            }]),
            Err(AgentRunError::AmbiguousIntegration { provider, .. }) => Err((
                400,
                format!(
                    "{provider} maps to multiple integrations; name an integration id explicitly"
                ),
            )),
            Err(_) => Err((
                404,
                format!("unknown integration or provider {selector:?} in model {model:?}"),
            )),
        };
    }
    match gateway.resolve(model) {
        Ok(integration) => Ok(vec![Attempt {
            integration,
            model: None,
        }]),
        Err(AgentRunError::AmbiguousIntegration { provider, .. }) => Err((
            400,
            format!("{provider} maps to multiple integrations; name an integration id explicitly"),
        )),
        // Not a selector at all: treat the string as a catalog model id and
        // let the planner pick the ranked provider advertising it.
        Err(_) => plan(model, Some(model.to_owned()), status),
    }
}

/// Plan the `auto` chain over a fresh cached status snapshot.
fn plan(
    requested: &str,
    model: Option<String>,
    status: &dyn Fn() -> StatusReport,
) -> Result<Vec<Attempt>, (u16, String)> {
    let report = status();
    let registry = crate::integration_registry().map_err(|error| (500, error.clone()))?;
    let config_dir = crate::aifuel_config_dir().map_err(|error| (500, error.clone()))?;
    let credentials = aifuel_providers::CredentialStore::new(config_dir);
    let candidates = crate::route_planner::provider_candidates(
        model.as_deref(),
        &report,
        &registry,
        &credentials,
    )
    .map_err(|error| (500, error))?;
    if candidates.is_empty() {
        return Err((
            404,
            match &model {
                Some(model) => format!(
                    "{requested:?} found no Discovered Provider or keyed integration advertising {model:?}; refresh evidence with `aifuel model refresh`"
                ),
                None => format!(
                    "{requested:?} found no Discovered Provider or keyed API-key integration; `aifuel status` shows which credential sources are present"
                ),
            },
        ));
    }
    Ok(candidates
        .into_iter()
        .map(|candidate| Attempt {
            integration: candidate.integration,
            model: model.clone(),
        })
        .collect())
}

/// Walk the attempt chain, feeding deltas to `feed`. The caller owns all
/// wire formatting - this only decides which attempt answers and how the
/// chain ends.
pub(crate) fn run(
    gateway: &Gateway,
    attempts: Vec<Attempt>,
    prompt: &str,
    feed: &mut dyn FnMut(Feed<'_>) -> Flow,
) -> Outcome {
    let mut last_failure = "no executable provider candidate".to_owned();
    let mut committed = false;
    let mut attempted = false;
    for attempt in attempts {
        let Some(adapter) = gateway.adapter(&attempt.integration) else {
            last_failure = format!("{} has no execution adapter", attempt.integration);
            continue;
        };
        let run_request = RunRequest {
            integration: attempt.integration.clone(),
            model: attempt.model.clone(),
            effort: None,
            external_tools: None,
            account: None,
            prompt: prompt.to_owned(),
            output: OutputFormat::Text,
            working_directory: None,
            access: AccessMode::ReadOnly,
            resume: None,
            timeout: None,
            env: Default::default(),
            interaction_handler: None,
        };
        if let Err(error) = adapter.validate(&run_request) {
            last_failure = error.to_string();
            continue;
        }
        if attempted {
            eprintln!(
                "aifuel: gateway failing over to {} ({last_failure})",
                attempt.integration
            );
        } else {
            eprintln!("aifuel: gateway selected {}", attempt.integration);
        }
        attempted = true;
        match drive(adapter, run_request, feed, &mut committed) {
            Verdict::Done(outcome) => return outcome,
            Verdict::Retry(reason) => last_failure = reason,
        }
    }
    // Every candidate declined or failed before committing: the request is
    // still answerable with a normal error response.
    Outcome::Failed {
        committed,
        status: if attempted { 502 } else { 404 },
        message: last_failure,
    }
}

/// One attempt's terminal routing decision: `Done` carries the chain
/// outcome; `Retry` records why the next candidate should try.
enum Verdict {
    Done(Outcome),
    Retry(String),
}

/// Run one attempt, forwarding deltas to the sink. A `Flow::Stop` verdict
/// from the sink cancels the run and ends the chain as [`Outcome::Aborted`].
fn drive(
    adapter: Arc<dyn AgentExecutionAdapter>,
    run_request: RunRequest,
    feed: &mut dyn FnMut(Feed<'_>) -> Flow,
    committed: &mut bool,
) -> Verdict {
    let (sender, receiver) = mpsc::channel::<Event>();
    let cancellation = RunCancellationToken::new();
    let worker_cancel = cancellation.clone();
    let sink = DeltaSink(sender.clone());
    let worker = thread::spawn(move || {
        let result = adapter.execute_with_output_handler(&run_request, &worker_cancel, &sink);
        let _ = sender.send(Event::Done(result));
    });

    // `collected` reconstructs the answer for adapters that report their
    // text only through deltas; `saw_delta` keeps a streamed attempt from
    // re-emitting the same text from `RunResult.output`.
    let mut collected = String::new();
    let mut saw_delta = false;
    let verdict = loop {
        match receiver.recv_timeout(KEEPALIVE) {
            Ok(Event::Delta(delta)) => {
                saw_delta = true;
                *committed = true;
                collected.push_str(&delta);
                if let Flow::Stop = feed(Feed::Delta(&delta)) {
                    cancellation.cancel();
                    break Verdict::Done(Outcome::Aborted);
                }
            }
            Ok(Event::Done(result)) => {
                break finish(result, *committed, &collected, saw_delta);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if *committed && matches!(feed(Feed::Keepalive), Flow::Stop) {
                    cancellation.cancel();
                    break Verdict::Done(Outcome::Aborted);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Verdict::Retry("the provider run ended without a result".to_owned());
            }
        }
    };
    let _ = worker.join();
    verdict
}

/// Classify one finished attempt into the routing decision.
fn finish(
    result: Result<RunResult, AgentRunError>,
    committed: bool,
    collected: &str,
    saw_delta: bool,
) -> Verdict {
    match result {
        Ok(result) if result.status == RunStatus::Succeeded => Verdict::Done(Outcome::Success {
            text: if saw_delta {
                collected.to_owned()
            } else {
                result.output.clone()
            },
            result,
        }),
        Ok(result) => {
            let reason = result
                .error
                .clone()
                .or(result.diagnostics.clone())
                .unwrap_or_else(|| format!("the provider run ended as {}", result.status.as_str()));
            if !committed && retriable_result(&result) {
                Verdict::Retry(reason)
            } else {
                Verdict::Done(Outcome::Failed {
                    committed,
                    status: status_for_result(&result),
                    message: reason,
                })
            }
        }
        Err(error) => {
            if !committed && retriable_error(&error) {
                Verdict::Retry(error.to_string())
            } else {
                Verdict::Done(Outcome::Failed {
                    committed,
                    status: status_for_error(&error),
                    message: error.to_string(),
                })
            }
        }
    }
}

/// Whether a completed-but-failed run may move to the next candidate: only
/// provider-reported quota or rate-limit exhaustion, mirroring the `auto`
/// chain's post-execution retry rule.
fn retriable_result(result: &RunResult) -> bool {
    result.quota_exhausted
        || result
            .error
            .as_deref()
            .is_some_and(crate::route_planner::provider_quota_wording)
        || result
            .diagnostics
            .as_deref()
            .is_some_and(crate::route_planner::provider_quota_wording)
}

/// Whether a launch-time error may move to the next candidate: everything
/// except timeouts and cancellation, mirroring `auto`'s error verdicts.
fn retriable_error(error: &AgentRunError) -> bool {
    !matches!(error, AgentRunError::Timeout(_) | AgentRunError::Cancelled)
}

fn status_for_result(result: &RunResult) -> u16 {
    if result.timed_out || result.status == RunStatus::Timeout {
        504
    } else {
        502
    }
}

fn status_for_error(error: &AgentRunError) -> u16 {
    match error {
        AgentRunError::Timeout(_) => 504,
        AgentRunError::UnsupportedIntegration(_) | AgentRunError::AmbiguousIntegration { .. } => {
            404
        }
        AgentRunError::InvalidRequest(_) => 400,
        _ => 502,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed_run(quota: bool, error: &str) -> RunResult {
        RunResult {
            run_id: "r".to_owned(),
            local_session_id: "s".to_owned(),
            session_id: None,
            resumed_from: None,
            provider_id: aifuel_core::ProviderId::new("codex"),
            integration_id: IntegrationId::new("codex"),
            requested_model: None,
            requested_effort: None,
            effective_model: None,
            effective_effort: None,
            requested_account_id: None,
            account_id: None,
            execution_mode: aifuel_core::ExecutionMode::PromptOnly,
            permission_profile: AccessMode::ReadOnly,
            status: RunStatus::Failed,
            exit_code: Some(1),
            output: String::new(),
            error: Some(error.to_owned()),
            diagnostics: None,
            usage: None,
            timed_out: false,
            quota_exhausted: quota,
            working_directory: std::path::PathBuf::new(),
        }
    }

    #[test]
    fn retry_verdicts_mirror_the_auto_chain() {
        // Quota exhaustion - flagged or spelled in provider text - is the
        // only post-execution failure the chain retries; ordinary failures
        // and timeouts end it, and launch errors retry unless they are
        // timeouts or cancellations. This mirrors `auto`'s verdicts so an
        // HTTP caller sees the same failover the CLI user gets.
        assert!(matches!(
            finish(Ok(failed_run(true, "quota exceeded")), false, "", false),
            Verdict::Retry(_)
        ));
        assert!(matches!(
            finish(
                Ok(failed_run(false, "HTTP 429 too many requests")),
                false,
                "",
                false
            ),
            Verdict::Retry(_)
        ));
        assert!(matches!(
            finish(
                Ok(failed_run(false, "the provider exited 1")),
                false,
                "",
                false
            ),
            Verdict::Done(Outcome::Failed {
                committed: false,
                ..
            })
        ));
        assert!(matches!(
            finish(
                Err(AgentRunError::Timeout("late".to_owned())),
                false,
                "",
                false
            ),
            Verdict::Done(Outcome::Failed { .. })
        ));
    }
}
