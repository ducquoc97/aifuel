//! Session state and the run worker behind [`CliAdapter`](super::CliAdapter).
//!
//! A session owns one event channel and serializes Agent Runs on its own
//! worker threads. All run-scoped emissions come from the worker, so event
//! order follows the run's causal order: `run.started` first, streamed
//! deltas and approval events while it runs, one terminal `run.completed`,
//! then the session status. Adapters emit event kinds only; the Session
//! Event Log stamps identity, sequence, and time.

use super::Execution;
use super::interactions::{CliInteractionHandler, PendingInteraction};
use aifuel_core::{
    AccessMode, AgentEventKind, AgentInteractionHandler, AgentRunError, AgentRunOutputHandler,
    AgentRuntimeError, ApprovalDecision, IntegrationId, MessageStream, ModelSelection,
    OutputFormat, ReceiptCode, RequestId, RunCancellationToken, RunId, RunOutcome, RunRequest,
    RunResult, RunStatus, SessionStatus, UserInput,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

/// One live session inside a `CliAdapter`.
pub(super) struct CliSession {
    pub integration: IntegrationId,
    pub cwd: PathBuf,
    pub access: AccessMode,
    /// Shared sender every run worker emits through. Sends that race a
    /// detached receiver are dropped, never panic.
    emit: mpsc::Sender<AgentEventKind>,
    /// Handed to `events()` once; later calls see an already-consumed stream.
    events: Mutex<Option<mpsc::Receiver<AgentEventKind>>>,
    pub state: Mutex<SessionState>,
    next_id: AtomicU64,
}

pub(super) struct SessionState {
    pub closed: bool,
    pub selection: ModelSelection,
    /// The provider-native session id the last run reported, used as the
    /// resume cursor where the adapter declares `resume`.
    pub provider_session: Option<String>,
    pub active_run: Option<ActiveRun>,
}

pub(super) struct ActiveRun {
    pub run_id: RunId,
    pub cancellation: RunCancellationToken,
    /// Interactions the worker is waiting on, keyed by the request id the
    /// `approval.requested` event carried.
    pub pending: Arc<Mutex<BTreeMap<String, PendingInteraction>>>,
    pub worker: Mutex<Option<JoinHandle<()>>>,
}

impl CliSession {
    pub fn new(
        integration: IntegrationId,
        cwd: PathBuf,
        selection: ModelSelection,
        access: AccessMode,
    ) -> Arc<Self> {
        let (emit, events) = mpsc::channel();
        Arc::new(Self {
            integration,
            cwd,
            access,
            emit,
            events: Mutex::new(Some(events)),
            state: Mutex::new(SessionState {
                closed: false,
                selection,
                provider_session: None,
                active_run: None,
            }),
            next_id: AtomicU64::new(0),
        })
    }

    pub fn emit(&self, kind: AgentEventKind) {
        let _ = self.emit.send(kind);
    }

    pub fn take_events(&self) -> Option<mpsc::Receiver<AgentEventKind>> {
        self.events.lock().expect("session events mutex").take()
    }

    /// The next opaque id within this session (approval request ids).
    pub fn next_request_id(&self) -> RequestId {
        RequestId::new(format!(
            "approval-{}-{}",
            std::process::id(),
            self.next_id.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Apply a `model.select` resolution. Only a resolvable selection
    /// replaces the stored one; failures change nothing.
    pub fn set_selection(&self, selection: ModelSelection) -> Result<(), AgentRuntimeError> {
        let mut state = self.state.lock().expect("session state mutex");
        if state.closed {
            return Err(invalid_state("the session is closed"));
        }
        if state.active_run.is_some() {
            return Err(invalid_state(
                "the selection cannot change while a run is in flight",
            ));
        }
        state.selection = selection;
        Ok(())
    }

    /// Start one run for `input` on a worker thread. Capability validation
    /// runs before anything is emitted, so a rejected request never opens a
    /// run on the event stream.
    pub fn send_run(
        self: &Arc<Self>,
        execution: &Execution,
        supports_jsonl: bool,
        supports_resume: bool,
        supports_interaction: bool,
        input: UserInput,
    ) -> Result<RunId, AgentRuntimeError> {
        if input.text.trim().is_empty() {
            return Err(invalid_state("input text must not be empty"));
        }
        if !input.attachments.is_empty() {
            return Err(unsupported("the CLI fallback does not carry attachments"));
        }
        let mut state = self.state.lock().expect("session state mutex");
        if state.closed {
            return Err(invalid_state("the session is closed"));
        }
        if state.active_run.is_some() {
            return Err(invalid_state(
                "one Agent Run is already in flight for this session",
            ));
        }
        let selection = state.selection.clone();
        let run_id = RunId::new(format!(
            "run-{}-{}",
            std::process::id(),
            self.next_id.fetch_add(1, Ordering::Relaxed)
        ));
        let pending = Arc::new(Mutex::new(BTreeMap::new()));
        let streamed = Arc::new(AtomicBool::new(false));
        let cancellation = RunCancellationToken::new();
        let interaction_handler = supports_interaction.then(|| {
            Arc::new(CliInteractionHandler::new(
                Arc::clone(self),
                run_id.clone(),
                Arc::clone(&pending),
            )) as Arc<dyn AgentInteractionHandler>
        });
        let request = RunRequest {
            integration: self.integration.clone(),
            model: (!selection.model.is_empty()).then(|| selection.model.clone()),
            effort: selection.effort.map(|effort| effort.as_str().to_owned()),
            external_tools: None,
            account: None,
            prompt: input.text,
            // Structured output is requested only where declared: it is the
            // format the provider-native session id parses back out of.
            output: if supports_jsonl {
                OutputFormat::Jsonl
            } else {
                OutputFormat::Text
            },
            working_directory: Some(self.cwd.clone()),
            access: self.access,
            resume: if supports_resume {
                state.provider_session.clone()
            } else {
                None
            },
            timeout: None,
            interaction_handler,
        };
        execution
            .adapter()
            .validate(&request)
            .map_err(execution_error)?;
        let worker = {
            let session = Arc::clone(self);
            let execution = execution.clone();
            let run_id = run_id.clone();
            let cancellation = cancellation.clone();
            let streamed = Arc::clone(&streamed);
            thread::Builder::new()
                .name(format!("aifuel-cli-{}", self.integration))
                .spawn(move || {
                    run_worker(
                        session,
                        execution,
                        request,
                        run_id,
                        selection,
                        cancellation,
                        streamed,
                    )
                })
                .map_err(|error| {
                    AgentRuntimeError::provider_error(format!(
                        "could not start the run worker: {error}"
                    ))
                })?
        };
        state.active_run = Some(ActiveRun {
            run_id: run_id.clone(),
            cancellation,
            pending,
            worker: Mutex::new(Some(worker)),
        });
        Ok(run_id)
    }

    /// Deliver a host decision to one pending interaction. The first answer
    /// wins; anything else reports `already_resolved`.
    pub fn answer(
        &self,
        request: &RequestId,
        decision: &ApprovalDecision,
    ) -> Result<(), AgentRuntimeError> {
        let pending_map = {
            let state = self.state.lock().expect("session state mutex");
            match &state.active_run {
                Some(active) => Arc::clone(&active.pending),
                None => {
                    return Err(AgentRuntimeError::new(
                        ReceiptCode::AlreadyResolved,
                        "no pending Approval Request exists for this session",
                    ));
                }
            }
        };
        let mut pending = pending_map.lock().expect("pending approvals mutex");
        let Some(entry) = pending.get(request.as_str()) else {
            return Err(AgentRuntimeError::new(
                ReceiptCode::AlreadyResolved,
                "the Approval Request is not pending",
            ));
        };
        let response = entry.response_for(decision)?;
        let entry = pending
            .remove(request.as_str())
            .expect("the entry was just inspected");
        if entry.response.send((decision.clone(), response)).is_err() {
            // The run ended between the lookup and the send; the request is
            // no longer answerable, so this answer did not resolve it.
            return Err(AgentRuntimeError::new(
                ReceiptCode::AlreadyResolved,
                "the Approval Request already left the run",
            ));
        }
        Ok(())
    }

    /// Record the terminal facts for one run and return the session to
    /// `idle` when it is still open.
    fn finish_run(&self, run_id: &RunId, result: Result<RunResult, AgentRunError>, streamed: bool) {
        let open = {
            let mut state = self.state.lock().expect("session state mutex");
            if let Some(active) = state.active_run.take() {
                // Dropping the senders releases any interaction waiter with
                // a closed channel instead of leaving it parked forever.
                active
                    .pending
                    .lock()
                    .expect("pending approvals mutex")
                    .clear();
                *active.worker.lock().expect("run worker mutex") = None;
            }
            match &result {
                Ok(result) if result.session_id.is_some() => {
                    state.provider_session = result.session_id.clone();
                }
                _ => {}
            }
            !state.closed
        };
        let mut message_open = streamed;
        match result {
            Ok(result) => {
                if !streamed && !result.output.is_empty() {
                    message_open = true;
                    self.emit(AgentEventKind::MessageDelta {
                        run_id: run_id.clone(),
                        stream: MessageStream::Assistant,
                        text: result.output.clone(),
                    });
                }
                if message_open {
                    self.emit(AgentEventKind::MessageCompleted {
                        run_id: run_id.clone(),
                        stream: MessageStream::Assistant,
                    });
                }
                match result.status {
                    RunStatus::Succeeded => self.emit(AgentEventKind::RunCompleted {
                        run_id: run_id.clone(),
                        outcome: RunOutcome::Success,
                        usage: result.usage,
                    }),
                    RunStatus::Cancelled => self.emit(AgentEventKind::RunCompleted {
                        run_id: run_id.clone(),
                        outcome: RunOutcome::Cancelled,
                        usage: result.usage,
                    }),
                    RunStatus::Failed | RunStatus::Timeout => {
                        let message = result
                            .error
                            .clone()
                            .or_else(|| result.diagnostics.clone())
                            .unwrap_or_else(|| "agent run failed".to_owned());
                        self.emit(AgentEventKind::Error {
                            run_id: Some(run_id.clone()),
                            code: ReceiptCode::ProviderError,
                            message,
                            retryable: false,
                        });
                        self.emit(AgentEventKind::RunCompleted {
                            run_id: run_id.clone(),
                            outcome: RunOutcome::Failed,
                            usage: result.usage,
                        });
                    }
                }
            }
            Err(AgentRunError::Cancelled) => self.emit(AgentEventKind::RunCompleted {
                run_id: run_id.clone(),
                outcome: RunOutcome::Cancelled,
                usage: None,
            }),
            Err(error) => {
                self.emit(AgentEventKind::Error {
                    run_id: Some(run_id.clone()),
                    code: ReceiptCode::ProviderError,
                    message: error.to_string(),
                    retryable: false,
                });
                self.emit(AgentEventKind::RunCompleted {
                    run_id: run_id.clone(),
                    outcome: RunOutcome::Failed,
                    usage: None,
                });
            }
        }
        if open {
            self.emit(AgentEventKind::SessionStatus {
                status: SessionStatus::Idle,
            });
        }
    }

    /// Close the session: cancel any in-flight run, wait for its worker,
    /// then emit `session.closed` last.
    pub fn stop(&self) {
        let worker = {
            let mut state = self.state.lock().expect("session state mutex");
            state.closed = true;
            state.active_run.take().and_then(|active| {
                active.cancellation.cancel();
                active
                    .pending
                    .lock()
                    .expect("pending approvals mutex")
                    .clear();
                active.worker.lock().expect("run worker mutex").take()
            })
        };
        if let Some(worker) = worker {
            let _ = worker.join();
        }
        self.emit(AgentEventKind::SessionClosed { reason: None });
    }
}

/// The run worker: emits the run's events, blocks in the execution
/// adapter's synchronous `execute`, then emits the terminal outcome.
fn run_worker(
    session: Arc<CliSession>,
    execution: Execution,
    request: RunRequest,
    run_id: RunId,
    selection: ModelSelection,
    cancellation: RunCancellationToken,
    streamed: Arc<AtomicBool>,
) {
    session.emit(AgentEventKind::RunStarted {
        run_id: run_id.clone(),
        selection,
    });
    session.emit(AgentEventKind::SessionStatus {
        status: SessionStatus::Working,
    });
    let output_handler = CliOutputHandler {
        emit: session.emit.clone(),
        run_id: run_id.clone(),
        streamed: Arc::clone(&streamed),
    };
    let result =
        execution
            .adapter()
            .execute_with_output_handler(&request, &cancellation, &output_handler);
    session.finish_run(&run_id, result, streamed.load(Ordering::Acquire));
}

/// Streams provider-reported public output as `message.delta` events. Only
/// adapters whose executor reports through `AgentRunOutputHandler` ever
/// call this; the fallback terminal delta fills the gap elsewhere.
struct CliOutputHandler {
    emit: mpsc::Sender<AgentEventKind>,
    run_id: RunId,
    streamed: Arc<AtomicBool>,
}

impl std::fmt::Debug for CliOutputHandler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CliOutputHandler")
    }
}

impl AgentRunOutputHandler for CliOutputHandler {
    fn on_output(&self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        self.streamed.store(true, Ordering::Release);
        let _ = self.emit.send(AgentEventKind::MessageDelta {
            run_id: self.run_id.clone(),
            stream: MessageStream::Assistant,
            text: delta.to_owned(),
        });
    }
}

/// Map a request-side execution failure onto a receipt code. Capability
/// refusals are `unsupported`; process and provider failures are
/// `provider_error`.
pub(super) fn execution_error(error: AgentRunError) -> AgentRuntimeError {
    match error {
        AgentRunError::UnsupportedIntegration(integration) => unsupported(format!(
            "this adapter does not serve integration {integration}"
        )),
        AgentRunError::InvalidRequest(message) => unsupported(message),
        error => AgentRuntimeError::provider_error(error.to_string()),
    }
}

pub(super) fn invalid_state(message: impl Into<String>) -> AgentRuntimeError {
    AgentRuntimeError::new(ReceiptCode::InvalidState, message)
}

pub(super) fn unsupported(message: impl Into<String>) -> AgentRuntimeError {
    AgentRuntimeError::unsupported(message)
}
