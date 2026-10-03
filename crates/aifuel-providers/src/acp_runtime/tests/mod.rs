//! Tests for the ACP adapter, run against a scripted agent double on a
//! duplex transport so no real provider process is involved.

use super::session::{SessionSetup, Transport};
use super::*;
use aifuel_core::{
    AccessMode, AgentAuthenticationEvidence, AgentAuthenticationState, AgentEventKind,
    AgentIntegrationInfo, AgentPresenceEvidence, AgentPresenceState, AgentSessionHandle,
    AgentVersionEvidence, CliAdapterId, ExecutionConfig, Integration, IntegrationId,
    ModelSelection, ProviderId, SessionStatus, StartOptions, UserInput,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader, DuplexStream};

mod approvals;
mod attachments;
mod runs;
mod session;

/// The agent end of the duplex transport, scripted by tests.
pub(super) struct FakeAgent {
    reader: BufReader<tokio::io::ReadHalf<DuplexStream>>,
    writer: tokio::io::WriteHalf<DuplexStream>,
}

impl FakeAgent {
    /// The next complete JSONL frame the adapter wrote, or `None` at
    /// transport EOF.
    pub async fn next_client_opt(&mut self) -> Option<Value> {
        let mut line = Vec::new();
        let read = self
            .reader
            .read_until(b'\n', &mut line)
            .await
            .expect("read the client frame");
        if read == 0 {
            return None;
        }
        Some(serde_json::from_slice(&line).expect("client frames are JSON"))
    }

    pub async fn next_client(&mut self) -> Value {
        self.next_client_opt()
            .await
            .expect("a client frame arrives")
    }

    /// Read client frames until one carries `method`; returns the whole
    /// message so scripts can assert on its params.
    pub async fn next_method(&mut self, method: &str) -> Value {
        loop {
            let message = self.next_client().await;
            if message["method"].as_str() == Some(method) {
                return message;
            }
        }
    }

    pub async fn write(&mut self, message: Value) {
        let mut bytes = serde_json::to_vec(&message).expect("json");
        bytes.push(b'\n');
        self.writer
            .write_all(&bytes)
            .await
            .expect("write the agent frame");
        self.writer.flush().await.expect("flush the agent frame");
    }

    /// Answer the client request `request` with `result`.
    pub async fn respond(&mut self, request: &Value, result: Value) {
        self.write(json!({"id": request["id"], "result": result}))
            .await;
    }

    /// Answer `initialize` with a full result payload and return the
    /// request, so tests can drive `authenticate`, `session/new`, and
    /// `session/load` by hand.
    pub async fn initialize(&mut self, result: Value) -> Value {
        let init = self.next_method("initialize").await;
        self.respond(&init, result).await;
        init
    }

    /// The `initialize`/`initialized`/`session/new` handshake with no
    /// auth methods, returning the session request for assertions.
    pub async fn handshake(&mut self, session_id: &str) -> Value {
        let session = self
            .handshake_with(session_id, json!({"loadSession": true}))
            .await;
        assert_eq!(
            session["method"].as_str(),
            Some("session/new"),
            "the session request arrives after initialized"
        );
        session
    }

    /// The handshake with explicit agent capabilities, so tests can
    /// gate `loadSession`, `promptCapabilities`, and `authMethods`.
    pub async fn handshake_with(&mut self, session_id: &str, capabilities: Value) -> Value {
        self.handshake_result(capabilities, json!({"sessionId": session_id}))
            .await
    }

    /// The handshake where the session request's response is supplied
    /// in full, so tests can attach `configOptions` or observe
    /// `session/load`. The session request itself is returned for the
    /// caller's method and param assertions.
    pub async fn handshake_result(&mut self, capabilities: Value, result: Value) -> Value {
        self.initialize(json!({
            "protocolVersion": 1,
            "agentInfo": {"name": "fake-agent", "version": "0.0-test"},
            "agentCapabilities": capabilities,
            "authMethods": [],
        }))
        .await;
        let _ready = self.next_method("initialized").await;
        let session = self.next_client().await;
        self.respond(&session, result).await;
        session
    }

    /// A `session/update` notification for the active prompt.
    pub async fn notify_update(&mut self, update: Value) {
        self.write(json!({
            "method": "session/update",
            "params": {"sessionId": "fake-session", "update": update},
        }))
        .await;
    }

    /// Keep the connection open, draining client frames until the
    /// adapter side disconnects.
    pub async fn park(&mut self) {
        while self.next_client_opt().await.is_some() {}
    }
}

/// An adapter whose sessions speak over one duplex pair apiece, plus
/// the channel that hands each agent end to the test.
pub(super) fn duplex_adapter() -> (AcpAdapter, Receiver<DuplexStream>) {
    let (agents_tx, agents_rx) = mpsc::channel::<DuplexStream>();
    let connector = Arc::new(move |setup: &SessionSetup| {
        assert_eq!(setup.program, "cursor-agent");
        assert_eq!(setup.args, vec!["acp".to_owned()]);
        let (client, agent) = tokio::io::duplex(64 * 1024);
        let _ = agents_tx.send(agent);
        let (read, write) = tokio::io::split(client);
        Ok(Transport {
            stdin: Box::new(write),
            stdout: BufReader::new(Box::new(read) as Box<dyn AsyncRead + Unpin + Send>),
            stderr: None,
            child: None,
        })
    });
    let adapter = AcpAdapter::new()
        .with_connector(connector)
        .with_agent_info(present_agent_info());
    (adapter, agents_rx)
}

/// The probed evidence an installed agent reports: presence proven,
/// authentication unprobed (ACP authenticates inside the session).
fn present_agent_info() -> AgentIntegrationInfo {
    AgentIntegrationInfo::from_inspection(
        ProviderId::new("cursor"),
        IntegrationId::new("cursor"),
        AgentPresenceEvidence {
            state: AgentPresenceState::Present,
            reason: "test".to_owned(),
        },
        AgentVersionEvidence {
            version: Some("0.0-test".to_owned()),
            reason: "test".to_owned(),
        },
        AgentAuthenticationEvidence {
            state: AgentAuthenticationState::Unknown,
            reason: "test".to_owned(),
        },
        BTreeMap::new(),
    )
}

/// Play `script` on the next duplex agent end the adapter opens. The
/// waiter thread blocks on the channel until `start` spawns its driver;
/// the returned handle finishes when the script completes.
pub(super) fn serve<R, Fut>(
    agents: Receiver<DuplexStream>,
    script: impl FnOnce(FakeAgent) -> Fut + Send + 'static,
) -> JoinHandle<R>
where
    R: Send + 'static,
    Fut: Future<Output = R> + Send + 'static,
{
    let agents = Mutex::new(agents);
    std::thread::spawn(move || {
        let agent = agents
            .lock()
            .expect("agents mutex")
            .recv()
            .expect("the adapter requested a transport");
        let (read, write) = tokio::io::split(agent);
        let agent = FakeAgent {
            reader: BufReader::new(read),
            writer: write,
        };
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("agent runtime")
            .block_on(script(agent))
    })
}

/// The integration this adapter serves.
pub(super) fn integration() -> Integration {
    Integration {
        id: IntegrationId::new("cursor"),
        provider: ProviderId::new("cursor"),
        name: "Cursor".to_owned(),
        execution: ExecutionConfig::Cli {
            adapter: CliAdapterId::new("cursor"),
        },
        monitoring: None,
    }
}

/// Session start options against a canonicalizable working directory.
pub(super) fn options(access: AccessMode) -> StartOptions {
    options_at(PathBuf::from("/tmp"), access)
}

/// Session start options rooted at `cwd`, for workspace-boundary tests.
pub(super) fn options_at(cwd: PathBuf, access: AccessMode) -> StartOptions {
    StartOptions {
        cwd,
        selection: ModelSelection {
            integration_id: IntegrationId::new("cursor"),
            model: String::new(),
            effort: None,
        },
        access,
        resume_cursor: None,
        external_tools: Vec::new(),
        env: Default::default(),
        optimize: Default::default(),
    }
}

/// A unique existing workspace directory under the system temp dir.
pub(super) fn temp_workspace(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("aifuel-acp-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&path).expect("create the test workspace");
    path
}

pub(super) fn input(text: &str) -> UserInput {
    UserInput {
        text: text.to_owned(),
        attachments: Vec::new(),
    }
}

/// Bounded read so a missing event fails the test instead of hanging it.
pub(super) fn recv(events: &Receiver<AgentEventKind>) -> AgentEventKind {
    events
        .recv_timeout(Duration::from_secs(10))
        .expect("the next event arrives")
}

/// The fresh-session prelude every `start` emits.
pub(super) fn expect_prelude(events: &Receiver<AgentEventKind>) {
    assert!(matches!(
        recv(events),
        AgentEventKind::SessionCreated { .. }
    ));
    assert!(matches!(
        recv(events),
        AgentEventKind::SessionStatus {
            status: SessionStatus::Idle
        }
    ));
}

/// Read until `run.completed` (inclusive) and return every event seen.
pub(super) fn through_run_completed(events: &Receiver<AgentEventKind>) -> Vec<AgentEventKind> {
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

/// Read until `session.closed` (inclusive) and return every event seen.
pub(super) fn through_closed(events: &Receiver<AgentEventKind>) -> Vec<AgentEventKind> {
    let mut kinds = Vec::new();
    for _ in 0..32 {
        let kind = recv(events);
        let terminal = matches!(kind, AgentEventKind::SessionClosed { .. });
        kinds.push(kind);
        if terminal {
            return kinds;
        }
    }
    panic!("session.closed never arrived: {kinds:?}")
}

/// Read until the session returns to `idle` after a completed run.
pub(super) fn through_idle(events: &Receiver<AgentEventKind>) -> Vec<AgentEventKind> {
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

pub(super) fn test_events(
    adapter: &AcpAdapter,
    handle: &AgentSessionHandle,
) -> Receiver<AgentEventKind> {
    adapter
        .test_events(handle)
        .expect("the session's event receiver is attached")
}
