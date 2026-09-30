//! Tests for the Claude stream-json adapter, run against a scripted
//! transport double so no real `claude` process is involved. The fake
//! speaks plain stdin/stdout JSON lines: every line the driver writes
//! is recorded, and the script plays provider frames back through a
//! channel-driven reader.

mod protocol;
mod session;

use super::*;
use aifuel_core::{
    AccessMode, AgentAuthenticationEvidence, AgentAuthenticationState, AgentEventKind,
    AgentIntegrationInfo, AgentPresenceEvidence, AgentPresenceState, AgentSessionHandle,
    AgentVersionEvidence, CliAdapterId, ExecutionConfig, Integration, IntegrationId,
    ModelSelection, ProviderId, ProviderKey, SessionStatus, StartOptions, UserInput,
};
use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The scripted provider-side player: it sees each line the driver
/// wrote and pushes reply frames through the channel sender.
type Responder = Arc<Mutex<Box<dyn FnMut(&str, &mpsc::Sender<String>) + Send>>>;

/// One scripted provider endpoint. `written` records every line the
/// driver pushed to provider stdin; `line` sends provider stdout
/// frames; the flags model process exit and stdin close.
pub(super) struct Script {
    /// Every complete line written to provider stdin, in order.
    pub written: Arc<Mutex<Vec<String>>>,
    line: mpsc::Sender<String>,
    /// The provider "process" ended: the stdout reader reports EOF.
    dead: Arc<AtomicBool>,
    /// Provider stdin was closed (the driver dropped its writer).
    pub stdin_closed: Arc<AtomicBool>,
    /// Played on each written line: scripted provider replies.
    respond: Responder,
}

impl Script {
    /// Push one provider stdout frame (one JSON object).
    pub fn push_line(&self, line: &str) {
        let _ = self.line.send(line.to_owned());
    }

    /// Simulate the provider process dying mid-session: the stdout
    /// reader reports EOF.
    pub fn kill_provider(&self) {
        self.dead.store(true, Ordering::SeqCst);
    }

    /// All lines the driver wrote to provider stdin so far.
    pub fn written_lines(&self) -> Vec<String> {
        self.written.lock().expect("written lines mutex").clone()
    }
}

/// Build a scripted duplex and the connector that hands it to a
/// session. `respond` plays the provider's side: it sees every written
/// line and pushes reply frames through the sender.
pub(super) fn scripted(
    respond: impl FnMut(&str, &mpsc::Sender<String>) + Send + 'static,
) -> (Arc<Script>, super::session::Connector) {
    let (line_tx, line_rx) = mpsc::channel();
    let script = Arc::new(Script {
        written: Arc::new(Mutex::new(Vec::new())),
        line: line_tx.clone(),
        dead: Arc::new(AtomicBool::new(false)),
        stdin_closed: Arc::new(AtomicBool::new(false)),
        respond: Arc::new(Mutex::new(Box::new(respond))),
    });
    let endpoint = Arc::clone(&script);
    let line_rx = Mutex::new(Some(line_rx));
    let connector: super::session::Connector = Arc::new(move |_setup| {
        let rx = line_rx
            .lock()
            .expect("scripted receiver mutex")
            .take()
            .expect("a scripted endpoint serves one session");
        Ok(super::session::Transport {
            stdin: Box::new(FakeStdin {
                pending: Vec::new(),
                written: Arc::clone(&endpoint.written),
                respond: Arc::clone(&endpoint.respond),
                line: line_tx.clone(),
                closed: Arc::clone(&endpoint.stdin_closed),
                dead: Arc::clone(&endpoint.dead),
            }),
            stdout: Box::new(ChannelReader {
                rx,
                buffer: Vec::new(),
                position: 0,
                dead: Arc::clone(&endpoint.dead),
            }),
            stderr: None,
            child: None,
        })
    });
    (script, connector)
}

/// The scripted stdin half. Buffers partial writes, and on `flush`
/// records each completed line and plays the scripted reply.
struct FakeStdin {
    pending: Vec<u8>,
    written: Arc<Mutex<Vec<String>>>,
    respond: Responder,
    line: mpsc::Sender<String>,
    closed: Arc<AtomicBool>,
    dead: Arc<AtomicBool>,
}

impl Write for FakeStdin {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let pending = std::mem::take(&mut self.pending);
        for line in String::from_utf8_lossy(&pending).lines() {
            self.written
                .lock()
                .expect("written lines mutex")
                .push(line.to_owned());
            (self.respond.lock().expect("respond mutex"))(line, &self.line);
        }
        Ok(())
    }
}

impl Drop for FakeStdin {
    /// Dropping the writer closes provider stdin; a real provider
    /// exits on stdin EOF, so the scripted endpoint dies too.
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        self.dead.store(true, Ordering::SeqCst);
    }
}

/// The scripted stdout half: a channel-driven `BufRead` that reports
/// queued lines, EOF on channel close, and EOF when the scripted
/// process is marked dead.
struct ChannelReader {
    rx: mpsc::Receiver<String>,
    buffer: Vec<u8>,
    position: usize,
    dead: Arc<AtomicBool>,
}

impl ChannelReader {
    fn pull(&mut self) -> io::Result<()> {
        loop {
            if self.dead.load(Ordering::SeqCst) {
                return Ok(());
            }
            match self.rx.recv_timeout(Duration::from_millis(25)) {
                Ok(line) => {
                    self.buffer.extend_from_slice(line.as_bytes());
                    self.buffer.push(b'\n');
                    return Ok(());
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let take = available.len().min(buf.len());
        buf[..take].copy_from_slice(&available[..take]);
        self.consume(take);
        Ok(take)
    }
}

impl BufRead for ChannelReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.position >= self.buffer.len() {
            self.buffer.clear();
            self.position = 0;
            self.pull()?;
        }
        Ok(&self.buffer[self.position..])
    }

    fn consume(&mut self, amount: usize) {
        self.position += amount;
    }
}

/// The `request_id` the driver stamped on a control request line it
/// wrote; the scripted provider echoes it back in its answer.
fn request_id_of(line: &str) -> String {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|value| {
            value
                .get("request_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

/// The `control_response` success answer a control request gets.
fn control_ok(request_id: &str) -> String {
    format!(
        "{{\"type\":\"control_response\",\"response\":{{\"subtype\":\"success\",\"request_id\":\"{request_id}\"}}}}"
    )
}

/// The `system`/`init` frame carrying the provider session id.
fn init_frame(session_id: &str) -> String {
    format!(
        "{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"{session_id}\",\"model\":\"claude-test\",\"cwd\":\"/tmp\",\"permissionMode\":\"default\"}}"
    )
}

/// A SessionStart `system`/`hook_*` frame, which carries the provider
/// session id before `system`/`init` exists.
fn hook_frame(subtype: &str, session_id: &str) -> String {
    format!(
        "{{\"type\":\"system\",\"subtype\":\"{subtype}\",\"session_id\":\"{session_id}\",\"hook_name\":\"SessionStart hook\",\"hook_event\":\"SessionStart\"}}"
    )
}

/// The scripted initialize handshake every session needs before
/// `start` returns: on the driver's `initialize` control request,
/// report the `system`/`init` frame carrying the provider session id
/// and answer the request - the order versions that emit init at
/// startup produce.
pub(super) fn init_replies(request_id: &str, session_id: &str) -> Vec<String> {
    vec![init_frame(session_id), control_ok(request_id)]
}

/// A `respond` closure that always plays the initialize handshake,
/// then delegates user-message writes to `on_user`, which pushes
/// provider frames through the `push` function it receives.
pub(super) fn with_init(
    session_id: &'static str,
    on_user: impl Fn(&dyn Fn(&str)) + Send + 'static,
) -> impl FnMut(&str, &mpsc::Sender<String>) + Send {
    with_init_then(session_id, on_user, |_| {})
}

/// `with_init` plus `on_response`, played for each `control_response`
/// the host writes. Permission turns only finish after the host
/// answers, so the `result` frame belongs on this hook.
pub(super) fn with_init_then(
    session_id: &'static str,
    on_user: impl Fn(&dyn Fn(&str)) + Send + 'static,
    on_response: impl Fn(&dyn Fn(&str)) + Send + 'static,
) -> impl FnMut(&str, &mpsc::Sender<String>) + Send {
    move |line, tx| {
        let push = |line: &str| {
            let _ = tx.send(line.to_owned());
        };
        if line.contains("\"initialize\"") {
            for reply in init_replies(&request_id_of(line), session_id) {
                push(&reply);
            }
        } else if line.contains("\"type\":\"user\"") {
            on_user(&push);
        } else if line.contains("\"control_response\"") {
            on_response(&push);
        }
    }
}

/// The claude 2.1.284 startup shape on a hookless machine: only the
/// `initialize` answer arrives up front, and `system`/`init` follows
/// the first user message rather than completing the handshake.
pub(super) fn late_init(
    session_id: &'static str,
    on_user: impl Fn(&dyn Fn(&str)) + Send + 'static,
) -> impl FnMut(&str, &mpsc::Sender<String>) + Send {
    late_init_with_prelude(session_id, Vec::new(), on_user)
}

/// `late_init` on a machine with SessionStart hooks: `hook_started`
/// and `hook_response` frames, which carry the provider session id,
/// precede the initialize answer.
pub(super) fn late_init_with_hooks(
    session_id: &'static str,
    on_user: impl Fn(&dyn Fn(&str)) + Send + 'static,
) -> impl FnMut(&str, &mpsc::Sender<String>) + Send {
    late_init_with_prelude(
        session_id,
        vec![
            hook_frame("hook_started", session_id),
            hook_frame("hook_response", session_id),
        ],
        on_user,
    )
}

/// The shared body of the late-init fixtures: `prelude` plays the
/// frames a real CLI emits ahead of the initialize answer, then the
/// answer lands; `system`/`init` waits for the first `user` write.
fn late_init_with_prelude(
    session_id: &'static str,
    prelude: Vec<String>,
    on_user: impl Fn(&dyn Fn(&str)) + Send + 'static,
) -> impl FnMut(&str, &mpsc::Sender<String>) + Send {
    let mut init_sent = false;
    move |line, tx| {
        let push = |line: &str| {
            let _ = tx.send(line.to_owned());
        };
        if line.contains("\"initialize\"") {
            for frame in &prelude {
                push(frame);
            }
            push(&control_ok(&request_id_of(line)));
        } else if line.contains("\"type\":\"user\"") {
            if !init_sent {
                init_sent = true;
                push(&init_frame(session_id));
            }
            on_user(&push);
        }
    }
}

/// `claude` native inspection evidence: present, unknown auth. It
/// keeps `availability()` at `unknown` so `start` passes its gate
/// without probing a real binary.
fn fake_agent_info() -> AgentIntegrationInfo {
    let provider = ProviderId::from(ProviderKey::Claude);
    AgentIntegrationInfo::from_inspection(
        provider.clone(),
        IntegrationId::new(provider.as_str()),
        AgentPresenceEvidence {
            state: AgentPresenceState::Present,
            reason: "fake".to_owned(),
        },
        AgentVersionEvidence {
            version: Some("2.1.284".to_owned()),
            reason: "fake".to_owned(),
        },
        AgentAuthenticationEvidence {
            state: AgentAuthenticationState::Unknown,
            reason: "fake".to_owned(),
        },
        Vec::new(),
    )
}

/// Build a test adapter over the connector a `scripted` call
/// returned.
pub(super) fn adapter_with(connector: super::session::Connector) -> ClaudeAdapter {
    ClaudeAdapter::new()
        .with_connector(connector)
        .with_agent_info(fake_agent_info())
}

pub(super) fn integration() -> Integration {
    let id = ProviderKey::Claude.as_str();
    Integration {
        id: IntegrationId::new(id),
        provider: ProviderId::new(id),
        name: format!("Test {id}"),
        execution: ExecutionConfig::Cli {
            adapter: CliAdapterId::new(id),
        },
        monitoring: None,
    }
}

pub(super) fn options(access: AccessMode) -> StartOptions {
    StartOptions {
        cwd: PathBuf::from("/tmp"),
        selection: ModelSelection {
            integration_id: IntegrationId::new(ProviderKey::Claude.as_str()),
            model: "claude-test-model".to_owned(),
            effort: None,
        },
        access,
        resume_cursor: None,
        external_tools: Vec::new(),
    }
}

pub(super) fn input(text: &str) -> UserInput {
    UserInput {
        text: text.to_owned(),
        attachments: Vec::new(),
    }
}

/// Bounded read so a missing event fails the test instead of hanging
/// it.
pub(super) fn recv(events: &mpsc::Receiver<AgentEventKind>) -> AgentEventKind {
    events
        .recv_timeout(Duration::from_secs(10))
        .expect("the next event arrives")
}

/// Read until `run.completed` (inclusive) and return every event seen.
pub(super) fn through_run_completed(
    events: &mpsc::Receiver<AgentEventKind>,
) -> Vec<AgentEventKind> {
    let mut kinds = Vec::new();
    for _ in 0..32 {
        let kind = recv(events);
        let terminal = matches!(kind, AgentEventKind::RunCompleted { .. });
        kinds.push(kind);
        if terminal {
            return kinds;
        }
    }
    panic!("run.completed never arrived: {kinds:?}")
}

/// Read until the session returns to `idle` after a completed run.
pub(super) fn through_idle(events: &mpsc::Receiver<AgentEventKind>) -> Vec<AgentEventKind> {
    let mut kinds = through_run_completed(events);
    for _ in 0..8 {
        let kind = recv(events);
        let idle = matches!(
            kind,
            AgentEventKind::SessionStatus {
                status: SessionStatus::Idle
            }
        );
        kinds.push(kind);
        if idle {
            return kinds;
        }
    }
    panic!("the session never returned to idle: {kinds:?}")
}

pub(super) fn take_events(
    adapter: &ClaudeAdapter,
    handle: &AgentSessionHandle,
) -> mpsc::Receiver<AgentEventKind> {
    adapter
        .test_events(handle)
        .expect("the session's event receiver is attached")
}
