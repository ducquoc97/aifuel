//! Session state, transport, and command plumbing behind
//! [`AcpAdapter`](super::AcpAdapter).
//!
//! A session owns one ACP agent process whose driver thread speaks the
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
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::mpsc as tokio_mpsc;

/// How long `send` waits for the driver to land `session/prompt` on the
/// wire. The write itself is instant; the bound covers a wedged driver.
const PROMPT_ACK_TIMEOUT: Duration = Duration::from_secs(15);
/// How long `answer`, `cancel`, and `model.select` wait for the driver
/// to write or complete their protocol exchange.
const COMMAND_ACK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `start` waits for the setup report; the driver's own
/// handshake deadline fires first, so this covers a wedged spawn only.
pub(super) const SESSION_SETUP_TIMEOUT: Duration = Duration::from_secs(15);

/// The transport factory the driver asks for one agent stdio pair.
/// Production spawns the configured binary; tests inject a duplex.
pub(super) type Connector =
    Arc<dyn Fn(&SessionSetup) -> Result<Transport, AgentRuntimeError> + Send + Sync>;

/// The connector that spawns `setup.program` with `setup.args`.
pub(super) fn process_connector() -> Connector {
    Arc::new(spawn_transport)
}

fn spawn_transport(setup: &SessionSetup) -> Result<Transport, AgentRuntimeError> {
    for candidate in crate::agent_execution::program_candidates(&setup.program) {
        let mut command = crate::agent_execution::owned_command(&candidate, |command| {
            command
                .args(&setup.args)
                .current_dir(&setup.cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        });
        match command.spawn() {
            Ok(mut child) => {
                let stdout = child.stdout().take().ok_or_else(|| {
                    AgentRuntimeError::provider_error("the agent stdout pipe is missing")
                })?;
                let stderr = child.stderr().take();
                let stdin = child.stdin().take().ok_or_else(|| {
                    AgentRuntimeError::provider_error("the agent stdin pipe is missing")
                })?;
                return Ok(Transport {
                    stdin: Box::new(stdin),
                    stdout: BufReader::new(Box::new(stdout) as Box<dyn AsyncRead + Unpin + Send>),
                    stderr: stderr
                        .map(|stderr| Box::new(stderr) as Box<dyn AsyncRead + Unpin + Send>),
                    child: Some(child),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(AgentRuntimeError::provider_error(format!(
                    "the agent could not be spawned: {error}"
                )));
            }
        }
    }
    Err(AgentRuntimeError::provider_error(format!(
        "provider executable {:?} was not found",
        setup.program
    )))
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
    /// The agent executable and argv, from the integration's wiring.
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// The requested model; applied through `session/set_config_option`
    /// when the agent advertises a model selector.
    pub model: Option<String>,
    /// The persisted ACP `sessionId` to `session/load` or
    /// `session/resume`; `None` opens a fresh session.
    pub resume_cursor: Option<String>,
}

/// The fact `start` learns from a completed handshake: the provider
/// ACP session id, which is the resume cursor. Capabilities the
/// handshake learned are already in session state.
pub(super) struct OpenedSession {
    pub session_id: String,
}

/// The report `start` waits on after launching the driver: the opened
/// session facts on success, the failure reason otherwise.
pub(super) type SetupReport = Result<OpenedSession, String>;

/// One command the adapter side pushes into the driver loop.
pub(super) enum DriverCommand {
    /// `session/prompt` for one user message.
    Prompt {
        run_id: RunId,
        blocks: Vec<Value>,
        selection: ModelSelection,
        reply: mpsc::Sender<Result<(), AgentRuntimeError>>,
    },
    /// A completed JSON-RPC answer to one `session/request_permission`.
    Answer {
        request_id: RequestId,
        decision: ApprovalDecision,
        message: Value,
        reply: mpsc::Sender<Result<(), AgentRuntimeError>>,
    },
    /// `session/cancel` for the in-flight prompt turn.
    CancelTurn {
        reply: mpsc::Sender<Result<(), AgentRuntimeError>>,
    },
    /// `session/set_config_option` for a `model.select` on the live
    /// session.
    SetModel {
        model: String,
        reply: mpsc::Sender<Result<(), AgentRuntimeError>>,
    },
    /// Tear the session down: finish run facts, kill the process,
    /// emit `session.closed`, and return the driver thread.
    Shutdown,
}

/// One ACP session inside an [`AcpAdapter`](super::AcpAdapter).
pub(super) struct AcpSession {
    pub integration: IntegrationId,
    pub cwd: PathBuf,
    pub access: AccessMode,
    /// Shared sender the driver and the session emit through. Sends
    /// that race a detached receiver are dropped, never panic.
    emit: mpsc::Sender<AgentEventKind>,
    /// Handed to `events()` once; later calls see an already-consumed
    /// stream.
    events: Mutex<Option<mpsc::Receiver<AgentEventKind>>>,
    /// Command ingress for the driver loop; `UnboundedSender::send`
    /// works from the synchronous adapter methods.
    commands: tokio_mpsc::UnboundedSender<DriverCommand>,
    /// The receiving half, handed to the driver once at spawn.
    commands_rx: Mutex<Option<tokio_mpsc::UnboundedReceiver<DriverCommand>>>,
    /// The agent process, shared so `stop` and adapter drop can
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
    /// The provider connection was lost; emitted once announced.
    pub interrupted: bool,
    /// `session.closed` was requested before the prelude ran; the
    /// announce path emits it right after `idle` so ordering stays
    /// causal.
    pub pending_close: Option<Option<String>>,
    /// `session.closed` was emitted; teardown paths must not repeat it.
    pub close_emitted: bool,
    pub selection: ModelSelection,
    /// The ACP `sessionId` reported at session start; the provider
    /// resume cursor for this session.
    pub provider_session: Option<String>,
    /// Whether the agent advertised `promptCapabilities.image`.
    pub prompt_image: bool,
    /// The session's advertised model selector, when it offered one.
    pub model_option: Option<interactions::ModelOption>,
    /// A `session/prompt` command is in flight and has not been
    /// answered. Kept separate from `active_run`.
    pub prompt_starting: bool,
    /// The in-flight run, identified by the adapter-generated run id.
    pub active_run: Option<RunId>,
    /// Approval Requests offered to hosts, keyed by contract request id.
    pub pending: BTreeMap<RequestId, PendingApproval>,
}

/// One `session/request_permission` parked on a host answer.
pub(super) struct PendingApproval {
    /// The agent request's raw JSON-RPC id; the answer responds on it.
    pub server_id: Value,
    /// The option ids the request offered, by contract vocabulary.
    pub options: interactions::PendingOptions,
    /// The contract option ids offered in `approval.requested`.
    pub offered: Vec<String>,
}

impl AcpSession {
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
                interrupted: false,
                pending_close: None,
                close_emitted: false,
                selection,
                provider_session: None,
                prompt_image: false,
                model_option: None,
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

    /// The next opaque id within this session (approval request ids and
    /// run ids).
    pub fn next_request_id(&self) -> RequestId {
        RequestId::new(format!(
            "approval-{}-{}",
            std::process::id(),
            self.next_id.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// The provider resume cursor: the ACP `sessionId`.
    pub fn resume_cursor(&self) -> Option<String> {
        self.state
            .lock()
            .expect("session state mutex")
            .provider_session
            .clone()
    }

    /// Apply a `model.select` resolution. Only a resolvable selection
    /// replaces the stored one; failures change nothing. A non-empty
    /// model is applied through `session/set_config_option` when the
    /// agent advertised a model selector, and fails otherwise: a model
    /// the agent cannot honor is never silently substituted.
    pub fn set_selection(&self, selection: ModelSelection) -> Result<(), AgentRuntimeError> {
        let model = {
            let mut state = self.state.lock().expect("session state mutex");
            if state.closed {
                return Err(invalid_state("the session is closed"));
            }
            if state.active_run.is_some() || state.prompt_starting {
                return Err(invalid_state(
                    "the selection cannot change while a run is in flight",
                ));
            }
            if selection.effort.is_some() {
                return Err(unsupported(
                    "the Agent Client Protocol has no effort selection",
                ));
            }
            if selection.model.is_empty() {
                state.selection = selection;
                return Ok(());
            }
            let Some(option) = state.model_option.as_ref() else {
                return Err(unsupported(
                    "the agent advertised no model configuration option",
                ));
            };
            interactions::selectable_model(option, &selection.model)?;
            state.selection = selection;
            state.selection.model.clone()
        };
        let (reply, wait) = mpsc::channel();
        self.commands
            .send(DriverCommand::SetModel { model, reply })
            .map_err(|_| AgentRuntimeError::provider_error("the provider driver is not running"))?;
        match wait.recv_timeout(COMMAND_ACK_TIMEOUT) {
            Ok(result) => result,
            Err(_) => Err(AgentRuntimeError::provider_error(
                "the model selection was not acknowledged",
            )),
        }
    }

    /// Emit the fresh-session prelude once, after the handshake reports
    /// the session facts. Held under the state lock so `session.closed`
    /// can never precede `session.created` when a teardown races the
    /// announcement: terminal facts that landed first replay here in
    /// causal order.
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
        if state.interrupted {
            self.emit(AgentEventKind::SessionStatus {
                status: SessionStatus::Interrupted,
            });
        }
        if let Some(reason) = state.pending_close.take() {
            state.close_emitted = true;
            self.emit(AgentEventKind::SessionClosed { reason });
        }
    }

    /// Record that the provider connection was lost. The `interrupted`
    /// status emits immediately once the prelude ran; before it, the
    /// flag waits for `announce` so the status cannot precede
    /// `session.created`.
    pub fn finish_interrupted(&self) {
        let mut state = self.state.lock().expect("session state mutex");
        state.interrupted = true;
        if state.announced && !state.close_emitted {
            self.emit(AgentEventKind::SessionStatus {
                status: SessionStatus::Interrupted,
            });
        }
    }

    /// Emit `session.closed` exactly once. Before the prelude the
    /// reason is parked for `announce` to replay; afterwards the event
    /// emits directly. Every exit path - requested stop, server death,
    /// adapter drop - funnels through here.
    pub fn finish_close(&self, reason: Option<String>) {
        let mut state = self.state.lock().expect("session state mutex");
        if state.close_emitted {
            return;
        }
        state.closed = true;
        if !state.announced {
            state.pending_close = Some(reason);
            return;
        }
        state.close_emitted = true;
        self.emit(AgentEventKind::SessionClosed { reason });
    }

    /// Clear the `session/prompt` in-flight flag when the driver
    /// answers the command without opening a run.
    pub fn clear_prompt_starting(&self) {
        self.state
            .lock()
            .expect("session state mutex")
            .prompt_starting = false;
    }

    /// Mark the session dead without emitting `session.closed`: the
    /// driver calls this on every exit so post-mortem commands fail
    /// fast. Terminal facts still flow through `emit` first.
    pub fn mark_closed(&self) {
        let mut state = self.state.lock().expect("session state mutex");
        state.closed = true;
        state.pending.clear();
        state.prompt_starting = false;
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
            .name(format!("aifuel-acp-{}", self.integration))
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

    /// Start one Agent Run for `input`: build the `session/prompt`
    /// content blocks and wait for the driver to land the request on
    /// the wire, which is when the run officially starts. The second
    /// `send` on a session fails while a run is in flight or still
    /// starting.
    pub fn send_turn(&self, input: UserInput) -> Result<RunId, AgentRuntimeError> {
        let (run_id, wait) = {
            let mut state = self.state.lock().expect("session state mutex");
            if state.closed {
                return Err(invalid_state("the session is closed"));
            }
            if state.prompt_starting || state.active_run.is_some() {
                return Err(invalid_state(
                    "one Agent Run is already in flight for this session",
                ));
            }
            let blocks = interactions::content_blocks(&input, state.prompt_image)?;
            let run_id = RunId::new(format!(
                "acp-run-{}-{}",
                std::process::id(),
                self.next_id.fetch_add(1, Ordering::Relaxed)
            ));
            let (reply, rx) = mpsc::channel();
            self.commands
                .send(DriverCommand::Prompt {
                    run_id: run_id.clone(),
                    blocks,
                    selection: state.selection.clone(),
                    reply,
                })
                .map_err(|_| {
                    AgentRuntimeError::provider_error("the provider driver is not running")
                })?;
            state.prompt_starting = true;
            (run_id, rx)
        };
        match wait.recv_timeout(PROMPT_ACK_TIMEOUT) {
            Ok(Ok(())) => Ok(run_id),
            Ok(Err(error)) => Err(error),
            Err(RecvTimeoutError::Timeout) => Err(AgentRuntimeError::provider_error(
                "the agent did not acknowledge the prompt",
            )),
            Err(RecvTimeoutError::Disconnected) => Err(AgentRuntimeError::provider_error(
                "the provider driver ended without answering the prompt",
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
            // The decision is valid only against the exact options the
            // request offered.
            if let ApprovalDecision::OptionId(id) = decision
                && !entry.offered.iter().any(|option| option == id)
            {
                return Err(invalid_state(format!(
                    "{id:?} was not offered on this request"
                )));
            }
            let outcome = interactions::permission_outcome(&entry.options, decision)?;
            let entry = state
                .pending
                .remove(request)
                .expect("the entry was just inspected");
            json_message_response(&entry.server_id, outcome)
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

    /// Request cancellation of the in-flight prompt turn. The
    /// `session/prompt` response reports the terminal `cancelled`
    /// outcome; `cancel` only confirms the notification reached the
    /// wire.
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
            .send(DriverCommand::CancelTurn { reply })
            .map_err(|_| AgentRuntimeError::provider_error("the provider driver is not running"))?;
        match wait.recv_timeout(COMMAND_ACK_TIMEOUT) {
            Ok(result) => result,
            Err(_) => Err(AgentRuntimeError::provider_error(
                "the cancel notification was not acknowledged",
            )),
        }
    }

    /// Force-kill the agent process so a wedged driver unblocks. Safe
    /// when the driver already reaped the process or the transport is
    /// a test duplex.
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

/// The JSON-RPC response frame answering a server-initiated request.
fn json_message_response(id: &Value, result: Value) -> Value {
    serde_json::json!({"id": id, "result": result})
}

impl crate::local_adapter::LocalSession for AcpSession {
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

/// Map a request-side failure onto a receipt code, matching the sibling
/// adapters' policy: request validation is `invalid_state`, process and
/// provider failures are `provider_error`.
pub(super) fn invalid_state(message: impl Into<String>) -> AgentRuntimeError {
    AgentRuntimeError::new(ReceiptCode::InvalidState, message)
}

pub(super) fn unsupported(message: impl Into<String>) -> AgentRuntimeError {
    AgentRuntimeError::unsupported(message)
}
