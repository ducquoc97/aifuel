//! Fake-serve tests: an in-process HTTP server scripts the OpenCode
//! API surface so no real `opencode` binary or credentials are needed.

mod approvals;
mod runs;
mod server;
mod session;

use super::serve::{self, ServeHandle};
use super::*;
use aifuel_core::{
    AccessMode, AgentAdapter, AgentAuthenticationEvidence, AgentAuthenticationState,
    AgentEventKind, AgentIntegrationInfo, AgentPresenceEvidence, AgentPresenceState,
    AgentSessionHandle, AgentVersionEvidence, CliAdapterId, ExecutionConfig, Integration,
    SessionStatus, StartOptions, UserInput,
};
use serde_json::{Value, json};
use server::FakeServe;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

/// An adapter whose sessions talk to the fake's HTTP endpoint plus the
/// probed evidence an installed provider reports.
pub(super) fn adapter_with(fake: &FakeServe) -> OpenCodeAdapter {
    let url = fake.url();
    let connector: serve::Connector = Arc::new(move |_setup| {
        Ok(ServeHandle {
            base_url: url.clone(),
            auth: None,
            child: None,
            stderr: None,
        })
    });
    OpenCodeAdapter::new()
        .with_connector(connector)
        .with_agent_info(present_agent_info())
        .with_catalog(super::catalog::CatalogResult {
            models: Vec::new(),
            connected: Default::default(),
        })
}

fn present_agent_info() -> AgentIntegrationInfo {
    AgentIntegrationInfo::from_inspection(
        ProviderId::new(PROVIDER_ID),
        IntegrationId::new(PROVIDER_ID),
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

/// The integration this adapter serves.
pub(super) fn integration() -> Integration {
    Integration {
        id: IntegrationId::new(PROVIDER_ID),
        provider: ProviderId::new(PROVIDER_ID),
        name: "OpenCode".to_owned(),
        execution: ExecutionConfig::Cli {
            adapter: CliAdapterId::new(PROVIDER_ID),
        },
        monitoring: None,
    }
}

/// Session start options against a canonicalizable working directory.
pub(super) fn options(access: AccessMode) -> StartOptions {
    StartOptions {
        cwd: PathBuf::from("/tmp"),
        selection: ModelSelection {
            integration_id: IntegrationId::new(PROVIDER_ID),
            model: "test-provider/test-model".to_owned(),
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

/// The `/event` greeting the driver waits on during setup.
pub(super) fn connected() -> Value {
    json!({"type": "server.connected", "properties": {}})
}

/// Start a session through the scripted setup and return its handle
/// plus the raw event receiver.
pub(super) fn start_session(
    adapter: &OpenCodeAdapter,
    fake: &FakeServe,
    options: StartOptions,
) -> (AgentSessionHandle, Receiver<AgentEventKind>) {
    fake.push_event(connected());
    let handle = adapter
        .start(&integration(), options)
        .expect("the session starts");
    let events = test_events(adapter, &handle);
    (handle, events)
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

/// Read until one event satisfies `until` (inclusive) and return every
/// event seen. Ordering-sensitive tests use this to confirm a bus event
/// landed before they release the held prompt response.
pub(super) fn through_match(
    events: &Receiver<AgentEventKind>,
    until: impl Fn(&AgentEventKind) -> bool,
) -> Vec<AgentEventKind> {
    let mut kinds = Vec::new();
    for _ in 0..64 {
        let kind = recv(events);
        let done = until(&kind);
        kinds.push(kind);
        if done {
            return kinds;
        }
    }
    panic!("the awaited event never arrived: {kinds:?}")
}

/// Read until `run.completed` (inclusive) and return every event seen.
pub(super) fn through_run_completed(events: &Receiver<AgentEventKind>) -> Vec<AgentEventKind> {
    let mut kinds = Vec::new();
    for _ in 0..64 {
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

/// Read until `session.closed` (inclusive) and return every event seen.
pub(super) fn through_closed(events: &Receiver<AgentEventKind>) -> Vec<AgentEventKind> {
    let mut kinds = Vec::new();
    for _ in 0..64 {
        let kind = recv(events);
        let terminal = matches!(kind, AgentEventKind::SessionClosed { .. });
        kinds.push(kind);
        if terminal {
            return kinds;
        }
    }
    panic!("session.closed never arrived: {kinds:?}")
}

pub(super) fn test_events(
    adapter: &OpenCodeAdapter,
    handle: &AgentSessionHandle,
) -> Receiver<AgentEventKind> {
    adapter
        .test_events(handle)
        .expect("the session's event receiver is attached")
}

/// The session id the fake's `POST /session` and `GET /session/{id}`
/// handlers report.
pub(super) const FAKE_SESSION: &str = "ses_fake";

/// Whether the request log contains one request whose path contains
/// `needle` (the `?directory` query is stripped by the fake already).
pub(super) fn requested(fake: &FakeServe, needle: &str) -> bool {
    fake.requests()
        .iter()
        .any(|request| request.path.contains(needle))
}

/// Wait for one request whose path contains `needle` to reach the
/// fake. `send` returns when the driver acks the run, but the prompt
/// POST leaves on a spawned task, so the wire record can trail the ack
/// by a few scheduler ticks.
pub(super) fn wait_for_request(fake: &FakeServe, needle: &str) -> server::RecordedRequest {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(request) = fake
            .requests()
            .into_iter()
            .find(|request| request.path.contains(needle))
        {
            return request;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no request containing {needle} reached the fake"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// One `message.part.updated` event for `part`.
pub(super) fn part_updated(part: Value) -> Value {
    json!({"type": "message.part.updated", "properties": {"part": part}})
}

/// A text or reasoning part object as the serve reports it.
pub(super) fn text_part(
    part_id: &str,
    session: &str,
    message: &str,
    text: &str,
    ended: bool,
) -> Value {
    let mut part = json!({
        "id": part_id,
        "sessionID": session,
        "messageID": message,
        "type": "text",
        "text": text,
        "time": {"start": 1},
    });
    if ended {
        part["time"]["end"] = json!(2);
    }
    part
}

/// A tool part in one state.
pub(super) fn tool_part(
    part_id: &str,
    session: &str,
    message: &str,
    call_id: &str,
    tool: &str,
    state: Value,
) -> Value {
    json!({
        "id": part_id,
        "sessionID": session,
        "messageID": message,
        "type": "tool",
        "callID": call_id,
        "tool": tool,
        "state": state,
    })
}
