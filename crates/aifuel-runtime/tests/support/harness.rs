//! Shared command constructors plus channel and receipt helpers for the
//! facade's integration tests.

use aifuel_core::{
    AccessMode, AgentCommand, AgentEvent, AgentEventKind, CommandId, IntegrationId, ModelSelection,
    Receipt, ReceiptCode, ReceiptOutcome, SessionId, SessionSnapshot, SessionStatus, UserInput,
};
use aifuel_runtime::CommandOutcome;
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use super::FAKE_INTEGRATION;

/// One unique command id per call.
pub fn next_id() -> CommandId {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    CommandId::new(format!("cmd-{}", NEXT.fetch_add(1, Ordering::Relaxed)))
}

/// A selection naming the fake integration's model.
pub fn selection(model: &str) -> ModelSelection {
    ModelSelection {
        integration_id: IntegrationId::new(FAKE_INTEGRATION),
        model: model.to_owned(),
        effort: None,
    }
}

/// `session.create` against the fake integration.
pub fn create(dir: &Path, model: &str) -> AgentCommand {
    AgentCommand::SessionCreate {
        command_id: next_id(),
        cwd: dir.to_path_buf(),
        selection: selection(model),
        access: AccessMode::WorkspaceWrite,
    }
}

/// `session.subscribe` from `last_seen_seq`.
pub fn subscribe(session_id: &SessionId, last_seen_seq: u64) -> AgentCommand {
    AgentCommand::SessionSubscribe {
        command_id: next_id(),
        session_id: session_id.clone(),
        last_seen_seq,
    }
}

/// `run.start` with a plain-text input.
pub fn run_start(session_id: &SessionId, text: &str) -> AgentCommand {
    AgentCommand::RunStart {
        command_id: next_id(),
        session_id: session_id.clone(),
        input: UserInput {
            text: text.to_owned(),
            attachments: Vec::new(),
        },
    }
}

/// The session id of a successful `session.create`, failing the test
/// loudly on a wrong receipt.
pub fn created_session(outcome: &CommandOutcome) -> SessionId {
    match &outcome.receipt.outcome {
        ReceiptOutcome::Ok {
            session_id: Some(session_id),
            ..
        } => session_id.clone(),
        other => panic!("session.create should succeed: {other:?}"),
    }
}

/// The head seq an ok receipt carries.
pub fn receipt_seq(receipt: &Receipt) -> u64 {
    match &receipt.outcome {
        ReceiptOutcome::Ok { seq, .. } => *seq,
        other => panic!("expected an ok receipt: {other:?}"),
    }
}

/// The snapshot an ok receipt carries.
pub fn receipt_snapshot(receipt: &Receipt) -> &SessionSnapshot {
    match &receipt.outcome {
        ReceiptOutcome::Ok {
            snapshot: Some(snapshot),
            ..
        } => snapshot,
        other => panic!("expected a snapshot receipt: {other:?}"),
    }
}

/// The error code an error receipt carries.
pub fn receipt_code(outcome: &CommandOutcome) -> ReceiptCode {
    match &outcome.receipt.outcome {
        ReceiptOutcome::Err { code, .. } => *code,
        other => panic!("expected an error receipt: {other:?}"),
    }
}

/// Collect channel events until `stop` matches one, with a bound so a
/// missing event fails instead of hanging.
pub fn collect_until(
    events: &Receiver<AgentEvent>,
    stop: impl Fn(&AgentEvent) -> bool,
) -> Vec<AgentEvent> {
    let mut collected = Vec::new();
    for _ in 0..600 {
        let event = events
            .recv_timeout(Duration::from_secs(15))
            .expect("the next event arrives");
        let done = stop(&event);
        collected.push(event);
        if done {
            return collected;
        }
    }
    panic!("the expected event never arrived");
}

/// Whether the event is a `run.completed` fact.
pub fn is_completed(event: &AgentEvent) -> bool {
    matches!(event.kind, AgentEventKind::RunCompleted { .. })
}

/// Collect until a run has completed and the session is idle again. The
/// completed gate matters: the session's own startup `idle` arrives or
/// replays first.
pub fn collect_run(events: &Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut collected = Vec::new();
    for _ in 0..600 {
        let event = events
            .recv_timeout(Duration::from_secs(15))
            .expect("the next event arrives");
        let done = collected.iter().any(is_completed)
            && matches!(
                event.kind,
                AgentEventKind::SessionStatus {
                    status: SessionStatus::Idle
                }
            );
        collected.push(event);
        if done {
            return collected;
        }
    }
    panic!("the run never settled to idle");
}
