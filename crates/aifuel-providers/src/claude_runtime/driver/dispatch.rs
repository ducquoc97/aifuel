//! Provider frame and adapter command dispatch: each parsed frame and
//! each queued command drives exactly the run-scoped emissions it
//! implies, on the driver's causal order.

use super::{RunFlow, close_streams, write_line};
use crate::claude_runtime::protocol::{self, Frame, ResultFrame};
use crate::claude_runtime::session::{ClaudeSession, DriverCommand, PendingApproval};
use aifuel_core::{
    AgentEventKind, AgentRuntimeError, MessageStream, ReceiptCode, RunOutcome, SessionStatus,
};
use std::collections::BTreeMap;
use std::io::Write;

/// One provider line: parse to a frame and dispatch it.
pub(super) fn on_line(
    session: &ClaudeSession,
    line: &str,
    flow: &mut Option<RunFlow>,
    stdin: &mut dyn Write,
) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    match protocol::parse_frame(&value) {
        Frame::Init { session_id } => {
            if let Some(session_id) = session_id {
                session
                    .state
                    .lock()
                    .expect("session state mutex")
                    .claude_session_id = Some(session_id);
            }
        }
        Frame::Status(status) => on_status(session, flow.as_ref(), &status),
        Frame::Assistant { blocks } => on_assistant(session, flow.as_mut(), blocks),
        Frame::ToolResults(results) => on_tool_results(session, flow.as_mut(), results),
        Frame::TextDelta(text) => on_delta(session, flow.as_mut(), MessageStream::Assistant, text),
        Frame::ThinkingDelta(text) => {
            on_delta(session, flow.as_mut(), MessageStream::Thinking, text);
        }
        Frame::StreamToolStart { tool_use_id, name } => {
            if let Some(flow) = flow.as_mut() {
                flow.tool_names.insert(tool_use_id, name);
            }
        }
        Frame::ControlRequest {
            request_id,
            request,
        } => {
            on_control_request(session, flow, request_id, request, stdin);
        }
        Frame::ControlCancel { request_id } => {
            session
                .state
                .lock()
                .expect("session state mutex")
                .pending
                .remove(&aifuel_core::RequestId::new(request_id));
        }
        // Answers to this adapter's own control requests (`interrupt`,
        // `set_model`) carry nothing the contract needs; the
        // wire-write acknowledgement already reported their outcome.
        Frame::ControlResponse { .. } => {}
        Frame::Result(result) => finish_run(session, flow, result),
        Frame::Quota(quota) => session.emit(AgentEventKind::QuotaObserved {
            integration_id: session.integration.clone(),
            quota,
        }),
        Frame::Error(message) => session.emit(AgentEventKind::Error {
            run_id: flow.as_ref().map(|flow| flow.run_id.clone()),
            code: ReceiptCode::ProviderError,
            message,
            retryable: false,
        }),
        Frame::Ignored => {}
    }
}

fn on_status(session: &ClaudeSession, flow: Option<&RunFlow>, status: &str) {
    if flow.is_none() {
        return;
    }
    let mapped = match status {
        "compacting" => Some(SessionStatus::Compacting),
        // The provider is working again after a compacting stretch or
        // an approval wait; `working` is the honest session status.
        "requesting" => Some(SessionStatus::Working),
        _ => None,
    };
    if let Some(status) = mapped {
        session.emit(AgentEventKind::SessionStatus { status });
    }
}

fn on_delta(
    session: &ClaudeSession,
    flow: Option<&mut RunFlow>,
    stream: MessageStream,
    text: String,
) {
    let Some(flow) = flow else { return };
    match stream {
        MessageStream::Assistant => {
            flow.text_streamed = true;
            flow.assistant_open = true;
        }
        MessageStream::Thinking => {
            flow.thinking_streamed = true;
            flow.thinking_open = true;
        }
    }
    session.emit(AgentEventKind::MessageDelta {
        run_id: flow.run_id.clone(),
        stream,
        text,
    });
}

/// One completed `assistant` message. Text and thinking blocks that
/// already streamed as partial deltas are not re-emitted; without
/// partial events the full text lands here as the fallback path the
/// contract requires.
fn on_assistant(session: &ClaudeSession, flow: Option<&mut RunFlow>, blocks: Vec<protocol::Block>) {
    let Some(flow) = flow else { return };
    let mut events = Vec::new();
    for block in blocks {
        match block {
            protocol::Block::Text(text) => {
                if !flow.text_streamed && !text.is_empty() {
                    flow.assistant_open = true;
                    events.push(AgentEventKind::MessageDelta {
                        run_id: flow.run_id.clone(),
                        stream: MessageStream::Assistant,
                        text,
                    });
                }
            }
            protocol::Block::Thinking(thinking) => {
                if !flow.thinking_streamed && !thinking.is_empty() {
                    flow.thinking_open = true;
                    events.push(AgentEventKind::MessageDelta {
                        run_id: flow.run_id.clone(),
                        stream: MessageStream::Thinking,
                        text: thinking,
                    });
                }
            }
            protocol::Block::ToolUse {
                tool_use_id,
                name,
                input,
            } => {
                flow.tool_names.insert(tool_use_id, name.clone());
                events.push(AgentEventKind::ToolStarted {
                    run_id: flow.run_id.clone(),
                    summary: protocol::tool_summary(&name, &input),
                    tool: name,
                });
            }
        }
    }
    close_streams(flow, &mut events);
    // The next API message streams fresh deltas; the streamed flags
    // describe exactly one provider message.
    flow.text_streamed = false;
    flow.thinking_streamed = false;
    for event in events {
        session.emit(event);
    }
}

fn on_tool_results(
    session: &ClaudeSession,
    flow: Option<&mut RunFlow>,
    results: Vec<protocol::ToolResultBlock>,
) {
    let Some(flow) = flow else { return };
    for result in results {
        let tool = flow
            .tool_names
            .get(&result.tool_use_id)
            .cloned()
            .unwrap_or_else(|| "tool".to_owned());
        session.emit(AgentEventKind::ToolCompleted {
            run_id: flow.run_id.clone(),
            tool,
            ok: result.ok,
            diff: None,
            output: result.output,
        });
    }
}

/// A CLI-initiated control request. `can_use_tool` becomes a typed
/// Approval Request and the turn waits on `answer`; every other
/// subtype gets an explicit error reply so the provider never blocks
/// on a request this adapter cannot honor.
fn on_control_request(
    session: &ClaudeSession,
    flow: &mut Option<RunFlow>,
    request_id: String,
    request: serde_json::Value,
    stdin: &mut dyn Write,
) {
    let subtype = request
        .get("subtype")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if subtype != "can_use_tool" {
        let _ = write_line(
            stdin,
            &protocol::error_response(
                &request_id,
                &format!("control request {subtype:?} is not supported"),
            ),
        );
        return;
    }
    let Some(flow) = flow.as_mut() else {
        let _ = write_line(
            stdin,
            &protocol::error_response(&request_id, "no Agent Run is in flight"),
        );
        return;
    };
    let tool_name = request
        .get("tool_name")
        .or_else(|| request.get("display_name"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("tool")
        .to_owned();
    let input = request
        .get("input")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    if let Some(tool_use_id) = request
        .get("tool_use_id")
        .and_then(serde_json::Value::as_str)
    {
        // The tool_result that follows names its tool through this id.
        flow.tool_names
            .insert(tool_use_id.to_owned(), tool_name.clone());
    }
    let approval = protocol::approval_request(&request, session.access);
    let options: Vec<String> = approval
        .options
        .iter()
        .map(|option| option.id.clone())
        .collect();
    // The contract request id wraps the provider's control request id;
    // the parked approval keys on it so `answer` finds the same id the
    // event carried.
    let request_id = aifuel_core::RequestId::new(request_id);
    session
        .state
        .lock()
        .expect("session state mutex")
        .pending
        .insert(request_id.clone(), PendingApproval { input, options });
    session.emit(AgentEventKind::SessionStatus {
        status: SessionStatus::WaitingApproval,
    });
    session.emit(AgentEventKind::ApprovalRequested {
        run_id: flow.run_id.clone(),
        request_id,
        request: approval,
    });
}

/// The terminal fact of a run: close open message streams, report the
/// outcome, then idle. A stream ending without this is a failure, and
/// this emission is what makes a run terminal.
fn finish_run(session: &ClaudeSession, flow: &mut Option<RunFlow>, result: ResultFrame) {
    let Some(mut flow) = flow.take() else {
        return;
    };
    if let Some(session_id) = result.session_id.clone() {
        session
            .state
            .lock()
            .expect("session state mutex")
            .claude_session_id = Some(session_id);
    }
    let mut events = Vec::new();
    close_streams(&mut flow, &mut events);
    for event in events {
        session.emit(event);
    }
    let cancelled = protocol::is_aborted(&result) || (flow.cancel_requested && result.is_error);
    let outcome = if cancelled {
        RunOutcome::Cancelled
    } else if result.is_error {
        session.emit(AgentEventKind::Error {
            run_id: Some(flow.run_id.clone()),
            code: ReceiptCode::ProviderError,
            message: result
                .message
                .clone()
                .unwrap_or_else(|| "the provider reported a failed run".to_owned()),
            retryable: false,
        });
        RunOutcome::Failed
    } else {
        RunOutcome::Success
    };
    session.emit(AgentEventKind::RunCompleted {
        run_id: flow.run_id.clone(),
        outcome,
        usage: result.usage,
    });
    let closed = {
        let mut state = session.state.lock().expect("session state mutex");
        state.active_run = None;
        state.pending.clear();
        state.closed
    };
    if !closed {
        session.emit(AgentEventKind::SessionStatus {
            status: SessionStatus::Idle,
        });
    }
}

/// One adapter command. `Shutdown` stays in the driver loop: it owns
/// the stdin box it must drop to close the provider's input.
pub(super) fn on_command(
    session: &ClaudeSession,
    command: DriverCommand,
    flow: &mut Option<RunFlow>,
    stdin: &mut dyn Write,
) {
    match command {
        DriverCommand::UserMessage {
            run_id,
            text,
            reply,
        } => {
            *flow = Some(RunFlow {
                run_id: run_id.clone(),
                cancel_requested: false,
                assistant_open: false,
                thinking_open: false,
                text_streamed: false,
                thinking_streamed: false,
                tool_names: BTreeMap::new(),
            });
            let selection = session
                .state
                .lock()
                .expect("session state mutex")
                .selection
                .clone();
            session.emit(AgentEventKind::RunStarted {
                run_id: run_id.clone(),
                selection,
            });
            session.emit(AgentEventKind::SessionStatus {
                status: SessionStatus::Working,
            });
            let session_id = session
                .state
                .lock()
                .expect("session state mutex")
                .claude_session_id
                .clone();
            let line = protocol::user_message(session_id.as_deref(), &text);
            let result = match write_line(stdin, &line) {
                Ok(()) => Ok(()),
                Err(error) => {
                    let flow = flow.take().expect("the run was just registered");
                    session.emit(AgentEventKind::Error {
                        run_id: Some(run_id.clone()),
                        code: ReceiptCode::ProviderError,
                        message: format!("the user message could not reach the provider: {error}"),
                        retryable: false,
                    });
                    session.emit(AgentEventKind::RunCompleted {
                        run_id: flow.run_id,
                        outcome: RunOutcome::Failed,
                        usage: None,
                    });
                    session
                        .state
                        .lock()
                        .expect("session state mutex")
                        .active_run = None;
                    Err(AgentRuntimeError::provider_error(format!(
                        "the user message could not reach the provider: {error}"
                    )))
                }
            };
            let _ = reply.send(result);
        }
        DriverCommand::Answer {
            request_id,
            decision,
            line,
            interrupt,
            reply,
        } => {
            let result = match write_line(stdin, &line) {
                Ok(()) => {
                    session.emit(AgentEventKind::ApprovalResolved {
                        request_id: request_id.clone(),
                        decision,
                        // Placeholder the runtime pump rewrites with the
                        // answering consumer id.
                        answered_by: "adapter".to_owned(),
                    });
                    session.emit(AgentEventKind::SessionStatus {
                        status: SessionStatus::Working,
                    });
                    if interrupt {
                        let line =
                            protocol::interrupt_request(&session.next_wire_id("aifuel-interrupt"));
                        if write_line(stdin, &line).is_ok()
                            && let Some(flow) = flow.as_mut()
                        {
                            flow.cancel_requested = true;
                        }
                    }
                    Ok(())
                }
                Err(error) => Err(AgentRuntimeError::provider_error(format!(
                    "the approval answer could not reach the provider: {error}"
                ))),
            };
            let _ = reply.send(result);
        }
        DriverCommand::Interrupt { run_id, reply } => {
            let line = protocol::interrupt_request(&session.next_wire_id("aifuel-interrupt"));
            let result = match write_line(stdin, &line) {
                Ok(()) => {
                    if let Some(flow) = flow.as_mut()
                        && flow.run_id == run_id
                    {
                        flow.cancel_requested = true;
                    }
                    Ok(())
                }
                Err(error) => Err(AgentRuntimeError::provider_error(format!(
                    "the interrupt could not reach the provider: {error}"
                ))),
            };
            let _ = reply.send(result);
        }
        DriverCommand::SetModel { model, reply } => {
            let line = protocol::set_model_request(
                &session.next_wire_id("aifuel-model"),
                model.as_deref(),
            );
            let result = write_line(stdin, &line).map_err(|error| {
                AgentRuntimeError::provider_error(format!(
                    "the model change could not reach the provider: {error}"
                ))
            });
            let _ = reply.send(result);
        }
        DriverCommand::Shutdown => {}
    }
}
