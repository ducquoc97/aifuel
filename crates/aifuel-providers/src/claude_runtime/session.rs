//! Session state, transport, and command plumbing behind
//! [`ClaudeAdapter`](super::ClaudeAdapter).
//!
//! A session owns one `claude` process whose driver thread speaks the
//! stream-json protocol, plus one event channel every emission flows
//! through. All run-scoped emissions come from the driver, so event
//! order follows the run's causal order: `run.started` first, streamed
//! deltas and approval events while it runs, one terminal
//! `run.completed`, then the session status. The adapter side only
//! posts commands onto the driver's queue and reads the shared state
//! it needs to validate them.

use super::protocol;
use aifuel_core::{
    AccessMode, AgentEventKind, AgentRuntimeError, ApprovalDecision, Effort, IntegrationId,
    ModelSelection, ReceiptCode, RequestId, RunId, SessionStatus, UserInput,
};
use process_wrap::std::{StdChildWrapper, StdCommandWrap};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[cfg(windows)]
use process_wrap::std::JobObject;
#[cfg(unix)]
use process_wrap::std::ProcessGroup;

/// How long `send` waits for the driver to write the user line. The
/// write is a pipe flush; this bound covers a wedged driver only.
const WRITE_ACK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `start` waits for the setup report; the driver's own
/// handshake deadline fires first, so this covers a wedged spawn only.
pub(super) const SESSION_SETUP_TIMEOUT: Duration = Duration::from_secs(20);

/// The transport factory the driver asks for one `claude` I/O pair.
/// Production spawns the process; tests inject a scripted duplex.
pub(super) type Connector =
    Arc<dyn Fn(&SessionSetup) -> Result<Transport, AgentRuntimeError> + Send + Sync>;

/// Spawn `claude` in stream-json mode and return its pipes.
pub(super) fn default_connector() -> Connector {
    Arc::new(spawn_transport)
}

fn spawn_transport(setup: &SessionSetup) -> Result<Transport, AgentRuntimeError> {
    let args = protocol::spawn_args(
        setup.model.as_deref(),
        setup.effort,
        setup.access,
        setup.resume_cursor.as_deref(),
    );
    let mut last_error = None;
    for candidate in crate::agent_execution::program_candidates("claude") {
        let cwd = setup.cwd.clone();
        let args = args.clone();
        let mut command = StdCommandWrap::with_new(candidate, move |command| {
            // Managed providers must not recursively start another AI Fuel
            // execution owner. The executable boundary rejects this marker.
            command
                .env("AIFUEL_MANAGED_RUN", "1")
                .current_dir(&cwd)
                .args(&args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        });
        #[cfg(unix)]
        command.wrap(ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(JobObject);
        match command.spawn() {
            Ok(mut child) => {
                let stdin = child.stdin().take().ok_or_else(|| {
                    AgentRuntimeError::provider_error("the claude stdin pipe is missing")
                })?;
                let stdout = child.stdout().take().ok_or_else(|| {
                    AgentRuntimeError::provider_error("the claude stdout pipe is missing")
                })?;
                let stderr = child.stderr().take();
                return Ok(Transport {
                    stdin: Box::new(stdin),
                    stdout: Box::new(BufReader::new(stdout)),
                    stderr: stderr.map(|stderr| Box::new(stderr) as Box<dyn Read + Send>),
                    child: Some(child),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                last_error = Some(error);
            }
            Err(error) => {
                return Err(AgentRuntimeError::provider_error(format!(
                    "could not start the claude session process: {error}"
                )));
            }
        }
    }
    Err(AgentRuntimeError::provider_error(format!(
        "the provider executable \"claude\" was not found on PATH: {}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "no spawn attempt ran".to_owned())
    )))
}

/// The I/O halves the protocol driver speaks over. `child` is `None`
/// on test transports, where no process exists to reap.
pub(super) struct Transport {
    pub stdin: Box<dyn Write + Send>,
    pub stdout: Box<dyn BufRead + Send>,
    pub stderr: Option<Box<dyn Read + Send>>,
    pub child: Option<Box<dyn StdChildWrapper>>,
}

/// What a session start asks the driver to open.
pub(super) struct SessionSetup {
    pub cwd: PathBuf,
    pub access: AccessMode,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    /// The persisted claude session id to `--resume`; `None` opens a
    /// fresh provider session.
    pub resume_cursor: Option<String>,
}

/// The report `start` waits on after launching the driver: `Ok` once
/// the provider's `system`/`init` frame reported the session id.
pub(super) type SetupReport = Result<(), String>;

/// One stdout line the reader forwards to the driver. `Eof` and
/// `Error` both mean the provider stream ended.
pub(super) enum LineRead {
    Data(String),
    Eof,
    Error(String),
}

/// Everything the driver consumes: adapter commands and provider
/// stdout lines share one queue so the driver works strictly in
/// arrival order.
pub(super) enum DriverInput {
    Command(DriverCommand),
    Line(LineRead),
}

/// One command the adapter side pushes onto the driver queue.
pub(super) enum DriverCommand {
    /// Write one `user` message; the driver emits `run.started` first.
    UserMessage {
        run_id: RunId,
        text: String,
        reply: mpsc::Sender<Result<(), AgentRuntimeError>>,
    },
    /// Answer one `can_use_tool` request. `line` is the serialized
    /// `control_response`; `interrupt` also cancels the run (`cancel`
    /// decisions deny and interrupt, mirroring the CLI fallback).
    Answer {
        request_id: RequestId,
        decision: ApprovalDecision,
        line: String,
        interrupt: bool,
        reply: mpsc::Sender<Result<(), AgentRuntimeError>>,
    },
    /// Write the protocol-level `interrupt` for the in-flight turn.
    Interrupt {
        run_id: RunId,
        reply: mpsc::Sender<Result<(), AgentRuntimeError>>,
    },
    /// `set_model` after a resolved `model.select`.
    SetModel {
        model: Option<String>,
        reply: mpsc::Sender<Result<(), AgentRuntimeError>>,
    },
    /// Tear the session down: finish run facts, close stdin, kill the
    /// process, emit `session.closed`, and return the driver thread.
    Shutdown,
}

/// One Claude session inside a [`ClaudeAdapter`](super::ClaudeAdapter).
pub(super) struct ClaudeSession {
    pub integration: IntegrationId,
    pub cwd: PathBuf,
    pub access: AccessMode,
    /// Shared sender the driver and the session emit through. Sends that
    /// race a detached receiver are dropped, never panic.
    emit: mpsc::Sender<AgentEventKind>,
    /// Handed to `events()` once; later calls see an already-consumed
    /// stream.
    events: Mutex<Option<mpsc::Receiver<AgentEventKind>>>,
    /// Command ingress for the driver loop; `mpsc::Sender::send` works
    /// from the synchronous adapter methods, and the stdout reader
    /// clones it to forward provider lines onto the same queue.
    inbox: mpsc::Sender<DriverInput>,
    /// The receiving half, handed to the driver once at spawn.
    inbox_rx: Mutex<Option<mpsc::Receiver<DriverInput>>>,
    /// The claude process, shared so `stop` and adapter drop can
    /// force-kill a wedged driver.
    child: Mutex<Option<Box<dyn StdChildWrapper>>>,
    pub state: Mutex<SessionState>,
    driver: Mutex<Option<JoinHandle<()>>>,
    reader: Mutex<Option<JoinHandle<()>>>,
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
    /// The claude session id `system`/`init` reported; the provider
    /// resume cursor for this session.
    pub claude_session_id: Option<String>,
    /// The in-flight run. The adapter reserves it before posting the
    /// command so a second `send` loses immediately.
    pub active_run: Option<RunId>,
    /// Approval Requests offered to hosts, keyed by the contract
    /// request id (which wraps the provider's control request id).
    pub pending: BTreeMap<RequestId, PendingApproval>,
}

/// One `can_use_tool` request parked on a host answer.
pub(super) struct PendingApproval {
    /// The tool input, echoed back unchanged on `allow`.
    pub input: Value,
    /// Option ids the `approval.requested` payload offered.
    pub options: Vec<String>,
}

impl ClaudeSession {
    pub fn new(
        integration: IntegrationId,
        cwd: PathBuf,
        access: AccessMode,
        selection: ModelSelection,
    ) -> Arc<Self> {
        let (emit, events) = mpsc::channel();
        let (inbox, inbox_rx) = mpsc::channel();
        Arc::new(Self {
            integration,
            cwd,
            access,
            emit,
            events: Mutex::new(Some(events)),
            inbox,
            inbox_rx: Mutex::new(Some(inbox_rx)),
            child: Mutex::new(None),
            state: Mutex::new(SessionState {
                closed: false,
                announced: false,
                close_emitted: false,
                selection,
                claude_session_id: None,
                active_run: None,
                pending: BTreeMap::new(),
            }),
            driver: Mutex::new(None),
            reader: Mutex::new(None),
            next_id: AtomicU64::new(0),
        })
    }

    pub fn emit(&self, kind: AgentEventKind) {
        let _ = self.emit.send(kind);
    }

    pub fn take_events(&self) -> Option<mpsc::Receiver<AgentEventKind>> {
        self.events.lock().expect("session events mutex").take()
    }

    /// The next opaque provider wire id within this session (control
    /// request ids).
    pub fn next_wire_id(&self, prefix: &str) -> String {
        format!(
            "{prefix}-{}-{}",
            std::process::id(),
            self.next_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// The claude session id, the provider resume cursor for this
    /// session.
    pub fn resume_cursor(&self) -> Option<String> {
        self.state
            .lock()
            .expect("session state mutex")
            .claude_session_id
            .clone()
    }

    /// Emit the fresh-session prelude once, after the handshake reports
    /// the provider session id. Held under the state lock so
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

    /// Emit `session.closed` exactly once, only for announced sessions.
    /// Every exit path - requested stop, provider death, adapter drop -
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

    /// Mark the session dead without emitting `session.closed`: the
    /// driver calls this on every exit so post-mortem commands fail
    /// fast. Terminal facts still flow through `emit` first.
    pub fn mark_closed(&self) {
        let mut state = self.state.lock().expect("session state mutex");
        state.closed = true;
        state.pending.clear();
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
        let inbox = self
            .inbox_rx
            .lock()
            .expect("driver inbox mutex")
            .take()
            .ok_or_else(|| invalid_state("the session driver is already running"))?;
        let handle = thread::Builder::new()
            .name(format!("aifuel-claude-{}", self.integration))
            .spawn(move || super::driver::run(session, connector, setup, setup_tx, inbox))
            .map_err(|error| {
                AgentRuntimeError::provider_error(format!(
                    "could not start the session driver: {error}"
                ))
            })?;
        *self.driver.lock().expect("driver mutex") = Some(handle);
        Ok(())
    }

    /// Start one Agent Run for `input`: post the user message and wait
    /// for the driver's wire-write acknowledgement. The second `send`
    /// on a session fails while a run is in flight.
    pub fn send_turn(&self, input: UserInput) -> Result<RunId, AgentRuntimeError> {
        if input.text.trim().is_empty() {
            return Err(invalid_state("the user input has no text"));
        }
        if !input.attachments.is_empty() {
            return Err(unsupported("this adapter does not carry attachments"));
        }
        let run_id = RunId::new(format!(
            "claude-run-{}-{}",
            std::process::id(),
            self.next_id.fetch_add(1, Ordering::Relaxed)
        ));
        let wait;
        {
            let mut state = self.state.lock().expect("session state mutex");
            if state.closed {
                return Err(invalid_state("the session is closed"));
            }
            if state.active_run.is_some() {
                return Err(invalid_state(
                    "one Agent Run is already in flight for this session",
                ));
            }
            // Reserve the run before posting so a racing send loses.
            state.active_run = Some(run_id.clone());
            let (reply, rx) = mpsc::channel();
            if self
                .inbox
                .send(DriverInput::Command(DriverCommand::UserMessage {
                    run_id: run_id.clone(),
                    text: input.text,
                    reply,
                }))
                .is_err()
            {
                state.active_run = None;
                return Err(AgentRuntimeError::provider_error(
                    "the provider driver is not running",
                ));
            }
            wait = rx;
        }
        match wait.recv_timeout(WRITE_ACK_TIMEOUT) {
            Ok(Ok(())) => Ok(run_id),
            Ok(Err(error)) => {
                self.clear_active_run();
                Err(error)
            }
            Err(RecvTimeoutError::Timeout) => {
                self.clear_active_run();
                Err(AgentRuntimeError::provider_error(
                    "the provider driver did not acknowledge the user message",
                ))
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.clear_active_run();
                Err(AgentRuntimeError::provider_error(
                    "the provider driver ended without acknowledging the user message",
                ))
            }
        }
    }

    /// Deliver a host decision to one pending Approval Request. The
    /// first answer wins; anything else reports `already_resolved`.
    pub fn answer(
        &self,
        request: &RequestId,
        decision: &ApprovalDecision,
    ) -> Result<(), AgentRuntimeError> {
        let (line, interrupt) = {
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
            let answer = protocol::tool_answer(&entry.options, decision)?;
            let line = match &answer {
                protocol::ToolAnswer::Allow => {
                    protocol::permission_response(request.as_str(), true, &entry.input, "")
                }
                protocol::ToolAnswer::Deny { message, .. } => {
                    protocol::permission_response(request.as_str(), false, &entry.input, message)
                }
            };
            let interrupt = matches!(answer, protocol::ToolAnswer::Deny { cancel: true, .. });
            state
                .pending
                .remove(request)
                .expect("the entry was just inspected");
            (line, interrupt)
        };
        let (reply, wait) = mpsc::channel();
        self.inbox
            .send(DriverInput::Command(DriverCommand::Answer {
                request_id: request.clone(),
                decision: decision.clone(),
                line,
                interrupt,
                reply,
            }))
            .map_err(|_| AgentRuntimeError::provider_error("the provider driver is not running"))?;
        match wait.recv_timeout(WRITE_ACK_TIMEOUT) {
            Ok(result) => result,
            Err(_) => Err(AgentRuntimeError::provider_error(
                "the approval answer was not acknowledged",
            )),
        }
    }

    /// Request interruption of the in-flight turn. The provider's
    /// `result` reports the terminal `cancelled` outcome; `cancel` only
    /// confirms the request reached the wire.
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
        self.inbox
            .send(DriverInput::Command(DriverCommand::Interrupt {
                run_id: run.clone(),
                reply,
            }))
            .map_err(|_| AgentRuntimeError::provider_error("the provider driver is not running"))?;
        match wait.recv_timeout(WRITE_ACK_TIMEOUT) {
            Ok(result) => result,
            Err(_) => Err(AgentRuntimeError::provider_error(
                "the interrupt request was not acknowledged",
            )),
        }
    }

    /// Apply a `model.select` resolution. The selection stores only
    /// after the provider wire write lands, so a failed write changes
    /// nothing.
    pub fn set_selection(&self, selection: ModelSelection) -> Result<(), AgentRuntimeError> {
        {
            let state = self.state.lock().expect("session state mutex");
            if state.closed {
                return Err(invalid_state("the session is closed"));
            }
            if state.active_run.is_some() {
                return Err(invalid_state(
                    "the selection cannot change while a run is in flight",
                ));
            }
        }
        let model = (!selection.model.is_empty()).then(|| selection.model.clone());
        let (reply, wait) = mpsc::channel();
        self.inbox
            .send(DriverInput::Command(DriverCommand::SetModel {
                model,
                reply,
            }))
            .map_err(|_| AgentRuntimeError::provider_error("the provider driver is not running"))?;
        match wait.recv_timeout(WRITE_ACK_TIMEOUT) {
            Ok(Ok(())) => {
                self.state.lock().expect("session state mutex").selection = selection;
                Ok(())
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(AgentRuntimeError::provider_error(
                "the model change was not acknowledged",
            )),
        }
    }

    /// Force-kill the claude process so a wedged driver unblocks.
    /// Safe when the driver already reaped the process or the
    /// transport is a test duplex.
    pub fn kill_child(&self) {
        let mut slot = self.child.lock().expect("child mutex");
        if let Some(child) = slot.as_mut() {
            let _ = child.start_kill();
            let _ = child.wait();
        }
    }

    /// Best-effort teardown kick used by adapter drop and failed setup:
    /// mark closed, ask the driver to shut down, and kill the process.
    /// The driver's own teardown emits terminal facts.
    pub fn close_transport(&self) {
        {
            self.state.lock().expect("session state mutex").closed = true;
        }
        let _ = self
            .inbox
            .send(DriverInput::Command(DriverCommand::Shutdown));
        self.kill_child();
    }

    /// Close the session: shut the driver down and wait for it, so the
    /// run's terminal facts and `session.closed` land in order before
    /// the handle is released.
    pub fn shutdown(&self) {
        {
            self.state.lock().expect("session state mutex").closed = true;
        }
        let _ = self
            .inbox
            .send(DriverInput::Command(DriverCommand::Shutdown));
        self.kill_child();
        let driver = self.driver.lock().expect("driver mutex").take();
        let reader = self.reader.lock().expect("reader mutex").take();
        crate::local_adapter::join_bounded(driver);
        crate::local_adapter::join_bounded(reader);
        self.finish_close(None);
    }

    /// Store the reader thread the driver spawned for its transport.
    pub fn store_reader(&self, reader: JoinHandle<()>) {
        *self.reader.lock().expect("reader mutex") = Some(reader);
    }

    /// Store the spawned process so teardown paths can kill it.
    pub fn store_child(&self, child: Box<dyn StdChildWrapper>) {
        *self.child.lock().expect("child mutex") = Some(child);
    }

    /// The sender half the stdout reader clones to forward provider
    /// lines onto the driver's queue.
    pub fn inbox_sender(&self) -> mpsc::Sender<DriverInput> {
        self.inbox.clone()
    }

    fn clear_active_run(&self) {
        self.state.lock().expect("session state mutex").active_run = None;
    }
}

impl Drop for ClaudeSession {
    /// A session dropped without `stop` or adapter drop still owes the
    /// provider process a kill; `shutdown` and `close_transport` take
    /// the graceful paths first.
    fn drop(&mut self) {
        if let Some(child) = self.child.get_mut().expect("child mutex").as_mut() {
            let _ = child.start_kill();
        }
    }
}

impl crate::local_adapter::LocalSession for ClaudeSession {
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
