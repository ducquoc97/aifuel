//! The run's event drain: `watch` consumes the session's event stream to a
//! terminal state and `answer` routes one approval through the owner handler
//! and back. [`Drain`] folds what the stream reported into the legacy
//! `RunResult` fields.

use super::interactions::{approval_decision, interaction_request};
use super::{CANCEL_SETTLE, EVENT_POLL, RuntimeExecutionAdapter};
use aifuel_core::{
    AgentCommand, AgentEvent, AgentEventKind::*, AgentRunError, AgentRunOutputHandler,
    ApprovalRequest, ConsumerId, MessageStream, ModelSelection, ReceiptCode, ReceiptOutcome,
    RequestId, RunCancellationToken, RunId, RunOutcome, RunRequest, RunStatus, SessionId,
    TokenUsage,
};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Instant;

impl RuntimeExecutionAdapter {
    /// Consume the session's event stream until the run reaches a terminal
    /// state. Approval Requests block on the owner handler here, inside the
    /// run's causal order; cancellation and the request deadline cancel the
    /// runtime run and bound the wait for its terminal event.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn watch(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
        consumer: &ConsumerId,
        events: &Receiver<AgentEvent>,
        session_id: &SessionId,
        run_id: &RunId,
        output_handler: Option<&dyn AgentRunOutputHandler>,
    ) -> Result<Drain, AgentRunError> {
        let deadline = request.timeout.map(|timeout| Instant::now() + timeout);
        let timed_out = move || deadline.is_some_and(|at| Instant::now() >= at);
        let mut drain = Drain::default();
        let mut cancel_requested = false;
        let mut cancel_deadline: Option<Instant> = None;
        loop {
            if !cancel_requested && (timed_out() || cancellation.is_cancelled()) {
                cancel_requested = true;
                cancel_deadline = Some(Instant::now() + CANCEL_SETTLE);
                self.cancel_run(session_id, run_id, consumer);
            }
            if cancel_deadline.is_some_and(|at| Instant::now() >= at) {
                drain.diagnostics.push(
                    "the adapter never reported the run's outcome after cancellation".to_owned(),
                );
                break Ok(drain.finish(cancel_requested, timed_out(), None));
            }
            let wait = [deadline, cancel_deadline]
                .into_iter()
                .flatten()
                .map(|at| at.saturating_duration_since(Instant::now()))
                .min()
                .unwrap_or(EVENT_POLL)
                .min(EVENT_POLL);
            let event = match events.recv_timeout(wait) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => continue,
                // The consumer channel lives inside the runtime for its
                // lifetime, so a disconnect means the runtime is gone.
                Err(RecvTimeoutError::Disconnected) => {
                    break Ok(drain.finish(cancel_requested, timed_out(), None));
                }
            };
            match event.kind {
                RunStarted {
                    run_id: id,
                    selection,
                } if id == *run_id => {
                    drain.selection = Some(selection);
                }
                MessageDelta {
                    run_id: id,
                    stream,
                    text,
                } if id == *run_id && stream == MessageStream::Assistant => {
                    drain.output.push_str(&text);
                    if let Some(handler) = output_handler {
                        handler.on_output(&text);
                    }
                }
                Error {
                    run_id: Some(id),
                    message,
                    retryable,
                    ..
                } if id == *run_id => {
                    drain.diagnostics.push(message.clone());
                    if retryable {
                        continue;
                    }
                    // A non-retryable run-scoped error is a terminal
                    // outcome: the contract accepts `error` where
                    // `run.completed` would stand.
                    drain.fatal = Some(message);
                    break Ok(drain.finish(cancel_requested, timed_out(), None));
                }
                Error { message, .. } => {
                    // Session-scoped failures stay diagnostic; the session's
                    // own `session.closed` carries the loss.
                    drain.diagnostics.push(message);
                }
                ApprovalRequested {
                    run_id: id,
                    request_id,
                    request: approval,
                } if id == *run_id => {
                    if let Err(error) = self.answer(
                        request,
                        &request_id,
                        &approval,
                        session_id,
                        consumer,
                        cancellation,
                        &mut drain.diagnostics,
                    ) {
                        // The owner's wait ended the run: cancel the runtime
                        // run so the provider call unwinds, then surface the
                        // wait's own error like a compiled adapter's
                        // propagation did.
                        self.cancel_run(session_id, run_id, consumer);
                        return Err(error);
                    }
                }
                RunCompleted {
                    run_id: id,
                    outcome,
                    usage,
                } if id == *run_id => {
                    drain.usage = usage;
                    break Ok(drain.finish(cancel_requested, timed_out(), Some(outcome)));
                }
                SessionClosed { reason } => {
                    if let Some(reason) = reason {
                        drain
                            .diagnostics
                            .push(format!("the provider session ended: {reason}"));
                    }
                    break Ok(drain.finish(cancel_requested, timed_out(), None));
                }
                _ => {}
            }
        }
    }

    /// Route one `approval.requested` through the owner interaction handler
    /// and back through `approval.answer`. A wait the owner ends - cancel,
    /// deadline, or handler failure - propagates so the run fails the way
    /// the owner decided rather than hanging.
    #[allow(clippy::too_many_arguments)]
    fn answer(
        &self,
        request: &RunRequest,
        request_id: &RequestId,
        approval: &ApprovalRequest,
        session_id: &SessionId,
        consumer: &ConsumerId,
        cancellation: &RunCancellationToken,
        diagnostics: &mut Vec<String>,
    ) -> Result<(), AgentRunError> {
        let Some(handler) = &request.interaction_handler else {
            // No owner wired: the request stays pending and only the run's
            // deadline or cancellation unwinds it - the runtime never
            // auto-approves.
            return Ok(());
        };
        let response = handler.interact(interaction_request(request_id, approval), cancellation)?;
        let outcome = self.runtime.dispatch(
            AgentCommand::ApprovalAnswer {
                command_id: self.command_id(),
                session_id: session_id.clone(),
                request_id: request_id.clone(),
                decision: approval_decision(&response),
            },
            consumer,
        );
        if let ReceiptOutcome::Err { code, message } = &outcome.receipt.outcome {
            // `already_resolved` means the answer lost a race the adapter
            // already settled - another consumer's answer or the run ending.
            if *code != ReceiptCode::AlreadyResolved {
                diagnostics.push(format!("the approval answer was rejected: {message}"));
            }
        }
        Ok(())
    }
}

/// What the event drain accumulated for one run: the assistant text stream,
/// session error facts, the committed run selection, and terminal usage.
#[derive(Default)]
pub(super) struct Drain {
    pub(super) output: String,
    pub(super) diagnostics: Vec<String>,
    pub(super) selection: Option<ModelSelection>,
    pub(super) usage: Option<TokenUsage>,
    /// The non-retryable run-scoped error the run ended on, when one exists.
    pub(super) fatal: Option<String>,
    pub(super) terminal: Terminal,
}

/// The terminal state the drain established for the legacy result.
pub(super) struct Terminal {
    pub(super) status: RunStatus,
    pub(super) error: Option<String>,
    pub(super) timed_out: bool,
}

impl Default for Terminal {
    fn default() -> Self {
        Self {
            status: RunStatus::Failed,
            error: None,
            timed_out: false,
        }
    }
}

impl Drain {
    /// The committed selection's model, when it diverged from the request.
    /// `run.started` echoes the selection `session.create` carried, so an
    /// unchanged value claims no separate effective evidence - matching the
    /// compiled adapters, which reported `effective_model` only from
    /// provider-reported fields.
    pub(super) fn effective_model(&self, request: &RunRequest) -> Option<String> {
        let committed = &self.selection.as_ref()?.model;
        if committed.is_empty() || request.model.as_deref() == Some(committed.as_str()) {
            return None;
        }
        Some(committed.clone())
    }

    /// The committed selection's effort, when it diverged from the request.
    pub(super) fn effective_effort(&self, request: &RunRequest) -> Option<String> {
        let committed = self.selection.as_ref()?.effort?;
        if request.effort.as_deref() == Some(committed.as_str()) {
            return None;
        }
        Some(committed.as_str().to_owned())
    }

    /// Fold the observed terminal facts into the legacy status. Local facts
    /// win over provider-reported outcomes: a deadline reached mid-flight is
    /// a `Timeout` even when the cancelled provider then reports `success`,
    /// and an owner cancellation stays `Cancelled` whatever the provider
    /// calls its unwind. A stream that ended without `run.completed` or a
    /// terminal `error` is a failure, per the runtime event contract.
    pub(super) fn finish(
        mut self,
        cancelled: bool,
        timed_out: bool,
        outcome: Option<RunOutcome>,
    ) -> Self {
        self.terminal = if timed_out {
            Terminal {
                status: RunStatus::Timeout,
                error: Some("agent run timed out".to_owned()),
                timed_out: true,
            }
        } else if cancelled {
            Terminal {
                status: RunStatus::Cancelled,
                error: Some("agent run was cancelled".to_owned()),
                timed_out: false,
            }
        } else {
            match outcome {
                Some(RunOutcome::Success) => Terminal {
                    status: RunStatus::Succeeded,
                    error: None,
                    timed_out: false,
                },
                Some(RunOutcome::Cancelled) => Terminal {
                    status: RunStatus::Cancelled,
                    error: Some("agent run was cancelled".to_owned()),
                    timed_out: false,
                },
                _ => {
                    let fallback = if matches!(outcome, Some(RunOutcome::Failed)) {
                        "agent run failed"
                    } else {
                        "the provider session ended before the run completed"
                    };
                    Terminal {
                        status: RunStatus::Failed,
                        error: Some(
                            self.fatal
                                .clone()
                                .or_else(|| self.diagnostics.last().cloned())
                                .unwrap_or_else(|| fallback.to_owned()),
                        ),
                        timed_out: false,
                    }
                }
            }
        };
        self
    }
}
