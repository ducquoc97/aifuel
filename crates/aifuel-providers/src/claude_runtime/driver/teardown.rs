//! Terminal facts on every exit path: a run that was still in flight
//! gets its `run.completed` fact, and `session.closed` lands last.

use super::{RunFlow, close_streams, stderr_hint};
use crate::claude_runtime::session::ClaudeSession;
use aifuel_core::{AgentEventKind, ReceiptCode, RunOutcome, SessionStatus};
use std::sync::Mutex;

/// The provider stdout ended. Whether the teardown is a requested
/// close or a lost provider is read from `state.closed`: adapter
/// teardown paths set it before killing the process.
pub(super) fn eof_teardown(
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
pub(super) fn teardown_run(session: &ClaudeSession, flow: &mut Option<RunFlow>) {
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
