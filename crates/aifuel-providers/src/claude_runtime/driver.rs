//! The per-session driver: one thread owning the transport, the
//! process lifetime, and every run-scoped emission.
//!
//! The driver consumes one queue mixing adapter commands and provider
//! stdout lines, so everything it emits follows the run's causal
//! order. Writes to provider stdin happen only here, which serializes
//! user messages, control answers, and interrupts against the single
//! stream-json channel.

use super::protocol::{self, Frame, ResultFrame};
use super::session::{
    ClaudeSession, DriverCommand, DriverInput, LineRead, PendingApproval, SessionSetup,
    SetupReport, Transport,
};
use aifuel_core::{
    AgentEventKind, AgentRuntimeError, MessageStream, ReceiptCode, RunId, RunOutcome, SessionStatus,
};
use std::collections::BTreeMap;
use std::io::{self, BufRead, Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// How long the handshake waits for `system`/`init`. A local spawn
/// reports in well under a second; the bound covers provider startup
/// work only.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// The stderr tail kept for diagnostics on unexpected process exit.
const STDERR_TAIL_BYTES: usize = 64 * 1024;
/// One provider line is bounded so a pathological frame cannot fill
/// memory unboundedly.
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// The run-scoped streaming state the driver carries between frames.
/// Shared session state holds only what adapter methods validate; the
/// rest lives here where the causal order is.
struct RunFlow {
    run_id: RunId,
    /// `interrupt` reached the wire: an error result that follows is a
    /// cancellation, not a failure.
    cancel_requested: bool,
    /// `message.delta` streamed on a message stream whose `assistant`
    /// frame has not landed yet.
    assistant_open: bool,
    thinking_open: bool,
    /// Partial deltas already covered this API message's text, so its
    /// `assistant` frame must not re-emit the same content.
    text_streamed: bool,
    thinking_streamed: bool,
    /// tool_use_id to tool name, for `tool.completed`.
    tool_names: BTreeMap<String, String>,
}

/// The driver entry point. Reports setup on `setup_tx`, then serves
/// the queue until shutdown or provider EOF.
pub(super) fn run(
    session: Arc<ClaudeSession>,
    connector: super::session::Connector,
    setup: SessionSetup,
    setup_tx: mpsc::Sender<SetupReport>,
    inbox: mpsc::Receiver<DriverInput>,
) {
    let transport = match connector(&setup) {
        Ok(transport) => transport,
        Err(error) => {
            let _ = setup_tx.send(Err(error.message));
            session.mark_closed();
            return;
        }
    };
    let Transport {
        mut stdin,
        stdout,
        stderr,
        child,
    } = transport;
    if let Some(child) = child {
        session.store_child(child);
    }
    let stderr_tail = drain_stderr(stderr);
    start_reader(session.as_ref(), stdout);

    let initialize = protocol::initialize_request(&session.next_wire_id("aifuel-init"));
    if let Err(error) = write_line(stdin.as_mut(), &initialize) {
        let _ = setup_tx.send(Err(format!(
            "the control handshake could not reach the provider: {error}"
        )));
        session.kill_child();
        session.mark_closed();
        return;
    }
    if let Err(error) = handshake(session.as_ref(), &inbox, &stderr_tail) {
        let _ = setup_tx.send(Err(error));
        session.kill_child();
        session.mark_closed();
        return;
    }
    let _ = setup_tx.send(Ok(()));

    let mut flow: Option<RunFlow> = None;
    loop {
        match inbox.recv() {
            Ok(DriverInput::Line(LineRead::Data(line))) => {
                on_line(&session, &line, &mut flow, stdin.as_mut());
            }
            Ok(DriverInput::Line(LineRead::Eof)) => {
                eof_teardown(&session, &mut flow, &stderr_tail, None);
                break;
            }
            Ok(DriverInput::Line(LineRead::Error(error))) => {
                eof_teardown(&session, &mut flow, &stderr_tail, Some(error));
                break;
            }
            Ok(DriverInput::Command(DriverCommand::Shutdown)) => {
                teardown_run(&session, &mut flow);
                drop(stdin);
                session.kill_child();
                session.finish_close(None);
                break;
            }
            Ok(DriverInput::Command(command)) => {
                on_command(&session, command, &mut flow, stdin.as_mut());
            }
            // Every sender is gone: the session itself is dropped.
            Err(_) => break,
        }
    }
    session.mark_closed();
}

/// Spawn the stdout reader: lines forward onto the same queue commands
/// use, so the driver's order is the provider's order.
fn start_reader(session: &ClaudeSession, mut stdout: Box<dyn BufRead + Send>) {
    let forward = session.inbox_sender();
    let reader = thread::Builder::new()
        .name(format!("aifuel-claude-read-{}", session.integration))
        .spawn(move || read_lines(stdout.as_mut(), forward))
        .ok();
    if let Some(reader) = reader {
        session.store_reader(reader);
    }
}

/// Forward provider stdout lines onto the driver queue. Ends on EOF,
/// on a read error, or when the driver stopped listening.
fn read_lines(stdout: &mut dyn BufRead, forward: mpsc::Sender<DriverInput>) {
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        match read_bounded_line(stdout, &mut bytes) {
            Ok(0) => {
                let _ = forward.send(DriverInput::Line(LineRead::Eof));
                return;
            }
            Ok(_) => {
                let line = String::from_utf8_lossy(&bytes);
                let line = line.trim_end_matches(['\n', '\r']);
                if line.is_empty() {
                    continue;
                }
                if forward
                    .send(DriverInput::Line(LineRead::Data(line.to_owned())))
                    .is_err()
                {
                    return;
                }
            }
            Err(error) => {
                let _ = forward.send(DriverInput::Line(LineRead::Error(error.to_string())));
                return;
            }
        }
    }
}

/// `BufRead` line read with a length bound, built on `fill_buf` so a
/// split UTF-8 sequence is never mangled: bytes collect until the
/// newline, then decode once.
fn read_bounded_line(stdout: &mut dyn BufRead, bytes: &mut Vec<u8>) -> io::Result<usize> {
    loop {
        let available = stdout.fill_buf()?;
        if available.is_empty() {
            return Ok(if bytes.is_empty() { 0 } else { bytes.len() });
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|pos| pos + 1)
            .unwrap_or(available.len());
        bytes.extend_from_slice(&available[..take]);
        let found_newline = bytes.ends_with(b"\n");
        stdout.consume(take);
        if bytes.len() > MAX_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a provider frame exceeded the line bound",
            ));
        }
        if found_newline {
            return Ok(bytes.len());
        }
    }
}

/// Drain provider stderr into a bounded tail for post-mortem
/// diagnostics. The drain thread ends with the process.
fn drain_stderr(stderr: Option<Box<dyn Read + Send>>) -> Arc<Mutex<String>> {
    let tail = Arc::new(Mutex::new(String::new()));
    if let Some(mut stderr) = stderr {
        let tail = Arc::clone(&tail);
        thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            loop {
                match stderr.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(count) => {
                        let mut tail = tail.lock().expect("stderr tail mutex");
                        tail.push_str(&String::from_utf8_lossy(&buffer[..count]));
                        if tail.len() > STDERR_TAIL_BYTES {
                            let keep = tail.len() - STDERR_TAIL_BYTES;
                            tail.drain(..keep);
                        }
                    }
                }
            }
        });
    }
    tail
}

/// Wait for the provider's `system`/`init` frame, which carries the
/// claude session id this adapter uses as the resume cursor. Frames
/// the CLI emits before init are safe to skip here; the session handle
/// does not exist yet, so nothing can observe them.
fn handshake(
    session: &ClaudeSession,
    inbox: &mpsc::Receiver<DriverInput>,
    stderr_tail: &Mutex<String>,
) -> Result<(), String> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("the provider did not report its session id in time".to_owned());
        }
        match inbox.recv_timeout(remaining) {
            Ok(DriverInput::Line(LineRead::Data(line))) => {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                match protocol::parse_frame(&value) {
                    Frame::Init { session_id } => {
                        session
                            .state
                            .lock()
                            .expect("session state mutex")
                            .claude_session_id = session_id;
                        return Ok(());
                    }
                    Frame::Error(message) => return Err(message),
                    Frame::Result(result) if result.is_error => {
                        return Err(result
                            .message
                            .unwrap_or_else(|| "the provider session failed to start".to_owned()));
                    }
                    _ => {}
                }
            }
            Ok(DriverInput::Line(LineRead::Eof | LineRead::Error(_))) => {
                return Err(format!(
                    "the provider process ended before reporting its session id{}",
                    stderr_hint(stderr_tail)
                ));
            }
            // Commands cannot arrive before the session is announced;
            // a shutdown racing setup still tears the driver down.
            Ok(DriverInput::Command(DriverCommand::Shutdown)) => {
                return Err("the session was closed during startup".to_owned());
            }
            Ok(DriverInput::Command(_)) => {}
            Err(RecvTimeoutError::Timeout) => {
                return Err("the provider did not report its session id in time".to_owned());
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err("the provider channel ended during startup".to_owned());
            }
        }
    }
}

/// One provider line: parse to a frame and dispatch it.
fn on_line(session: &ClaudeSession, line: &str, flow: &mut Option<RunFlow>, stdin: &mut dyn Write) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    match protocol::parse_frame(&value) {
        Frame::Init { session_id } => {
            if let Some(session_id) = session_id {
                session
                    .state
                    .lock()
                    .expect("session state mutex")
                    .claude_session_id = Some(session_id);
            }
        }
        Frame::Status(status) => on_status(session, flow.as_ref(), &status),
        Frame::Assistant { blocks } => on_assistant(session, flow.as_mut(), blocks),
        Frame::ToolResults(results) => on_tool_results(session, flow.as_mut(), results),
        Frame::TextDelta(text) => on_delta(session, flow.as_mut(), MessageStream::Assistant, text),
        Frame::ThinkingDelta(text) => {
            on_delta(session, flow.as_mut(), MessageStream::Thinking, text);
        }
        Frame::StreamToolStart { tool_use_id, name } => {
            if let Some(flow) = flow.as_mut() {
                flow.tool_names.insert(tool_use_id, name);
            }
        }
        Frame::ControlRequest {
            request_id,
            request,
        } => {
            on_control_request(session, flow, request_id, request, stdin);
        }
        Frame::ControlCancel { request_id } => {
            session
                .state
                .lock()
                .expect("session state mutex")
                .pending
                .remove(&request_id);
        }
        Frame::Result(result) => finish_run(session, flow, result),
        Frame::Quota(quota) => session.emit(AgentEventKind::QuotaObserved {
            integration_id: session.integration.clone(),
            quota,
        }),
        Frame::Error(message) => session.emit(AgentEventKind::Error {
            run_id: flow.as_ref().map(|flow| flow.run_id.clone()),
            code: ReceiptCode::ProviderError,
            message,
            retryable: false,
        }),
        Frame::Ignored => {}
    }
}

fn on_status(session: &ClaudeSession, flow: Option<&RunFlow>, status: &str) {
    if flow.is_none() {
        return;
    }
    let mapped = match status {
        "compacting" => Some(SessionStatus::Compacting),
        // The provider is working again after a compacting stretch or
        // an approval wait; `working` is the honest session status.
        "requesting" => Some(SessionStatus::Working),
        _ => None,
    };
    if let Some(status) = mapped {
        session.emit(AgentEventKind::SessionStatus { status });
    }
}

fn on_delta(
    session: &ClaudeSession,
    flow: Option<&mut RunFlow>,
    stream: MessageStream,
    text: String,
) {
    let Some(flow) = flow else { return };
    match stream {
        MessageStream::Assistant => {
            flow.text_streamed = true;
            flow.assistant_open = true;
        }
        MessageStream::Thinking => {
            flow.thinking_streamed = true;
            flow.thinking_open = true;
        }
    }
    session.emit(AgentEventKind::MessageDelta {
        run_id: flow.run_id.clone(),
        stream,
        text,
    });
}

/// One completed `assistant` message. Text and thinking blocks that
/// already streamed as partial deltas are not re-emitted; without
/// partial events the full text lands here as the fallback path the
/// contract requires.
fn on_assistant(session: &ClaudeSession, flow: Option<&mut RunFlow>, blocks: Vec<protocol::Block>) {
    let Some(flow) = flow else { return };
    let mut events = Vec::new();
    for block in blocks {
        match block {
            protocol::Block::Text(text) => {
                if !flow.text_streamed && !text.is_empty() {
                    flow.assistant_open = true;
                    events.push(AgentEventKind::MessageDelta {
                        run_id: flow.run_id.clone(),
                        stream: MessageStream::Assistant,
                        text,
                    });
                }
            }
            protocol::Block::Thinking(thinking) => {
                if !flow.thinking_streamed && !thinking.is_empty() {
                    flow.thinking_open = true;
                    events.push(AgentEventKind::MessageDelta {
                        run_id: flow.run_id.clone(),
                        stream: MessageStream::Thinking,
                        text: thinking,
                    });
                }
            }
            protocol::Block::ToolUse {
                tool_use_id,
                name,
                input,
            } => {
                flow.tool_names.insert(tool_use_id, name.clone());
                events.push(AgentEventKind::ToolStarted {
                    run_id: flow.run_id.clone(),
                    summary: protocol::tool_summary(&name, &input),
                    tool: name,
                });
            }
        }
    }
    close_streams(flow, &mut events);
    // The next API message streams fresh deltas; the streamed flags
    // describe exactly one provider message.
    flow.text_streamed = false;
    flow.thinking_streamed = false;
    for event in events {
        session.emit(event);
    }
}

fn on_tool_results(
    session: &ClaudeSession,
    flow: Option<&mut RunFlow>,
    results: Vec<protocol::ToolResultBlock>,
) {
    let Some(flow) = flow else { return };
    for result in results {
        let tool = flow
            .tool_names
            .get(&result.tool_use_id)
            .cloned()
            .unwrap_or_else(|| "tool".to_owned());
        session.emit(AgentEventKind::ToolCompleted {
            run_id: flow.run_id.clone(),
            tool,
            ok: result.ok,
            diff: None,
            output: result.output,
        });
    }
}

/// A CLI-initiated control request. `can_use_tool` becomes a typed
/// Approval Request and the turn waits on `answer`; every other
/// subtype gets an explicit error reply so the provider never blocks
/// on a request this adapter cannot honor.
fn on_control_request(
    session: &ClaudeSession,
    flow: &mut Option<RunFlow>,
    request_id: String,
    request: serde_json::Value,
    stdin: &mut dyn Write,
) {
    let subtype = request
        .get("subtype")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if subtype != "can_use_tool" {
        let _ = write_line(
            stdin,
            &protocol::error_response(
                &request_id,
                &format!("control request {subtype:?} is not supported"),
            ),
        );
        return;
    }
    let Some(flow) = flow.as_mut() else {
        let _ = write_line(
            stdin,
            &protocol::error_response(&request_id, "no Agent Run is in flight"),
        );
        return;
    };
    let tool_name = request
        .get("tool_name")
        .or_else(|| request.get("display_name"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("tool")
        .to_owned();
    let input = request
        .get("input")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    if let Some(tool_use_id) = request
        .get("tool_use_id")
        .and_then(serde_json::Value::as_str)
    {
        // The tool_result that follows names its tool through this id.
        flow.tool_names
            .insert(tool_use_id.to_owned(), tool_name.clone());
    }
    let approval = protocol::approval_request(&request, session.access);
    let options: Vec<String> = approval
        .options
        .iter()
        .map(|option| option.id.clone())
        .collect();
    session
        .state
        .lock()
        .expect("session state mutex")
        .pending
        .insert(request_id.clone(), PendingApproval { input, options });
    session.emit(AgentEventKind::SessionStatus {
        status: SessionStatus::WaitingApproval,
    });
    session.emit(AgentEventKind::ApprovalRequested {
        run_id: flow.run_id.clone(),
        request_id: aifuel_core::RequestId::new(request_id),
        request: approval,
    });
}

/// The terminal fact of a run: close open message streams, report the
/// outcome, then idle. A stream ending without this is a failure, and
/// this emission is what makes a run terminal.
fn finish_run(session: &ClaudeSession, flow: &mut Option<RunFlow>, result: ResultFrame) {
    let Some(mut flow) = flow.take() else {
        return;
    };
    if let Some(session_id) = result.session_id.clone() {
        session
            .state
            .lock()
            .expect("session state mutex")
            .claude_session_id = Some(session_id);
    }
    let mut events = Vec::new();
    close_streams(&mut flow, &mut events);
    for event in events {
        session.emit(event);
    }
    let cancelled = protocol::is_aborted(&result) || (flow.cancel_requested && result.is_error);
    let outcome = if cancelled {
        RunOutcome::Cancelled
    } else if result.is_error {
        session.emit(AgentEventKind::Error {
            run_id: Some(flow.run_id.clone()),
            code: ReceiptCode::ProviderError,
            message: result
                .message
                .clone()
                .unwrap_or_else(|| "the provider reported a failed run".to_owned()),
            retryable: false,
        });
        RunOutcome::Failed
    } else {
        RunOutcome::Success
    };
    session.emit(AgentEventKind::RunCompleted {
        run_id: flow.run_id.clone(),
        outcome,
        usage: result.usage,
    });
    let closed = {
        let mut state = session.state.lock().expect("session state mutex");
        state.active_run = None;
        state.pending.clear();
        state.closed
    };
    if !closed {
        session.emit(AgentEventKind::SessionStatus {
            status: SessionStatus::Idle,
        });
    }
}

/// One adapter command. `Shutdown` stays in the driver loop: it owns
/// the stdin box it must drop to close the provider's input.
fn on_command(
    session: &ClaudeSession,
    command: DriverCommand,
    flow: &mut Option<RunFlow>,
    stdin: &mut dyn Write,
) {
    match command {
        DriverCommand::UserMessage {
            run_id,
            text,
            reply,
        } => {
            *flow = Some(RunFlow {
                run_id: run_id.clone(),
                cancel_requested: false,
                assistant_open: false,
                thinking_open: false,
                text_streamed: false,
                thinking_streamed: false,
                tool_names: BTreeMap::new(),
            });
            let selection = session
                .state
                .lock()
                .expect("session state mutex")
                .selection
                .clone();
            session.emit(AgentEventKind::RunStarted {
                run_id: run_id.clone(),
                selection,
            });
            session.emit(AgentEventKind::SessionStatus {
                status: SessionStatus::Working,
            });
            let session_id = session
                .state
                .lock()
                .expect("session state mutex")
                .claude_session_id
                .clone();
            let line = protocol::user_message(session_id.as_deref(), &text);
            let result = match write_line(stdin, &line) {
                Ok(()) => Ok(()),
                Err(error) => {
                    let flow = flow.take().expect("the run was just registered");
                    session.emit(AgentEventKind::Error {
                        run_id: Some(run_id.clone()),
                        code: ReceiptCode::ProviderError,
                        message: format!("the user message could not reach the provider: {error}"),
                        retryable: false,
                    });
                    session.emit(AgentEventKind::RunCompleted {
                        run_id: flow.run_id,
                        outcome: RunOutcome::Failed,
                        usage: None,
                    });
                    session
                        .state
                        .lock()
                        .expect("session state mutex")
                        .active_run = None;
                    Err(AgentRuntimeError::provider_error(format!(
                        "the user message could not reach the provider: {error}"
                    )))
                }
            };
            let _ = reply.send(result);
        }
        DriverCommand::Answer {
            request_id,
            decision,
            line,
            interrupt,
            reply,
        } => {
            let result = match write_line(stdin, &line) {
                Ok(()) => {
                    session.emit(AgentEventKind::ApprovalResolved {
                        request_id: request_id.clone(),
                        decision,
                        // Placeholder the runtime pump rewrites with the
                        // answering consumer id.
                        answered_by: "adapter".to_owned(),
                    });
                    session.emit(AgentEventKind::SessionStatus {
                        status: SessionStatus::Working,
                    });
                    if interrupt {
                        let line =
                            protocol::interrupt_request(&session.next_wire_id("aifuel-interrupt"));
                        if write_line(stdin, &line).is_ok()
                            && let Some(flow) = flow.as_mut()
                        {
                            flow.cancel_requested = true;
                        }
                    }
                    Ok(())
                }
                Err(error) => Err(AgentRuntimeError::provider_error(format!(
                    "the approval answer could not reach the provider: {error}"
                ))),
            };
            let _ = reply.send(result);
        }
        DriverCommand::Interrupt { run_id, reply } => {
            let line = protocol::interrupt_request(&session.next_wire_id("aifuel-interrupt"));
            let result = match write_line(stdin, &line) {
                Ok(()) => {
                    if let Some(flow) = flow.as_mut()
                        && flow.run_id == run_id
                    {
                        flow.cancel_requested = true;
                    }
                    Ok(())
                }
                Err(error) => Err(AgentRuntimeError::provider_error(format!(
                    "the interrupt could not reach the provider: {error}"
                ))),
            };
            let _ = reply.send(result);
        }
        DriverCommand::SetModel { model, reply } => {
            let line = protocol::set_model_request(
                &session.next_wire_id("aifuel-model"),
                model.as_deref(),
            );
            let result = write_line(stdin, &line).map_err(|error| {
                AgentRuntimeError::provider_error(format!(
                    "the model change could not reach the provider: {error}"
                ))
            });
            let _ = reply.send(result);
        }
        DriverCommand::Shutdown => {}
    }
}

/// The provider stdout ended. Whether the teardown is a requested
/// close or a lost provider is read from `state.closed`: adapter
/// teardown paths set it before killing the process.
fn eof_teardown(
    session: &ClaudeSession,
    flow: &mut Option<RunFlow>,
    stderr_tail: &Mutex<String>,
    read_error: Option<String>,
) {
    let requested = {
        let mut state = session.state.lock().expect("session state mutex");
        let requested = state.closed;
        state.closed = true;
        requested
    };
    if requested {
        teardown_run(session, flow);
        session.finish_close(None);
        return;
    }
    if let Some(mut flow) = flow.take() {
        let mut events = Vec::new();
        close_streams(&mut flow, &mut events);
        for event in events {
            session.emit(event);
        }
        session.emit(AgentEventKind::Error {
            run_id: Some(flow.run_id.clone()),
            code: ReceiptCode::ProviderError,
            message: format!(
                "the provider process ended during the run{}{}",
                read_error
                    .map(|error| format!("; stdout: {error}"))
                    .unwrap_or_default(),
                stderr_hint(stderr_tail)
            ),
            retryable: false,
        });
        session.emit(AgentEventKind::RunCompleted {
            run_id: flow.run_id,
            outcome: RunOutcome::Failed,
            usage: None,
        });
    }
    {
        let mut state = session.state.lock().expect("session state mutex");
        state.active_run = None;
        state.pending.clear();
    }
    session.emit(AgentEventKind::SessionStatus {
        status: SessionStatus::Interrupted,
    });
    session.finish_close(Some("the provider process ended".to_owned()));
}

/// End the in-flight run's facts on a requested teardown: close
/// streams, report `cancelled`, clear shared run state.
fn teardown_run(session: &ClaudeSession, flow: &mut Option<RunFlow>) {
    let Some(mut flow) = flow.take() else { return };
    let mut events = Vec::new();
    close_streams(&mut flow, &mut events);
    for event in events {
        session.emit(event);
    }
    session.emit(AgentEventKind::RunCompleted {
        run_id: flow.run_id.clone(),
        outcome: RunOutcome::Cancelled,
        usage: None,
    });
    let mut state = session.state.lock().expect("session state mutex");
    state.active_run = None;
    state.pending.clear();
}

/// Close every open message stream, so no `message.delta` is left
/// without its `message.completed`.
fn close_streams(flow: &mut RunFlow, events: &mut Vec<AgentEventKind>) {
    if flow.assistant_open {
        events.push(AgentEventKind::MessageCompleted {
            run_id: flow.run_id.clone(),
            stream: MessageStream::Assistant,
        });
        flow.assistant_open = false;
    }
    if flow.thinking_open {
        events.push(AgentEventKind::MessageCompleted {
            run_id: flow.run_id.clone(),
            stream: MessageStream::Thinking,
        });
        flow.thinking_open = false;
    }
}

fn write_line(stdin: &mut dyn Write, line: &str) -> io::Result<()> {
    stdin.write_all(line.as_bytes())?;
    stdin.write_all(b"\n")?;
    stdin.flush()
}

/// A stderr tail hint for post-mortem error facts; empty when the
/// provider died silently.
fn stderr_hint(stderr_tail: &Mutex<String>) -> String {
    let tail = stderr_tail
        .lock()
        .map(|tail| tail.trim().to_owned())
        .unwrap_or_default();
    if tail.is_empty() {
        String::new()
    } else {
        format!("; stderr tail: {tail}")
    }
}
