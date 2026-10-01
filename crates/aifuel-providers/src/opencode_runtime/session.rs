//! Session state, transport, and command plumbing behind
//! [`OpenCodeAdapter`](super::OpenCodeAdapter).
//!
//! A session owns one `opencode serve` process whose driver thread
//! speaks the HTTP API, plus one event channel every emission flows
//! through. All run-scoped emissions come from the driver, so event
//! order follows the run's causal order: `run.started` first, streamed
//! deltas and approval events while it runs, one terminal
//! `run.completed`, then the session status. The adapter side only
//! posts commands into the driver and takes the shared state it needs
//! to validate them.

use super::{protocol, serve};
use aifuel_core::{
    AccessMode, AgentEventKind, AgentRuntimeError, ApprovalDecision, IntegrationId, ModelSelection,
    ReceiptCode, RequestId, RunCancellationToken, RunId, SessionStatus, UserInput,
};
use process_wrap::tokio::TokioChildWrapper;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tokio::sync::mpsc as tokio_mpsc;

/// How long `send` waits for the driver to acknowledge the prompt was
/// dispatched. Dispatch is immediate - the prompt runs on a spawned
/// task - so this bound covers a wedged driver only.
const PROMPT_DISPATCH_TIMEOUT: Duration = Duration::from_secs(15);
/// How long `answer` and `cancel` wait for the driver to write their
/// HTTP request.
const COMMAND_ACK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `start` waits for the setup report; the driver's own
/// handshake deadline fires first, so this covers a wedged spawn only.
/// It must exceed the driver's `SETUP_TIMEOUT` with room for spawn and
/// teardown.
pub(super) const SESSION_SETUP_TIMEOUT: Duration = Duration::from_secs(60);

/// What a session start asks the driver to open.
pub(super) struct SessionSetup {
    pub cwd: PathBuf,
    pub access: AccessMode,
    /// The persisted OpenCode session id to reattach; `None` opens a
    /// fresh session with `POST /session`.
    pub resume_cursor: Option<String>,
}

/// The report `start` waits on after launching the driver: the OpenCode
/// session id on success, the failure reason otherwise.
pub(super) type SetupReport = Result<String, String>;

/// One command the adapter side pushes into the driver loop.
pub(super) enum DriverCommand {
    /// `POST /session/{id}/message` for one user message; its response
    /// mails the run's terminal outcome back to the driver.
    StartPrompt {
        parts: Vec<Value>,
        selection: ModelSelection,
        run_id: RunId,
        reply: mpsc::Sender<Result<RunId, AgentRuntimeError>>,
    },
    /// `POST /session/{id}/permissions/{permissionID}` for one pending
    /// Approval Request; `response` is the provider's reply spelling.
    AnswerPermission {
        request_id: RequestId,
        permission_id: String,
        response: String,
        decision: ApprovalDecision,
        reply: mpsc::Sender<Result<(), AgentRuntimeError>>,
    },
    /// `POST /session/{id}/abort` for the in-flight run.
    Abort {
        reply: mpsc::Sender<Result<(), AgentRuntimeError>>,
    },
    /// Tear the session down: finish run facts, kill the serve process,
    /// emit `session.closed`, and return the driver thread.
    Shutdown,
}

/// One OpenCode session inside a [`OpenCodeAdapter`](super::OpenCodeAdapter).
pub(super) struct OpenCodeSession {
    pub integration: IntegrationId,
    pub cwd: PathBuf,
    pub access: AccessMode,
    /// Shared sender the driver and the session emit through; sends
    /// racing a detached receiver are dropped, never panic.
    emit: mpsc::Sender<AgentEventKind>,
    /// Handed to `events()` once.
    events: Mutex<Option<mpsc::Receiver<AgentEventKind>>>,
    /// Command ingress for the driver loop.
    commands: tokio_mpsc::UnboundedSender<DriverCommand>,
    /// The receiving half, handed to the driver once at spawn.
    commands_rx: Mutex<Option<tokio_mpsc::UnboundedReceiver<DriverCommand>>>,
    /// The serve process, shared so `stop` and adapter drop can
    /// force-kill a wedged driver. The driver takes it out to reap.
    pub(super) child: Mutex<Option<Box<dyn TokioChildWrapper>>>,
    /// Cancels the event-stream reader's poll at shutdown.
    pub(super) reader_cancel: RunCancellationToken,
    pub state: Mutex<SessionState>,
    driver: Mutex<Option<JoinHandle<()>>>,
    next_id: AtomicU64,
}

pub(super) struct SessionState {
    pub closed: bool,
    /// `session.created`/`idle` were emitted; `session.closed` may
    /// follow only after that, and only once (`close_emitted`).
    pub announced: bool,
    pub close_emitted: bool,
    pub selection: ModelSelection,
    /// The OpenCode session id reported at session start; the provider
    /// resume cursor for this session.
    pub provider_session: Option<String>,
    /// A prompt command is in flight and unacknowledged, separate
    /// from `active_run`, which covers the provider's whole prompt
    /// exchange.
    pub prompt_starting: bool,
    /// The in-flight run: the provider user-message id sent as the
    /// prompt's `messageID`.
    pub active_run: Option<RunId>,
    /// Approval Requests offered to hosts, keyed by contract request id
    /// - the provider permission id.
    pub pending: BTreeMap<RequestId, PendingPermission>,
}

/// One provider permission ask parked on a host answer.
pub(super) struct PendingPermission {
    /// The provider permission id the reply route addresses.
    pub permission_id: String,
    /// Option ids the `approval.requested` payload offered, spelled in
    /// the provider's reply vocabulary.
    pub options: Vec<String>,
}

impl OpenCodeSession {
    pub fn new(
        integration: IntegrationId,
        cwd: PathBuf,
        access: AccessMode,
        selection: ModelSelection,
    ) -> Arc<Self> {
        let (emit, events) = mpsc::channel();
        let (commands, commands_rx) = tokio_mpsc::unbounded_channel();
        Arc::new(Self {
            integration,
            cwd,
            access,
            emit,
            events: Mutex::new(Some(events)),
            commands,
            commands_rx: Mutex::new(Some(commands_rx)),
            child: Mutex::new(None),
            reader_cancel: RunCancellationToken::new(),
            state: Mutex::new(SessionState {
                closed: false,
                announced: false,
                close_emitted: false,
                selection,
                provider_session: None,
                prompt_starting: false,
                active_run: None,
                pending: BTreeMap::new(),
            }),
            driver: Mutex::new(None),
            next_id: AtomicU64::new(0),
        })
    }

    pub fn emit(&self, kind: AgentEventKind) {
        let _ = self.emit.send(kind);
    }

    pub fn take_events(&self) -> Option<mpsc::Receiver<AgentEventKind>> {
        self.events.lock().expect("session events mutex").take()
    }

    /// The next OpenCode user-message id; `msg`-prefixed per the
    /// provider's convention, doubling as the contract `RunId`.
    pub fn next_message_id(&self) -> RunId {
        RunId::new(format!(
            "msg_aifuel{}_{}",
            std::process::id(),
            self.next_id.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// The OpenCode session id, the provider resume cursor for this
    /// session.
    pub fn resume_cursor(&self) -> Option<String> {
        self.state
            .lock()
            .expect("session state mutex")
            .provider_session
            .clone()
    }

    /// Apply a `model.select` resolution. Only a resolvable selection
    /// replaces the stored one; failures change nothing.
    pub fn set_selection(&self, selection: ModelSelection) -> Result<(), AgentRuntimeError> {
        let mut state = self.state.lock().expect("session state mutex");
        if state.closed {
            return Err(invalid_state("the session is closed"));
        }
        if state.active_run.is_some() || state.prompt_starting {
            return Err(invalid_state(
                "the selection cannot change while a run is in flight",
            ));
        }
        state.selection = selection;
        Ok(())
    }

    /// Emit the fresh-session prelude once. Held under the state lock so
    /// `session.closed` can never precede `session.created` when a
    /// teardown races the announcement.
    pub fn announce(&self) {
        let mut state = self.state.lock().expect("session state mutex");
        if state.announced {
            return;
        }
        state.announced = true;
        self.emit(AgentEventKind::SessionCreated {
            integration_id: self.integration.clone(),
            cwd: self.cwd.clone(),
        });
        self.emit(AgentEventKind::SessionStatus {
            status: SessionStatus::Idle,
        });
    }

    /// Emit `session.closed` once, only for announced sessions. Every
    /// exit path - requested stop, server death, adapter drop - funnels
    /// through here.
    pub fn finish_close(&self, reason: Option<String>) {
        let mut state = self.state.lock().expect("session state mutex");
        if !state.announced || state.close_emitted {
            return;
        }
        state.closed = true;
        state.close_emitted = true;
        self.emit(AgentEventKind::SessionClosed { reason });
    }

    /// Clear the prompt in-flight flag when the driver answers without
    /// opening a run (dispatch or build failure).
    pub fn clear_prompt_starting(&self) {
        self.state
            .lock()
            .expect("session state mutex")
            .prompt_starting = false;
    }

    /// Mark the session dead without emitting `session.closed`: the
    /// driver calls this on every exit so post-mortem commands fail fast.
    pub fn mark_closed(&self) {
        let mut state = self.state.lock().expect("session state mutex");
        state.closed = true;
        state.pending.clear();
        state.prompt_starting = false;
        state.active_run = None;
    }

    /// Launch the driver thread; the caller waits on `setup_tx`'s pair
    /// for the handshake report.
    pub fn start_driver(
        self: &Arc<Self>,
        connector: serve::Connector,
        setup: SessionSetup,
        setup_tx: mpsc::Sender<SetupReport>,
    ) -> Result<(), AgentRuntimeError> {
        let session = Arc::clone(self);
        let commands = self
            .commands_rx
            .lock()
            .expect("commands receiver mutex")
            .take()
            .ok_or_else(|| invalid_state("the session driver is already running"))?;
        let handle = thread::Builder::new()
            .name(format!("aifuel-opencode-{}", self.integration))
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = setup_tx.send(Err(format!(
                            "the session driver runtime could not start: {error}"
                        )));
                        return;
                    }
                };
                runtime.block_on(super::driver::run(
                    session, connector, setup, setup_tx, commands,
                ));
            })
            .map_err(|error| {
                AgentRuntimeError::provider_error(format!(
                    "could not start the session driver: {error}"
                ))
            })?;
        *self.driver.lock().expect("driver mutex") = Some(handle);
        Ok(())
    }

    /// Start one Agent Run for `input` and wait for the driver's
    /// dispatch acknowledgment. A second `send` fails while a run is
    /// in flight or still starting.
    pub fn send_turn(&self, input: UserInput) -> Result<RunId, AgentRuntimeError> {
        let parts = protocol::prompt_parts(&input)?;
        let wait;
        {
            let mut state = self.state.lock().expect("session state mutex");
            if state.closed {
                return Err(invalid_state("the session is closed"));
            }
            if state.prompt_starting || state.active_run.is_some() {
                return Err(invalid_state(
                    "one Agent Run is already in flight for this session",
                ));
            }
            // Mark before dispatch: the driver clears this flag the
            // moment it answers, so setting it after the send could
            // overwrite its clear and wedge every later send.
            state.prompt_starting = true;
            let run_id = self.next_message_id();
            let (reply, rx) = mpsc::channel();
            if self
                .commands
                .send(DriverCommand::StartPrompt {
                    parts,
                    selection: state.selection.clone(),
                    run_id,
                    reply,
                })
                .is_err()
            {
                state.prompt_starting = false;
                return Err(AgentRuntimeError::provider_error(
                    "the provider driver is not running",
                ));
            }
            wait = rx;
        }
        match wait.recv_timeout(PROMPT_DISPATCH_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                self.clear_prompt_starting();
                Err(AgentRuntimeError::provider_error(
                    "the driver did not acknowledge the prompt dispatch",
                ))
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.clear_prompt_starting();
                Err(AgentRuntimeError::provider_error(
                    "the provider driver ended without answering the prompt dispatch",
                ))
            }
        }
    }

    /// Deliver a host decision to one pending Approval Request. The first
    /// answer wins; anything else reports `already_resolved`.
    pub fn answer(
        &self,
        request: &RequestId,
        decision: &ApprovalDecision,
    ) -> Result<(), AgentRuntimeError> {
        let entry = {
            let mut state = self.state.lock().expect("session state mutex");
            if state.closed {
                return Err(AgentRuntimeError::new(
                    ReceiptCode::AlreadyResolved,
                    "the Approval Request is not pending",
                ));
            }
            let Some(entry) = state.pending.get(request) else {
                return Err(AgentRuntimeError::new(
                    ReceiptCode::AlreadyResolved,
                    "the Approval Request is not pending",
                ));
            };
            let response = protocol::permission_verdict(&entry.options, decision)?;
            let permission_id = entry.permission_id.clone();
            state.pending.remove(request);
            (permission_id, response)
        };
        let (reply, wait) = mpsc::channel();
        self.commands
            .send(DriverCommand::AnswerPermission {
                request_id: request.clone(),
                permission_id: entry.0,
                response: entry.1,
                decision: decision.clone(),
                reply,
            })
            .map_err(|_| AgentRuntimeError::provider_error("the provider driver is not running"))?;
        match wait.recv_timeout(COMMAND_ACK_TIMEOUT) {
            Ok(result) => result,
            Err(_) => Err(AgentRuntimeError::provider_error(
                "the permission answer was not acknowledged",
            )),
        }
    }

    /// Request `session.abort` for the in-flight run; `cancel` confirms
    /// the request reached the server, while the prompt response owns
    /// the terminal `cancelled` outcome.
    pub fn cancel(&self, run: &RunId) -> Result<(), AgentRuntimeError> {
        {
            let state = self.state.lock().expect("session state mutex");
            match &state.active_run {
                Some(active) if *active == *run => {}
                _ => {
                    return Err(invalid_state(
                        "no in-flight Agent Run with that id exists for this session",
                    ));
                }
            }
        }
        let (reply, wait) = mpsc::channel();
        self.commands
            .send(DriverCommand::Abort { reply })
            .map_err(|_| AgentRuntimeError::provider_error("the provider driver is not running"))?;
        match wait.recv_timeout(COMMAND_ACK_TIMEOUT) {
            Ok(result) => result,
            Err(_) => Err(AgentRuntimeError::provider_error(
                "the abort request was not acknowledged",
            )),
        }
    }

    /// Force-kill the serve process so a wedged driver unblocks; safe
    /// when nothing is running (fake transports, reaped children).
    pub fn kill_child(&self) {
        let mut slot = self.child.lock().expect("child mutex");
        if let Some(child) = slot.as_mut() {
            let _ = child.start_kill();
        }
    }

    /// Teardown kick used by adapter drop: mark closed, cancel the
    /// reader, ask the driver to shut down, and kill the process.
    pub fn close_transport(&self) {
        {
            self.state.lock().expect("session state mutex").closed = true;
        }
        self.reader_cancel.cancel();
        let _ = self.commands.send(DriverCommand::Shutdown);
        self.kill_child();
    }

    /// Close the session: shut the driver down and wait for it so the
    /// run's terminal facts and `session.closed` land in order.
    pub fn shutdown(&self) {
        let driver = {
            {
                self.state.lock().expect("session state mutex").closed = true;
            }
            self.reader_cancel.cancel();
            let _ = self.commands.send(DriverCommand::Shutdown);
            self.kill_child();
            self.driver.lock().expect("driver mutex").take()
        };
        crate::local_adapter::join_bounded(driver);
        self.finish_close(None);
    }
}

impl crate::local_adapter::LocalSession for OpenCodeSession {
    fn resume_cursor(&self) -> Option<String> {
        self.resume_cursor()
    }

    fn set_selection(&self, selection: ModelSelection) -> Result<(), AgentRuntimeError> {
        self.set_selection(selection)
    }

    fn take_events(&self) -> Option<mpsc::Receiver<AgentEventKind>> {
        self.take_events()
    }

    fn shutdown(&self) {
        self.shutdown();
    }

    fn close_transport(&self) {
        self.close_transport();
    }
}

/// Map a request-side failure onto a receipt code, matching the CLI
/// adapter's `execution_error` policy: request validation is
/// `invalid_state`, process and provider failures are `provider_error`.
pub(super) fn invalid_state(message: impl Into<String>) -> AgentRuntimeError {
    AgentRuntimeError::new(ReceiptCode::InvalidState, message)
}
