//! OpenCode bus events mapped onto contract emissions.
//!
//! The driver routes one `{type, properties}` event here; every
//! emission stays inside the run's causal order. Events for any other
//! session, or arriving after the run closed, are dropped rather than
//! guessed.

use super::PromptOutcome;
use super::dispatch::{
    RunFlow, SessionFlow, ToolTrack, emit_error, emit_error_retryable, emit_status,
};
use crate::opencode_runtime::protocol::{self, ABORTED_ERROR_NAME};
use crate::opencode_runtime::session::{OpenCodeSession, PendingPermission};
use aifuel_core::{
    AgentEventKind, ApprovalDecision, MessageStream, ReceiptCode, RequestId, RunOutcome,
    SessionStatus,
};
use serde_json::Value;
use std::sync::Arc;

/// One `{type, properties}` bus event. Everything is scoped to this
/// session's provider session id; other instances' traffic on the
/// shared bus is dropped.
pub(super) fn on_event(
    session: &Arc<OpenCodeSession>,
    flow: &mut SessionFlow,
    provider_session: &str,
    event: &Value,
) {
    let properties = &event["properties"];
    let owned = properties["sessionID"].as_str() == Some(provider_session)
        || properties["part"]["sessionID"].as_str() == Some(provider_session);
    if !owned {
        return;
    }
    match event["type"].as_str() {
        Some(protocol::EV_PART_UPDATED) => part_updated(session, flow, properties),
        Some(protocol::EV_PERMISSION_UPDATED) => permission_updated(session, flow, properties),
        Some(protocol::EV_PERMISSION_REPLIED) => permission_replied(session, flow, properties),
        Some(protocol::EV_SESSION_IDLE) => {
            if flow.run.is_none() {
                emit_status(session, flow, SessionStatus::Idle);
            }
        }
        Some(protocol::EV_SESSION_STATUS) => session_status(session, flow, properties),
        Some(protocol::EV_SESSION_ERROR) => session_error(session, flow, properties),
        Some(protocol::EV_TODO_UPDATED) => todo_updated(session, flow, properties),
        _ => {}
    }
}

/// `message.part.updated`: text and reasoning parts stream deltas,
/// tool parts drive tool events, `step-finish` reports token
/// accounting. The full part object is authoritative, so the emitted
/// increment is the part's new suffix.
fn part_updated(session: &Arc<OpenCodeSession>, flow: &mut SessionFlow, properties: &Value) {
    let Some(run) = flow.run.as_mut() else {
        return;
    };
    let part = &properties["part"];
    let run_id = run.run_id.clone();
    // Parts on the user message echo host input, not model output.
    if part["messageID"].as_str() == Some(run_id.as_str()) {
        return;
    }
    let delta = properties["delta"].as_str();
    handle_part(session, run, part, delta);
}

/// One part object, from the event stream or the prompt response's
/// completeness flush.
fn handle_part(
    session: &Arc<OpenCodeSession>,
    run: &mut RunFlow,
    part: &Value,
    delta: Option<&str>,
) {
    let run_id = run.run_id.clone();
    match part["type"].as_str() {
        Some("text") | Some("reasoning") => {
            // Synthetic text parts are provider-injected context, not
            // model output.
            if part["synthetic"].as_bool() == Some(true) {
                return;
            }
            let stream = if part["type"] == "reasoning" {
                MessageStream::Thinking
            } else {
                MessageStream::Assistant
            };
            let Some(part_id) = part["id"].as_str().map(str::to_owned) else {
                return;
            };
            let text = part["text"].as_str().unwrap_or("");
            let known = run.part_text.get(&part_id).cloned().unwrap_or_default();
            let increment = match delta {
                Some(delta) => delta,
                None => text.strip_prefix(&known).unwrap_or(""),
            };
            if !increment.is_empty() {
                run.open_streams.entry(part_id.clone()).or_insert(stream);
                run.part_text.insert(part_id.clone(), text.to_owned());
                session.emit(AgentEventKind::MessageDelta {
                    run_id: run_id.clone(),
                    stream,
                    text: increment.to_owned(),
                });
            }
            if part["time"]["end"].is_number() && run.open_streams.remove(&part_id).is_some() {
                session.emit(AgentEventKind::MessageCompleted { run_id, stream });
            }
        }
        Some("tool") => tool_part(session, run, part),
        Some("step-finish") => {
            if let Some(usage) = protocol::token_usage(&part["tokens"]) {
                run.usage_fallback = Some(usage);
            }
        }
        _ => {}
    }
}

/// One tool part's state transition, deduplicated by call id: a call
/// emits `tool.started` once on `pending`/`running` and
/// `tool.completed` once on `completed`/`error`.
fn tool_part(session: &Arc<OpenCodeSession>, run: &mut RunFlow, part: &Value) {
    let Some(call_id) = part["callID"].as_str().map(str::to_owned) else {
        return;
    };
    let tool = part["tool"].as_str().unwrap_or("tool").to_owned();
    let run_id = run.run_id.clone();
    let track = run.tools.entry(call_id).or_insert(ToolTrack {
        tool: tool.clone(),
        started: false,
        done: false,
    });
    let status = part["state"]["status"].as_str();
    if !track.started
        && matches!(
            status,
            Some("pending") | Some("running") | Some("completed") | Some("error")
        )
    {
        track.started = true;
        let summary = part["state"]["title"].as_str().unwrap_or(&tool).to_owned();
        session.emit(AgentEventKind::ToolStarted {
            run_id: run_id.clone(),
            tool: track.tool.clone(),
            summary,
        });
    }
    if track.done {
        return;
    }
    match status {
        Some("completed") => {
            track.done = true;
            session.emit(AgentEventKind::ToolCompleted {
                run_id,
                tool: track.tool.clone(),
                ok: true,
                diff: None,
                output: part["state"]["output"]
                    .as_str()
                    .map(protocol::bounded_output),
            });
        }
        Some("error") => {
            track.done = true;
            session.emit(AgentEventKind::ToolCompleted {
                run_id,
                tool: track.tool.clone(),
                ok: false,
                diff: None,
                output: part["state"]["error"]
                    .as_str()
                    .map(protocol::bounded_output),
            });
        }
        _ => {}
    }
}

/// `permission.updated`: park the ask and raise the contract's
/// `approval.requested`. Permissions only exist inside a run.
fn permission_updated(session: &Arc<OpenCodeSession>, flow: &mut SessionFlow, properties: &Value) {
    let Some(run_id) = flow.run.as_ref().map(|run| run.run_id.clone()) else {
        return;
    };
    let Some((permission_id, request)) = protocol::permission_request(properties, session.access)
    else {
        return;
    };
    let request_id = RequestId::new(permission_id.clone());
    {
        let mut state = session.state.lock().expect("session state mutex");
        if state.pending.contains_key(&request_id) {
            return;
        }
        state.pending.insert(
            request_id.clone(),
            PendingPermission {
                permission_id,
                options: request
                    .options
                    .iter()
                    .map(|option| option.id.clone())
                    .collect(),
            },
        );
    }
    emit_status(session, flow, SessionStatus::WaitingApproval);
    session.emit(AgentEventKind::ApprovalRequested {
        run_id,
        request_id,
        request,
    });
}

/// `permission.replied`: another client answered the ask. Remove the
/// pending entry and record the verdict honestly rather than leaving a
/// request that can never be answered.
fn permission_replied(session: &Arc<OpenCodeSession>, flow: &mut SessionFlow, properties: &Value) {
    let Some(permission_id) = properties["permissionID"].as_str() else {
        return;
    };
    let response = properties["response"]
        .as_str()
        .unwrap_or("reject")
        .to_owned();
    let request_id = RequestId::new(permission_id.to_owned());
    let removed = session
        .state
        .lock()
        .expect("session state mutex")
        .pending
        .remove(&request_id)
        .is_some();
    if !removed {
        return;
    }
    session.emit(AgentEventKind::ApprovalResolved {
        request_id,
        decision: ApprovalDecision::OptionId(response),
        answered_by: "server".to_owned(),
    });
    emit_status(session, flow, SessionStatus::Working);
}

/// `session.status`: `idle` only completes the picture outside a run
/// (the prompt response owns in-run completion), `busy` restates
/// working, and `retry` is a provider retry worth surfacing.
fn session_status(session: &Arc<OpenCodeSession>, flow: &mut SessionFlow, properties: &Value) {
    match properties["status"]["type"].as_str() {
        Some("idle") => {
            if flow.run.is_none() {
                emit_status(session, flow, SessionStatus::Idle);
            }
        }
        Some("busy") => emit_status(session, flow, SessionStatus::Working),
        Some("retry") => {
            let message = properties["status"]["message"]
                .as_str()
                .unwrap_or("the provider is retrying the request");
            emit_error_retryable(session, flow, message);
        }
        _ => {}
    }
}

/// `session.error`: a provider-side failure fact, scoped to this run
/// when one is in flight.
fn session_error(session: &Arc<OpenCodeSession>, flow: &mut SessionFlow, properties: &Value) {
    let message = protocol::error_message(&properties["error"])
        .unwrap_or_else(|| "the opencode server reported a session error".to_owned());
    emit_error(session, flow, &message);
}

/// `todo.updated`: the agent's task list inside an active run.
fn todo_updated(session: &Arc<OpenCodeSession>, flow: &mut SessionFlow, properties: &Value) {
    let Some(run) = flow.run.as_ref() else {
        return;
    };
    session.emit(AgentEventKind::TodosUpdated {
        run_id: run.run_id.clone(),
        items: protocol::todo_items(properties),
    });
}

/// The prompt response returned: flush any part state the stream did
/// not surface (the response carries the full part list), close open
/// streams, then emit the terminal outcome and idle status.
pub(super) fn on_prompt_done(
    session: &Arc<OpenCodeSession>,
    flow: &mut SessionFlow,
    outcome: PromptOutcome,
) {
    let Some(mut run) = flow.run.take() else {
        return;
    };
    let run_id = run.run_id.clone();
    let (outcome, usage) = match outcome {
        PromptOutcome::Completed {
            error_name,
            error_message,
            usage,
            parts,
        } => {
            for part in &parts {
                if part["sessionID"].as_str().is_some()
                    && part["messageID"].as_str() != Some(run_id.as_str())
                {
                    handle_part(session, &mut run, part, None);
                }
            }
            let usage = usage.or(run.usage_fallback);
            match error_name.as_deref() {
                None => (RunOutcome::Success, usage),
                Some(name) if run.cancel_requested || name == ABORTED_ERROR_NAME => {
                    (RunOutcome::Cancelled, usage)
                }
                Some(name) => {
                    session.emit(AgentEventKind::Error {
                        run_id: Some(run_id.clone()),
                        code: ReceiptCode::ProviderError,
                        message: error_message.unwrap_or_else(|| format!("the run failed: {name}")),
                        retryable: false,
                    });
                    (RunOutcome::Failed, usage)
                }
            }
        }
        PromptOutcome::Failed(message) => {
            if !run.cancel_requested {
                session.emit(AgentEventKind::Error {
                    run_id: Some(run_id.clone()),
                    code: ReceiptCode::ProviderError,
                    message,
                    retryable: false,
                });
            }
            (
                if run.cancel_requested {
                    RunOutcome::Cancelled
                } else {
                    RunOutcome::Failed
                },
                run.usage_fallback,
            )
        }
    };
    for (_, stream) in std::mem::take(&mut run.open_streams) {
        session.emit(AgentEventKind::MessageCompleted {
            run_id: run_id.clone(),
            stream,
        });
    }
    {
        let mut state = session.state.lock().expect("session state mutex");
        if state.active_run.as_ref() == Some(&run_id) {
            state.active_run = None;
        }
        // A finished run retracts its asks; parked approvals cannot be
        // answered anymore.
        state.pending.clear();
    }
    session.emit(AgentEventKind::RunCompleted {
        run_id,
        outcome,
        usage,
    });
    emit_status(session, flow, SessionStatus::Idle);
}
