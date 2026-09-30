//! The per-session driver: one thread owning the transport, the
//! process lifetime, and every run-scoped emission.
//!
//! The driver consumes one queue mixing adapter commands and provider
//! stdout lines, so everything it emits follows the run's causal
//! order. Writes to provider stdin happen only here, which serializes
//! user messages, control answers, and interrupts against the single
//! stream-json channel.

mod dispatch;
mod handshake;
mod teardown;

use super::session::{
    ClaudeSession, DriverCommand, DriverInput, LineRead, SessionSetup, SetupReport, Transport,
};
use aifuel_core::{AgentEventKind, MessageStream, RunId};
use std::collections::BTreeMap;
use std::io::{self, BufRead, Read, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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

    if let Err(error) = handshake::handshake(session.as_ref(), &inbox, stdin.as_mut(), &stderr_tail)
    {
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
                dispatch::on_line(&session, &line, &mut flow, stdin.as_mut());
            }
            Ok(DriverInput::Line(LineRead::Eof)) => {
                teardown::eof_teardown(&session, &mut flow, &stderr_tail, None);
                break;
            }
            Ok(DriverInput::Line(LineRead::Error(error))) => {
                teardown::eof_teardown(&session, &mut flow, &stderr_tail, Some(error));
                break;
            }
            Ok(DriverInput::Command(DriverCommand::Shutdown)) => {
                teardown::teardown_run(&session, &mut flow);
                drop(stdin);
                session.kill_child();
                session.finish_close(None);
                break;
            }
            Ok(DriverInput::Command(command)) => {
                dispatch::on_command(&session, command, &mut flow, stdin.as_mut());
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
