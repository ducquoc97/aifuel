//! Terminal facts and process teardown on every exit path: a run that
//! was still in flight gets its `run.completed` fact, the serve process
//! is reaped, and `session.closed` lands last.

use super::{Exit, SHUTDOWN_TIMEOUT};
use crate::agent_execution::kill_and_wait;
use crate::opencode_runtime::session::OpenCodeSession;
use aifuel_core::{AgentEventKind, ReceiptCode, RunOutcome, SessionStatus};
use std::io;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// Emit the run and session terminal facts the exit path implies, then
/// reap the serve process and its stderr capture.
pub(super) async fn teardown(
    session: &Arc<OpenCodeSession>,
    stderr_task: Option<JoinHandle<io::Result<String>>>,
    exit: Exit,
) {
    let (reason, run_outcome, run_error) = match &exit {
        Exit::Requested => (None, Some(RunOutcome::Cancelled), None),
        Exit::Dropped => (
            Some("the session was dropped".to_owned()),
            Some(RunOutcome::Cancelled),
            None,
        ),
        Exit::ServerClosed => (
            Some("the opencode event stream closed".to_owned()),
            Some(RunOutcome::Failed),
            Some("the opencode event stream closed".to_owned()),
        ),
        Exit::ServerFailed(reason) => (
            Some(reason.clone()),
            Some(RunOutcome::Failed),
            Some(reason.clone()),
        ),
        // Nothing was announced and no run existed; the caller got the
        // handshake error through the setup report.
        Exit::SetupFailed => (None, None, None),
    };
    {
        let mut state = session.state.lock().expect("session state mutex");
        state.pending.clear();
    }
    // Reap the process before emitting `session.closed`, matching the
    // CLI session's worker-join ordering.
    let child = session.child.lock().expect("child mutex").take();
    if let Some(mut child) = child {
        let _ = kill_and_wait(&mut child).await;
    }
    let diagnostics = match stderr_task {
        Some(task) => timeout(SHUTDOWN_TIMEOUT, task)
            .await
            .ok()
            .and_then(Result::ok)
            .and_then(Result::ok)
            .unwrap_or_default(),
        None => String::new(),
    };
    let run_id = session
        .state
        .lock()
        .expect("session state mutex")
        .active_run
        .clone();
    if let (Some(run_id), Some(outcome)) = (run_id, run_outcome) {
        if let Some(message) = run_error {
            let message = append_diagnostics(message, &diagnostics);
            session.emit(AgentEventKind::Error {
                run_id: Some(run_id.clone()),
                code: ReceiptCode::ProviderError,
                message,
                retryable: false,
            });
        }
        session.emit(AgentEventKind::RunCompleted {
            run_id,
            outcome,
            usage: None,
        });
    }
    session.mark_closed();
    // An unexpected disconnect records `interrupted` before `closed`:
    // the run stopped through a lost provider connection, not an answer.
    if matches!(
        exit,
        Exit::Dropped | Exit::ServerClosed | Exit::ServerFailed(_)
    ) {
        session.emit(AgentEventKind::SessionStatus {
            status: SessionStatus::Interrupted,
        });
    }
    let reason = reason.map(|reason| append_diagnostics(reason, &diagnostics));
    session.finish_close(reason);
}

/// Append serve stderr to a reason when the capture is non-empty,
/// bounded so a huge diagnostic cannot inflate the event.
fn append_diagnostics(mut reason: String, diagnostics: &str) -> String {
    let diagnostics = diagnostics.trim();
    if diagnostics.is_empty() {
        return reason;
    }
    const LIMIT: usize = 2048;
    let tail: String = diagnostics.chars().take(LIMIT).collect();
    reason.push_str(": ");
    reason.push_str(&tail);
    reason
}
