//! Tests for the Codex App Server adapter, run against a scripted
//! app-server double on a duplex transport so no real provider process
//! is involved.

mod approvals;
mod catalog;
mod runs;

use super::session::{SessionSetup, Transport};
use super::*;
use aifuel_core::{
    AccessMode, AgentAuthenticationEvidence, AgentAuthenticationState, AgentEventKind,
    AgentIntegrationInfo, AgentPresenceEvidence, AgentPresenceState, AgentSessionHandle,
    AgentVersionEvidence, CliAdapterId, ExecutionConfig, Integration, SessionStatus, StartOptions,
    UserInput,
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

/// The app-server end of the duplex transport, scripted by tests.
pub(super) struct FakeServer {
    reader: BufReader<tokio::io::ReadHalf<DuplexStream>>,
    writer: tokio::io::WriteHalf<DuplexStream>,
}

impl FakeServer {
    /// The next complete JSONL message the adapter wrote, or `None` at
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
            .expect("write the server frame");
        self.writer.flush().await.expect("flush the server frame");
    }

    /// Answer the client request `request` with `result`.
    pub async fn respond(&mut self, request: &Value, result: Value) {
        self.write(json!({"id": request["id"], "result": result}))
            .await;
    }

    /// The `initialize`/`initialized`/thread handshake, returning the
    /// `thread/start` or `thread/resume` request for assertions.
    pub async fn handshake(&mut self, thread_id: &str) -> Value {
        let init = self.next_method("initialize").await;
        self.respond(&init, json!({})).await;
        let ready = self.next_method("initialized").await;
        let start = self.next_client().await;
        assert!(
            matches!(
                start["method"].as_str(),
                Some("thread/start") | Some("thread/resume")
            ),
            "the thread request arrives after initialized: {ready:?}"
        );
        self.respond(&start, json!({"thread": {"id": thread_id}}))
            .await;
        start
    }

    /// Keep the connection open, draining client frames until the
    /// adapter side disconnects.
    pub async fn park(&mut self) {
        while self.next_client_opt().await.is_some() {}
    }
}

/// An adapter whose sessions speak over one duplex pair apiece, plus
/// the channel that hands each server end to the test.
pub(super) fn duplex_adapter() -> (CodexAdapter, Receiver<DuplexStream>) {
    duplex_adapter_with_catalog(Vec::new())
}

/// [`duplex_adapter`] with a pre-seeded Advertised Model catalog.
pub(super) fn duplex_adapter_with_catalog(
    models: Vec<ProviderCatalogModel>,
) -> (CodexAdapter, Receiver<DuplexStream>) {
    let (servers_tx, servers_rx) = mpsc::channel::<DuplexStream>();
    let connector = Arc::new(move |_setup: &SessionSetup| {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let _ = servers_tx.send(server);
        let (read, write) = tokio::io::split(client);
        Ok(Transport {
            stdin: Box::new(write),
            stdout: BufReader::new(Box::new(read) as Box<dyn AsyncRead + Unpin + Send>),
            stderr: None,
            child: None,
        })
    });
    let adapter = CodexAdapter::new()
        .with_connector(connector)
        .with_agent_info(present_agent_info())
        .with_catalog(models);
    (adapter, servers_rx)
}

/// The probed evidence an installed-and-authenticating-could-be provider
/// reports: presence proven, authentication unprobed.
fn present_agent_info() -> AgentIntegrationInfo {
    AgentIntegrationInfo::from_inspection(
        ProviderId::from(ProviderKey::Codex),
        IntegrationId::from(ProviderKey::Codex),
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

/// Play `script` on the next duplex server end the adapter opens. The
/// waiter thread blocks on the channel until `start` spawns its driver;
/// the returned handle finishes when the script completes.
pub(super) fn serve<R, Fut>(
    servers: Receiver<DuplexStream>,
    script: impl FnOnce(FakeServer) -> Fut + Send + 'static,
) -> JoinHandle<R>
where
    R: Send + 'static,
    Fut: Future<Output = R> + Send + 'static,
{
    let servers = Mutex::new(servers);
    std::thread::spawn(move || {
        let server = servers
            .lock()
            .expect("servers mutex")
            .recv()
            .expect("the adapter requested a transport");
        let (read, write) = tokio::io::split(server);
        let server = FakeServer {
            reader: BufReader::new(read),
            writer: write,
        };
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("server runtime")
            .block_on(script(server))
    })
}

/// The integration this adapter serves.
pub(super) fn integration() -> Integration {
    Integration {
        id: IntegrationId::from(ProviderKey::Codex),
        provider: ProviderId::from(ProviderKey::Codex),
        name: "Codex".to_owned(),
        execution: ExecutionConfig::Cli {
            adapter: CliAdapterId::new("codex"),
        },
        monitoring: None,
    }
}

/// Session start options against a canonicalizable working directory.
pub(super) fn options(access: AccessMode) -> StartOptions {
    StartOptions {
        cwd: PathBuf::from("/tmp"),
        selection: ModelSelection {
            integration_id: IntegrationId::from(ProviderKey::Codex),
            model: "codex-test-model".to_owned(),
            effort: None,
        },
        access,
        resume_cursor: None,
    }
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
    adapter: &CodexAdapter,
    handle: &AgentSessionHandle,
) -> Receiver<AgentEventKind> {
    adapter
        .test_events(handle)
        .expect("the session's event receiver is attached")
}
