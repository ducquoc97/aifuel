//! Session state, transport, and command plumbing behind
//! [`CodexAdapter`](super::CodexAdapter).
//!
//! A session owns one app-server process whose driver thread speaks the
//! protocol, plus one event channel every emission flows through. All
//! run-scoped emissions come from the driver, so event order follows the
//! run's causal order: `run.started` first, streamed deltas and approval
//! events while it runs, one terminal `run.completed`, then the session
//! status. The adapter side only posts commands into the driver and
//! takes the shared state it needs to validate them.

use super::interactions;
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
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::mpsc as tokio_mpsc;

/// How long `send` waits for the app-server to acknowledge `turn/start`.
/// The provider answers in milliseconds; this bound covers a wedged
/// process only.
const TURN_START_TIMEOUT: Duration = Duration::from_secs(15);
/// How long `answer` and `cancel` wait for the driver to write their
/// protocol message.
const COMMAND_ACK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `start` waits for the setup report; the driver's own
/// handshake deadline fires first, so this covers a wedged spawn only.
pub(super) const SESSION_SETUP_TIMEOUT: Duration = Duration::from_secs(15);

/// The transport factory the driver asks for one app-server I/O pair.
pub(super) type Connector =
    Arc<dyn Fn(&SessionSetup) -> Result<Transport, AgentRuntimeError> + Send + Sync>;

/// Spawn `codex app-server` and return its pipes as a [`Transport`].
pub(super) fn default_connector() -> Connector {
    Arc::new(spawn_transport)
}

fn spawn_transport(setup: &SessionSetup) -> Result<Transport, AgentRuntimeError> {
    let mut child = crate::codex::app_server::spawn_app_server(&setup.cwd, &setup.env)
        .map_err(|error| AgentRuntimeError::provider_error(error.to_string()))?;
    let stdout = child.stdout().take().ok_or_else(|| {
        AgentRuntimeError::provider_error("the app-server stdout pipe is missing")
    })?;
    let stderr = child.stderr().take();
    let stdin = child
        .stdin()
        .take()
        .ok_or_else(|| AgentRuntimeError::provider_error("the app-server stdin pipe is missing"))?;
    Ok(Transport {
        stdin: Box::new(stdin),
        stdout: BufReader::new(Box::new(stdout) as Box<dyn AsyncRead + Unpin + Send>),
        stderr: stderr.map(|stderr| Box::new(stderr) as Box<dyn AsyncRead + Unpin + Send>),
        child: Some(child),
    })
}

/// The I/O pair the protocol driver speaks over. `child` is `None` on
/// test transports, where no process exists to reap.
pub(super) struct Transport {
    pub stdin: Box<dyn AsyncWrite + Unpin + Send>,
    pub stdout: BufReader<Box<dyn AsyncRead + Unpin + Send>>,
    pub stderr: Option<Box<dyn AsyncRead + Unpin + Send>>,
    pub child: Option<Box<dyn TokioChildWrapper>>,
}

/// What a session start asks the driver to open.
pub(super) struct SessionSetup {
    pub cwd: PathBuf,
    pub access: AccessMode,
    pub model: Option<String>,
    /// The persisted Codex thread id to `thread/resume`; `None` opens a
    /// fresh thread with `thread/start`.
    pub resume_cursor: Option<String>,
    /// The exact AI Fuel Gateway tool names the host asked the session
    /// to enforce. Non-empty selections spawn the filtered gateway MCP
    /// server through the thread config and gate setup on those tools
    /// reporting ready.
    pub external_tools: Vec<String>,
    /// The instance environment overlay applied to the app-server process
    /// at spawn. Resolved credential material may ride along - it is never
    /// logged or persisted by this layer.
    pub env: std::collections::BTreeMap<String, String>,
}

/// The report `start` waits on after launching the driver: the Codex
/// thread id on success, the failure reason otherwise.
pub(super) type SetupReport = Result<String, String>;

/// One command the adapter side pushes into the driver loop.
pub(super) enum DriverCommand {
    /// `turn/start` for one user message.
    StartTurn {
        items: Vec<Value>,
        selection: ModelSelection,
        reply: mpsc::Sender<Result<RunId, AgentRuntimeError>>,
    },
    /// A completed JSON-RPC answer to one server-initiated request.
    Answer {
        request_id: RequestId,
        decision: ApprovalDecision,
        message: Value,
        reply: mpsc::Sender<Result<(), AgentRuntimeError>>,
    },
    /// `turn/interrupt` for the in-flight turn.
    Interrupt {
        turn_id: String,
        reply: mpsc::Sender<Result<(), AgentRuntimeError>>,
    },
    /// Tear the session down: finish run facts, kill the process,
    /// emit `session.closed`, and return the driver thread.
    Shutdown,
}

/// One Codex session inside a [`CodexAdapter`](super::CodexAdapter).
pub(super) struct CodexSession {
    pub integration: IntegrationId,
    pub cwd: PathBuf,
    pub access: AccessMode,
    /// Shared sender the driver and the session emit through. Sends that
    /// race a detached receiver are dropped, never panic.
    emit: mpsc::Sender<AgentEventKind>,
    /// Handed to `events()` once; later calls see an already-consumed
    /// stream.
    events: Mutex<Option<mpsc::Receiver<AgentEventKind>>>,
    /// Command ingress for the driver loop; `UnboundedSender::send`
    /// works from the synchronous adapter methods.
    commands: tokio_mpsc::UnboundedSender<DriverCommand>,
    /// The receiving half, handed to the driver once at spawn.
    commands_rx: Mutex<Option<tokio_mpsc::UnboundedReceiver<DriverCommand>>>,
    /// The app-server process, shared so `stop` and adapter drop can
    /// force-kill a wedged driver. The driver takes it out to reap.
    pub(super) child: Mutex<Option<Box<dyn TokioChildWrapper>>>,
    /// Cancels the stdout reader's poll when the session shuts down.
    pub(super) reader_cancel: RunCancellationToken,
    pub state: Mutex<SessionState>,
    driver: Mutex<Option<JoinHandle<()>>>,
    next_id: AtomicU64,
}

pub(super) struct SessionState {
    pub closed: bool,
    /// `session.created`/`idle` have been emitted; teardown may emit
    /// `session.closed` only after that.
    pub announced: bool,
    /// `session.closed` was emitted; teardown paths must not repeat it.
    pub close_emitted: bool,
    pub selection: ModelSelection,
    /// The Codex thread id reported at session start; the provider
    /// resume cursor for this session.
    pub thread_id: Option<String>,
    /// A `turn/start` command is in flight and has not been answered.
    /// Kept separate from `active_run`, which needs the provider's turn
    /// id the reply returns.
    pub turn_starting: bool,
    /// The in-flight run, identified by the provider turn id.
    pub active_run: Option<RunId>,
    /// Approval Requests offered to hosts, keyed by contract request id.
    pub pending: BTreeMap<RequestId, PendingApproval>,
}

/// One server-initiated request parked on a host answer.
pub(super) struct PendingApproval {
    /// The parsed provider request; building the JSON-RPC answer needs
    /// its request id and method.
    pub interaction: crate::codex::interaction::PendingInteraction,
    /// Option ids the `approval.requested` payload offered.
    pub options: Vec<String>,
    /// Question ids a text decision fills, in order.
    pub question_ids: Vec<String>,
}

impl CodexSession {
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
                thread_id: None,
                turn_starting: false,
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

    /// The next opaque id within this session (approval request ids).
    pub fn next_request_id(&self) -> RequestId {
        RequestId::new(format!(
            "approval-{}-{}",
            std::process::id(),
            self.next_id.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// The Codex thread id, the provider resume cursor for this session.
    pub fn resume_cursor(&self) -> Option<String> {
        self.state
            .lock()
            .expect("session state mutex")
            .thread_id
            .clone()
    }

    /// Apply a `model.select` resolution. Only a resolvable selection
    /// replaces the stored one; failures change nothing.
    pub fn set_selection(&self, selection: ModelSelection) -> Result<(), AgentRuntimeError> {
        let mut state = self.state.lock().expect("session state mutex");
        if state.closed {
            return Err(invalid_state("the session is closed"));
        }
        if state.active_run.is_some() || state.turn_starting {
            return Err(invalid_state(
                "the selection cannot change while a run is in flight",
            ));
        }
        state.selection = selection;
        Ok(())
    }

    /// Emit the fresh-session prelude once, after the handshake reports
    /// the thread id. Held under the state lock so `session.closed` can
    /// never precede `session.created` when a teardown races the
    /// announcement.
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

    /// Emit `session.closed` exactly once, only for announced sessions.
    /// Every exit path - requested stop, server death, adapter drop -
    /// funnels through here.
    pub fn finish_close(&self, reason: Option<String>) {
        let mut state = self.state.lock().expect("session state mutex");
        if !state.announced || state.close_emitted {
            return;
        }
        state.closed = true;
        state.close_emitted = true;
        self.emit(AgentEventKind::SessionClosed { reason });
    }

    /// Clear the `turn/start` in-flight flag when the driver answers the
    /// command without opening a run (write or response failure).
    pub fn clear_turn_starting(&self) {
        self.state
            .lock()
            .expect("session state mutex")
            .turn_starting = false;
    }

    /// Mark the session dead without emitting `session.closed`: the
    /// driver calls this on every exit so post-mortem commands fail
    /// fast. Terminal facts still flow through `emit` first.
    pub fn mark_closed(&self) {
        let mut state = self.state.lock().expect("session state mutex");
        state.closed = true;
        state.pending.clear();
        state.turn_starting = false;
        state.active_run = None;
    }

    /// Launch the driver thread for this session. The caller waits on
    /// `setup_rx` for the handshake report.
    pub fn start_driver(
        self: &Arc<Self>,
        connector: Connector,
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
            .name(format!("aifuel-codex-{}", self.integration))
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

    /// Start one Agent Run for `input`: post `turn/start` and wait for
    /// the provider's turn id, which becomes the opaque `RunId`. The
    /// second `send` on a session fails while a run is in flight or
    /// still starting.
    pub fn send_turn(&self, input: UserInput) -> Result<RunId, AgentRuntimeError> {
        let items = interactions::input_items(&input)?;
        let wait;
        {
            let mut state = self.state.lock().expect("session state mutex");
            if state.closed {
                return Err(invalid_state("the session is closed"));
            }
            if state.turn_starting || state.active_run.is_some() {
                return Err(invalid_state(
                    "one Agent Run is already in flight for this session",
                ));
            }
            let (reply, rx) = mpsc::channel();
            self.commands
                .send(DriverCommand::StartTurn {
                    items,
                    selection: state.selection.clone(),
                    reply,
                })
                .map_err(|_| {
                    AgentRuntimeError::provider_error("the provider driver is not running")
                })?;
            state.turn_starting = true;
            wait = rx;
        }
        match wait.recv_timeout(TURN_START_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(AgentRuntimeError::provider_error(
                "the app-server did not acknowledge the turn start",
            )),
            Err(RecvTimeoutError::Disconnected) => Err(AgentRuntimeError::provider_error(
                "the provider driver ended without answering the turn start",
            )),
        }
    }

    /// Deliver a host decision to one pending Approval Request. The
    /// first answer wins; anything else reports `already_resolved`.
    pub fn answer(
        &self,
        request: &RequestId,
        decision: &ApprovalDecision,
    ) -> Result<(), AgentRuntimeError> {
        let message = {
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
            let response = interactions::decision_response(entry, decision)?;
            let entry = state
                .pending
                .remove(request)
                .expect("the entry was just inspected");
            crate::codex::interaction::response_message(&entry.interaction, response)
                .map_err(AgentRuntimeError::provider_error)?
        };
        let (reply, wait) = mpsc::channel();
        self.commands
            .send(DriverCommand::Answer {
                request_id: request.clone(),
                decision: decision.clone(),
                message,
                reply,
            })
            .map_err(|_| AgentRuntimeError::provider_error("the provider driver is not running"))?;
        match wait.recv_timeout(COMMAND_ACK_TIMEOUT) {
            Ok(result) => result,
            Err(_) => Err(AgentRuntimeError::provider_error(
                "the approval answer was not acknowledged",
            )),
        }
    }

    /// Request interruption of the in-flight turn. The provider's
    /// `turn/completed` notification reports the terminal `cancelled`
    /// outcome; `cancel` only confirms the request reached the wire.
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
            .send(DriverCommand::Interrupt {
                turn_id: run.as_str().to_owned(),
                reply,
            })
            .map_err(|_| AgentRuntimeError::provider_error("the provider driver is not running"))?;
        match wait.recv_timeout(COMMAND_ACK_TIMEOUT) {
            Ok(result) => result,
            Err(_) => Err(AgentRuntimeError::provider_error(
                "the interrupt request was not acknowledged",
            )),
        }
    }

    /// Force-kill the app-server process so a wedged driver unblocks.
    /// Safe when the driver already reaped the process or the transport
    /// is a test duplex.
    pub fn kill_child(&self) {
        let mut slot = self.child.lock().expect("child mutex");
        if let Some(child) = slot.as_mut() {
            let _ = child.start_kill();
        }
    }

    /// Best-effort teardown kick used by adapter drop: mark closed,
    /// cancel the reader, ask the driver to shut down, and kill the
    /// process. The driver's own teardown emits terminal facts.
    pub fn close_transport(&self) {
        {
            self.state.lock().expect("session state mutex").closed = true;
        }
        self.reader_cancel.cancel();
        let _ = self.commands.send(DriverCommand::Shutdown);
        self.kill_child();
    }

    /// Close the session: shut the driver down and wait for it, so the
    /// run's terminal facts and `session.closed` land in order before
    /// the handle is released.
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

impl crate::local_adapter::LocalSession for CodexSession {
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

pub(super) fn unsupported(message: impl Into<String>) -> AgentRuntimeError {
    AgentRuntimeError::unsupported(message)
}
