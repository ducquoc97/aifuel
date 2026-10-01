//! Steady-state message dispatch: protocol responses complete pending
//! client requests, server-initiated requests become Approval Requests
//! or protocol rejections, and notifications flow through the run's
//! event mapping.

use super::{PendingRpc, reject_server_request};
use crate::codex::app_server::protocol_send as send;
use crate::codex::interaction;
use crate::codex_runtime::mapping::TurnEvents;
use crate::codex_runtime::session::{CodexSession, PendingApproval, SessionSetup};
use crate::codex_runtime::{APPROVAL_POLICY, interactions, sandbox_policy};
use aifuel_core::{
    AgentEventKind, AgentRuntimeError, ModelSelection, ReceiptCode, RunId, SessionStatus,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tokio::io::AsyncWrite;

/// Route one decoded app-server message: a response (has `id`, no
/// `method`), a server-initiated request (`id` plus `method`), or a
/// notification (`method` only).
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_message(
    session: &Arc<CodexSession>,
    stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
    message: &Value,
    next_rpc: &mut u64,
    pending_rpc: &mut BTreeMap<u64, PendingRpc>,
    turn: &mut Option<TurnEvents>,
    orphans: &mut BTreeSet<String>,
    thread_id: &str,
) {
    match (message.get("id").is_some(), message.get("method")) {
        (true, None) => {
            on_response(
                session,
                stdin,
                message,
                next_rpc,
                pending_rpc,
                turn,
                orphans,
                thread_id,
            )
            .await;
        }
        (true, Some(_)) => on_server_request(session, stdin, message, turn).await,
        (false, Some(method)) => {
            if let Some(method) = method.as_str() {
                on_notification(session, message, method, turn, orphans);
            }
        }
        (false, None) => {}
    }
}

/// Complete one pending client request, or ignore an unsolicited id.
#[allow(clippy::too_many_arguments)]
async fn on_response(
    session: &Arc<CodexSession>,
    stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
    message: &Value,
    next_rpc: &mut u64,
    pending_rpc: &mut BTreeMap<u64, PendingRpc>,
    turn: &mut Option<TurnEvents>,
    orphans: &mut BTreeSet<String>,
    thread_id: &str,
) {
    let Some(id) = message.get("id").and_then(Value::as_u64) else {
        return;
    };
    let Some(wait) = pending_rpc.remove(&id) else {
        return;
    };
    match wait {
        PendingRpc::TurnStart { reply, selection } => {
            let error = message.get("error").filter(|error| !error.is_null());
            let turn_id = message["result"]["turn"]["id"].as_str();
            if error.is_some() || turn_id.is_none() {
                session.clear_turn_starting();
                let reason = error
                    .and_then(|error| error["message"].as_str().map(str::to_owned))
                    .unwrap_or_else(|| "the app-server did not return a turn id".to_owned());
                let _ = reply.send(Err(AgentRuntimeError::provider_error(reason)));
                return;
            }
            let turn_id = turn_id.expect("checked above").to_owned();
            let run_id = RunId::new(turn_id.clone());
            // Register the run before releasing `send`: a caller that
            // proceeds to `cancel` must never observe the run missing.
            {
                let mut state = session.state.lock().expect("session state mutex");
                state.turn_starting = false;
                state.active_run = Some(run_id.clone());
            }
            if reply.send(Ok(run_id.clone())).is_err() {
                // `send` gave up waiting: never announce the run, but
                // the provider turn is live, so interrupt it and drop
                // its notifications.
                {
                    let mut state = session.state.lock().expect("session state mutex");
                    state.active_run = None;
                }
                orphans.insert(turn_id.clone());
                let id = *next_rpc;
                *next_rpc += 1;
                let interrupt = json!({
                    "id": id,
                    "method": "turn/interrupt",
                    "params": {"threadId": thread_id, "turnId": turn_id},
                });
                let _ = send(stdin, interrupt, None).await;
                return;
            }
            *turn = Some(TurnEvents::new(run_id.clone()));
            session.emit(AgentEventKind::RunStarted { run_id, selection });
            session.emit(AgentEventKind::SessionStatus {
                status: SessionStatus::Working,
            });
        }
        PendingRpc::Interrupt { run_id } => {
            if let Some(error) = message.get("error").filter(|error| !error.is_null()) {
                session.emit(AgentEventKind::Error {
                    run_id: Some(run_id),
                    code: ReceiptCode::ProviderError,
                    message: format!(
                        "the app-server rejected the interrupt request: {}",
                        error["message"].as_str().unwrap_or("unknown error")
                    ),
                    retryable: false,
                });
            }
        }
    }
}

/// Convert one server-initiated request into an `approval.requested`
/// event and park it until `answer` replies, or reject it at the
/// protocol layer when the method is not one the adapter supports.
async fn on_server_request(
    session: &Arc<CodexSession>,
    stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
    message: &Value,
    turn: &mut Option<TurnEvents>,
) {
    let Some(pending) = interaction::parse_pending(message) else {
        reject_server_request(stdin, message, None).await;
        return;
    };
    let Some(run) = turn.as_ref() else {
        // Server-initiated requests only make sense inside a turn.
        reject_server_request(stdin, message, None).await;
        return;
    };
    let run_id = run.run_id().clone();
    let request = interaction::application_request(&pending);
    let request_id = session.next_request_id();
    let payload = interactions::approval_request(&request, session.access);
    {
        let mut state = session.state.lock().expect("session state mutex");
        state.pending.insert(
            request_id.clone(),
            PendingApproval {
                interaction: pending,
                options: payload
                    .options
                    .iter()
                    .map(|option| option.id.clone())
                    .collect(),
                question_ids: request.question_ids(),
            },
        );
    }
    session.emit(AgentEventKind::SessionStatus {
        status: SessionStatus::WaitingApproval,
    });
    session.emit(AgentEventKind::ApprovalRequested {
        run_id,
        request_id,
        request: payload,
    });
}

/// Map one server notification through the run's event mapping.
/// Notifications naming an orphaned or finished turn are dropped; a
/// run-less `error` notification still surfaces without a run id.
fn on_notification(
    session: &Arc<CodexSession>,
    message: &Value,
    method: &str,
    turn: &mut Option<TurnEvents>,
    orphans: &mut BTreeSet<String>,
) {
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let turn_id = params
        .get("turnId")
        .or_else(|| params["turn"].get("id"))
        .and_then(Value::as_str);
    if let Some(turn_id) = turn_id
        && orphans.remove(turn_id)
    {
        return;
    }
    if method == "turn/completed" {
        let Some(active) = turn.as_ref() else {
            return;
        };
        if turn_id != Some(active.run_id().as_str()) {
            return;
        }
        let active = turn.take().expect("checked above");
        let events_run_id = active.run_id().clone();
        let events = active.finish_events(&params["turn"]);
        {
            let mut state = session.state.lock().expect("session state mutex");
            if state.active_run.as_ref() == Some(&events_run_id) {
                state.active_run = None;
            }
            // A turn ending retracts its server requests; parked
            // approvals cannot be answered anymore.
            state.pending.clear();
        }
        let open = !session.state.lock().expect("session state mutex").closed;
        for event in events {
            session.emit(event);
        }
        if open {
            session.emit(AgentEventKind::SessionStatus {
                status: SessionStatus::Idle,
            });
        }
        return;
    }
    let Some(active) = turn.as_mut() else {
        // Outside a run only thread-level errors are worth surfacing.
        if method == "error" {
            session.emit(AgentEventKind::Error {
                run_id: None,
                code: ReceiptCode::ProviderError,
                message: params["error"]["message"]
                    .as_str()
                    .unwrap_or("the app-server reported an error")
                    .to_owned(),
                retryable: params["willRetry"].as_bool().unwrap_or(false),
            });
        }
        return;
    };
    // Run-scoped notifications belong to the active turn only; late
    // notifications for an ended turn are protocol noise.
    if let Some(turn_id) = turn_id
        && turn_id != active.run_id().as_str()
    {
        return;
    }
    for event in active.map(method, &params) {
        session.emit(event);
    }
}

/// The `turn/start` request for one user message. Model and effort come
/// from the session's current selection, which `model.select` can update
/// between runs.
pub(super) fn turn_start_message(
    id: u64,
    thread_id: &str,
    setup: &SessionSetup,
    selection: &ModelSelection,
    items: Vec<Value>,
) -> Value {
    let model = (!selection.model.is_empty()).then(|| selection.model.clone());
    json!({
        "id": id,
        "method": "turn/start",
        "params": {
            "threadId": thread_id,
            "input": items,
            "cwd": setup.cwd,
            "model": model,
            "effort": selection.effort.map(|effort| effort.as_str()),
            "approvalPolicy": APPROVAL_POLICY,
            "sandboxPolicy": sandbox_policy(setup.access),
        }
    })
}
