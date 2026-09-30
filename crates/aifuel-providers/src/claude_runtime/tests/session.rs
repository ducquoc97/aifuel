//! Session lifecycle coverage over the scripted duplex: ordering,
//! streaming, approvals, cancellation, resume cursors, and teardown.
//! The fake transport answers the `initialize` control request with
//! the `system`/`init` frame a real `claude` sends.

use super::*;
use aifuel_core::{
    AccessMode, AgentAdapter, AgentEventKind, AgentRuntimeError, AgentSessionHandle,
    ApprovalDecision, ApprovalKind, Effort, MessageStream, ModelSelection, ReceiptCode, RunId,
    RunOutcome, SessionStatus,
};
use serde_json::Value;

/// The frames a text-only turn emits from the provider side.
fn text_turn_reply() -> Vec<String> {
    vec![
        "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello, \"}}}".to_owned(),
        "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"world\"}}}".to_owned(),
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Hello, world\"}]}}".to_owned(),
        "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"sess-1\",\"usage\":{\"input_tokens\":4,\"output_tokens\":2}}".to_owned(),
    ]
}

/// Start a session over a scripted endpoint and return the adapter,
/// script, handle, and event receiver. `on_user` plays the provider's
/// turn replies.
fn running_session(
    on_user: impl Fn(&dyn Fn(&str)) + Send + 'static,
) -> (
    ClaudeAdapter,
    Arc<Script>,
    AgentSessionHandle,
    mpsc::Receiver<AgentEventKind>,
) {
    running_session_with(with_init("sess-1", on_user))
}

/// `running_session` over a full `respond` closure, for tests whose
/// provider replies to more than user messages.
fn running_session_with(
    respond: impl FnMut(&str, &mpsc::Sender<String>) + Send + 'static,
) -> (
    ClaudeAdapter,
    Arc<Script>,
    AgentSessionHandle,
    mpsc::Receiver<AgentEventKind>,
) {
    let (script, connector) = scripted(respond);
    let adapter = adapter_with(connector);
    let handle = adapter
        .start(&integration(), options(AccessMode::WorkspaceWrite))
        .expect("the scripted session starts");
    let events = take_events(&adapter, &handle);
    (adapter, script, handle, events)
}

fn send_and_wait(adapter: &ClaudeAdapter, handle: &AgentSessionHandle, text: &str) -> RunId {
    adapter
        .send(handle, input(text))
        .expect("the run reaches the wire")
}

#[test]
fn start_reports_the_provider_session_and_goes_idle() {
    let (script, connector) = scripted(with_init("sess-42", |_| {}));
    let adapter = adapter_with(connector);
    let handle = adapter
        .start(&integration(), options(AccessMode::WorkspaceWrite))
        .expect("the scripted session starts");
    // The init frame's session id becomes the resume cursor.
    assert_eq!(handle.provider_session.as_deref(), Some("sess-42"));
    assert_eq!(
        adapter.resume_cursor(&handle.session_id).as_deref(),
        Some("sess-42")
    );
    let events = take_events(&adapter, &handle);
    assert!(matches!(
        recv(&events),
        AgentEventKind::SessionCreated { .. }
    ));
    assert!(matches!(
        recv(&events),
        AgentEventKind::SessionStatus {
            status: SessionStatus::Idle
        }
    ));
    // The initialize handshake reached the wire before init arrived.
    assert!(script.written_lines()[0].contains("\"initialize\""));
    adapter.stop(handle).expect("stop succeeds");
}

#[test]
fn a_text_turn_streams_deltas_then_completes() {
    let (adapter, script, handle, events) = running_session(|push| {
        for line in text_turn_reply() {
            push(&line);
        }
    });
    recv(&events); // session.created
    recv(&events); // idle
    send_and_wait(&adapter, &handle, "say hello");

    let user_line = script
        .written_lines()
        .into_iter()
        .find(|line| line.contains("\"type\":\"user\""))
        .expect("the user message reached the wire");
    let user: Value = serde_json::from_str(&user_line).unwrap();
    assert_eq!(user["session_id"], "sess-1");
    assert_eq!(user["message"]["content"][0]["text"], "say hello");

    let seen = through_idle(&events);
    let kinds: Vec<&str> = seen.iter().map(event_name).collect();
    assert_eq!(
        kinds,
        [
            "run.started",
            "session.status",
            "message.delta",
            "message.delta",
            "message.completed",
            "run.completed",
            "session.status",
        ]
    );
    let deltas: Vec<&str> = seen
        .iter()
        .filter_map(|kind| match kind {
            AgentEventKind::MessageDelta { text, stream, .. }
                if *stream == MessageStream::Assistant =>
            {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(deltas.concat(), "Hello, world");
    let completed = seen
        .iter()
        .find(|kind| matches!(kind, AgentEventKind::RunCompleted { .. }));
    assert!(matches!(
        completed,
        Some(AgentEventKind::RunCompleted {
            outcome: RunOutcome::Success,
            usage: Some(_),
            ..
        })
    ));
    adapter.stop(handle).expect("stop succeeds");
}

#[test]
fn assistant_frames_answer_when_no_deltas_stream() {
    // Without stream_events the completed assistant message carries
    // the answer: the fallback path still emits it as a delta plus a
    // completion, rather than dropping the content.
    let (adapter, _script, handle, events) = running_session(|push| {
        push(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"whole answer\"}]}}",
        );
        push("{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}");
    });
    recv(&events);
    recv(&events);
    send_and_wait(&adapter, &handle, "question");
    let seen = through_idle(&events);
    let delta = seen.iter().find_map(|kind| match kind {
        AgentEventKind::MessageDelta { text, .. } => Some(text.clone()),
        _ => None,
    });
    assert_eq!(delta.as_deref(), Some("whole answer"));
    assert!(seen.iter().any(|kind| matches!(
        kind,
        AgentEventKind::MessageCompleted {
            stream: MessageStream::Assistant,
            ..
        }
    )));
    adapter.stop(handle).expect("stop succeeds");
}

#[test]
fn tool_use_and_tool_result_map_to_tool_events() {
    let (adapter, _script, handle, events) = running_session(|push| {
        push(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Write\",\"input\":{\"file_path\":\"/tmp/x.txt\"}}]}}",
        );
        push(
            "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_1\",\"content\":\"wrote 4 bytes\"}]}}",
        );
        push(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}",
        );
        push("{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}");
    });
    recv(&events);
    recv(&events);
    send_and_wait(&adapter, &handle, "write the file");
    let seen = through_idle(&events);
    let started = seen.iter().find_map(|kind| match kind {
        AgentEventKind::ToolStarted { tool, summary, .. } => Some((tool.clone(), summary.clone())),
        _ => None,
    });
    let (tool, summary) = started.expect("tool.started was emitted");
    assert_eq!(tool, "Write");
    assert!(summary.contains("/tmp/x.txt"));
    let completed = seen.iter().find_map(|kind| match kind {
        AgentEventKind::ToolCompleted {
            tool, ok, output, ..
        } => Some((tool.clone(), *ok, output.clone())),
        _ => None,
    });
    let (tool, ok, output) = completed.expect("tool.completed was emitted");
    assert_eq!((tool.as_str(), ok), ("Write", true));
    assert_eq!(output.as_deref(), Some("wrote 4 bytes"));
    adapter.stop(handle).expect("stop succeeds");
}

#[test]
fn a_permission_request_round_trips_through_answer() {
    // The turn finishes only after the host answers: the provider
    // raises the request on the user message and reports the result
    // once the control_response lands.
    let (adapter, script, handle, events) = running_session_with(with_init_then(
        "sess-1",
        |push| {
            push(
                "{\"type\":\"control_request\",\"request_id\":\"req-1\",\"request\":{\"subtype\":\"can_use_tool\",\"tool_name\":\"Write\",\"input\":{\"file_path\":\"/tmp/x.txt\"},\"description\":\"Write x.txt\",\"tool_use_id\":\"toolu_1\"}}",
            );
        },
        |push| {
            push(
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"wrote it\"}]}}",
            );
            push("{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}");
        },
    ));
    recv(&events);
    recv(&events);
    send_and_wait(&adapter, &handle, "create x.txt");

    // The turn waits: run.started, working, waiting status, then the
    // typed request.
    let request_id = loop {
        match recv(&events) {
            AgentEventKind::ApprovalRequested {
                request_id,
                request,
                ..
            } => {
                assert_eq!(request.kind, ApprovalKind::ToolPermission);
                assert!(request.options.iter().any(|option| option.id == "accept"));
                break request_id;
            }
            AgentEventKind::SessionStatus {
                status: SessionStatus::WaitingApproval,
            }
            | AgentEventKind::SessionStatus {
                status: SessionStatus::Working,
            }
            | AgentEventKind::RunStarted { .. } => {}
            other => panic!("unexpected event while waiting: {other:?}"),
        }
    };
    adapter
        .answer(
            &handle,
            request_id.clone(),
            ApprovalDecision::OptionId("accept".into()),
        )
        .expect("the answer reaches the wire");

    let response = script
        .written_lines()
        .into_iter()
        .find(|line| line.contains("\"control_response\""))
        .expect("the control response reached the wire");
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["response"]["request_id"], "req-1");
    assert_eq!(response["response"]["response"]["behavior"], "allow");
    assert_eq!(
        response["response"]["response"]["updatedInput"],
        serde_json::json!({"file_path": "/tmp/x.txt"})
    );

    let seen = through_idle(&events);
    assert!(seen.iter().any(|kind| matches!(
        kind,
        AgentEventKind::ApprovalResolved {
            decision: ApprovalDecision::OptionId(id),
            ..
        } if id == "accept"
    )));
    assert!(seen.iter().any(|kind| matches!(
        kind,
        AgentEventKind::RunCompleted {
            outcome: RunOutcome::Success,
            ..
        }
    )));
    // A second answer loses: the request is already resolved.
    let error = adapter
        .answer(
            &handle,
            request_id,
            ApprovalDecision::OptionId("decline".into()),
        )
        .expect_err("the second answer loses");
    assert_eq!(error.code, ReceiptCode::AlreadyResolved);
    adapter.stop(handle).expect("stop succeeds");
}

#[test]
fn declining_a_permission_writes_a_deny_response() {
    let (adapter, script, handle, events) = running_session_with(with_init_then(
        "sess-1",
        |push| {
            push(
                "{\"type\":\"control_request\",\"request_id\":\"req-2\",\"request\":{\"subtype\":\"can_use_tool\",\"tool_name\":\"Bash\",\"input\":{\"command\":\"rm -rf /\"}}}",
            );
        },
        |push| {
            push("{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}");
        },
    ));
    recv(&events);
    recv(&events);
    send_and_wait(&adapter, &handle, "clean up");
    let request_id = loop {
        match recv(&events) {
            AgentEventKind::ApprovalRequested { request_id, .. } => break request_id,
            AgentEventKind::SessionStatus {
                status: SessionStatus::WaitingApproval,
            }
            | AgentEventKind::SessionStatus {
                status: SessionStatus::Working,
            }
            | AgentEventKind::RunStarted { .. } => {}
            other => panic!("unexpected event: {other:?}"),
        }
    };
    adapter
        .answer(
            &handle,
            request_id,
            ApprovalDecision::OptionId("decline".into()),
        )
        .expect("the decline reaches the wire");
    let response = script
        .written_lines()
        .into_iter()
        .find(|line| line.contains("\"control_response\""))
        .expect("the control response reached the wire");
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["response"]["response"]["behavior"], "deny");
    assert!(response["response"]["response"]["message"].is_string());
    let seen = through_idle(&events);
    assert!(seen.iter().any(|kind| matches!(
        kind,
        AgentEventKind::ApprovalResolved {
            decision: ApprovalDecision::OptionId(id),
            ..
        } if id == "decline"
    )));
    adapter.stop(handle).expect("stop succeeds");
}

#[test]
fn cancel_writes_an_interrupt_and_reports_cancelled() {
    // The provider is mid-turn: it streamed one delta but never
    // finished. `cancel` writes the protocol interrupt; the aborted
    // result that follows reports `cancelled`, not `failed`.
    let (adapter, script, handle, events) = running_session(|push| {
        push(
            "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"half\"}}}",
        );
    });
    recv(&events);
    recv(&events);
    let run_id = send_and_wait(&adapter, &handle, "long answer");
    recv(&events); // run.started
    recv(&events); // working
    recv(&events); // the streamed delta

    adapter
        .cancel(&handle, run_id.clone())
        .expect("cancel reaches the wire");
    let interrupt = script
        .written_lines()
        .into_iter()
        .find(|line| line.contains("\"interrupt\""))
        .expect("the interrupt reached the wire");
    let interrupt: Value = serde_json::from_str(&interrupt).unwrap();
    assert_eq!(interrupt["type"], "control_request");
    assert_eq!(interrupt["request"]["subtype"], "interrupt");

    // The provider aborts the turn after the interrupt.
    script.push_line("{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"terminal_reason\":\"aborted_streaming\",\"result\":null}");
    let seen = through_idle(&events);
    assert!(seen.iter().any(|kind| matches!(
        kind,
        AgentEventKind::RunCompleted {
            outcome: RunOutcome::Cancelled,
            ..
        }
    )));
    adapter.stop(handle).expect("stop succeeds");
}

#[test]
fn a_second_send_loses_while_a_run_is_in_flight() {
    // The script replies nothing to the user message, so the run stays
    // in flight and the next send fails fast instead of queueing.
    let (adapter, _script, handle, events) = running_session(|_| {});
    recv(&events);
    recv(&events);
    send_and_wait(&adapter, &handle, "first");
    let error = adapter
        .send(&handle, input("second"))
        .expect_err("the second run cannot start");
    assert_eq!(error.code, ReceiptCode::InvalidState);
    adapter.stop(handle).expect("stop succeeds");
}

#[test]
fn resume_cursor_updates_from_the_result_frame() {
    let (adapter, _script, handle, events) = running_session(|push| {
        push(
            "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"sess-next\"}",
        );
    });
    recv(&events);
    recv(&events);
    send_and_wait(&adapter, &handle, "turn");
    through_idle(&events);
    assert_eq!(
        adapter.resume_cursor(&handle.session_id).as_deref(),
        Some("sess-next")
    );
    adapter.stop(handle).expect("stop succeeds");
}

#[test]
fn resume_cursor_and_setup_fields_reach_the_spawn_request() {
    // The connector sees exactly what start would pass to the process:
    // working directory, access mode, model, effort, and the resume
    // cursor the runtime persisted.
    let (setup_tx, setup_rx) = mpsc::channel();
    let connector: crate::claude_runtime::session::Connector = Arc::new(move |setup| {
        setup_tx
            .send((
                setup.cwd.clone(),
                setup.access,
                setup.model.clone(),
                setup.effort,
                setup.resume_cursor.clone(),
            ))
            .unwrap();
        Err(AgentRuntimeError::provider_error(
            "scripted spawn stops here",
        ))
    });
    let adapter = adapter_with(connector);
    let mut options = options(AccessMode::ReadOnly);
    options.selection.effort = Some(Effort::Medium);
    options.resume_cursor = Some("sess-persisted".to_owned());
    let error = adapter
        .start(&integration(), options)
        .expect_err("the scripted spawn fails the start");
    assert_eq!(error.code, ReceiptCode::ProviderError);
    let (cwd, access, model, effort, resume) =
        setup_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(cwd, PathBuf::from("/tmp"));
    assert_eq!(access, AccessMode::ReadOnly);
    assert_eq!(model.as_deref(), Some("claude-test-model"));
    assert_eq!(effort, Some(Effort::Medium));
    assert_eq!(resume.as_deref(), Some("sess-persisted"));
}

#[test]
fn stop_cancels_an_in_flight_run_and_closes_the_session() {
    let (adapter, script, handle, events) = running_session(|_| {});
    recv(&events);
    recv(&events);
    send_and_wait(&adapter, &handle, "unfinished turn");
    adapter.stop(handle).expect("stop succeeds");
    let mut cancelled = false;
    let mut closed = false;
    for _ in 0..16 {
        match recv(&events) {
            AgentEventKind::RunCompleted {
                outcome: RunOutcome::Cancelled,
                ..
            } => cancelled = true,
            AgentEventKind::SessionClosed { .. } => {
                closed = true;
                break;
            }
            _ => {}
        }
    }
    assert!(cancelled, "the in-flight run reports cancelled");
    assert!(closed, "session.closed is emitted");
    assert!(script.stdin_closed.load(Ordering::SeqCst));
}

#[test]
fn a_provider_death_fails_the_run_and_closes() {
    let (adapter, script, handle, events) = running_session(|_| {});
    recv(&events);
    recv(&events);
    send_and_wait(&adapter, &handle, "doomed turn");
    // The provider process dies mid-turn: stdout EOF without a result.
    script.kill_provider();
    let mut saw_error = false;
    let mut saw_failed = false;
    let mut saw_closed = false;
    for _ in 0..16 {
        match recv(&events) {
            AgentEventKind::Error { .. } => saw_error = true,
            AgentEventKind::RunCompleted {
                outcome: RunOutcome::Failed,
                ..
            } => saw_failed = true,
            AgentEventKind::SessionClosed { .. } => {
                saw_closed = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_error && saw_failed && saw_closed);
    adapter
        .stop(handle)
        .expect("stop succeeds on a dead session");
}

#[test]
fn unsupported_control_requests_get_an_error_reply() {
    // A subtype the adapter does not implement still gets an answer:
    // the CLI would otherwise block for a minute on it.
    let (adapter, script, handle, events) = running_session(|push| {
        push(
            "{\"type\":\"control_request\",\"request_id\":\"req-odd\",\"request\":{\"subtype\":\"elicitation\",\"prompt\":\"name?\"}}",
        );
        push("{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}");
    });
    recv(&events);
    recv(&events);
    send_and_wait(&adapter, &handle, "odd turn");
    through_idle(&events);
    let reply = script
        .written_lines()
        .into_iter()
        .find(|line| line.contains("req-odd"))
        .expect("the error reply reached the wire");
    let reply: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["response"]["subtype"], "error");
    adapter.stop(handle).expect("stop succeeds");
}

#[test]
fn model_select_writes_a_set_model_control_request() {
    let (adapter, script, handle, events) = running_session(|push| {
        push("{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}");
    });
    recv(&events);
    recv(&events);
    send_and_wait(&adapter, &handle, "first turn");
    through_idle(&events);
    let selection = ModelSelection {
        integration_id: integration().id.clone(),
        model: "claude-opus".to_owned(),
        effort: None,
    };
    adapter
        .set_selection(&handle, selection)
        .expect("the resolved selection applies");
    let line = script
        .written_lines()
        .into_iter()
        .find(|line| line.contains("set_model"))
        .expect("the set_model request reached the wire");
    let line: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(line["request"]["model"], "claude-opus");
    adapter.stop(handle).expect("stop succeeds");
}

#[test]
fn events_expose_honest_capabilities_and_unsupported_surfaces() {
    let (script, connector) = scripted(with_init("sess-1", |_| {}));
    let adapter = adapter_with(connector);
    let capabilities = adapter.capabilities();
    assert!(capabilities.streaming && capabilities.resume && capabilities.approvals);
    assert!(!capabilities.checkpoints && !capabilities.images && !capabilities.todos);
    assert!(capabilities.effort);
    let _ = script;

    // No catalog interface exists for Claude, so the advertised list
    // is empty rather than fabricated.
    assert!(adapter.list_models(&integration()).unwrap().is_empty());
    let handle = adapter
        .start(&integration(), options(AccessMode::WorkspaceWrite))
        .expect("the scripted session starts");
    let error = adapter
        .checkpoint(&handle, RunId::new("run-1"))
        .expect_err("checkpoints are unsupported");
    assert_eq!(error.code, ReceiptCode::Unsupported);
    adapter.stop(handle).expect("stop succeeds");
}

/// The serialized event name, for compact ordering assertions.
fn event_name(kind: &AgentEventKind) -> &'static str {
    match kind {
        AgentEventKind::SessionCreated { .. } => "session.created",
        AgentEventKind::SessionStatus { .. } => "session.status",
        AgentEventKind::SessionClosed { .. } => "session.closed",
        AgentEventKind::RunStarted { .. } => "run.started",
        AgentEventKind::RunCompleted { .. } => "run.completed",
        AgentEventKind::MessageDelta { .. } => "message.delta",
        AgentEventKind::MessageCompleted { .. } => "message.completed",
        AgentEventKind::ToolStarted { .. } => "tool.started",
        AgentEventKind::ToolCompleted { .. } => "tool.completed",
        AgentEventKind::TodosUpdated { .. } => "todos.updated",
        AgentEventKind::ApprovalRequested { .. } => "approval.requested",
        AgentEventKind::ApprovalResolved { .. } => "approval.resolved",
        AgentEventKind::CheckpointCreated { .. } => "checkpoint.created",
        AgentEventKind::CheckpointRestored { .. } => "checkpoint.restored",
        AgentEventKind::QuotaObserved { .. } => "quota.observed",
        AgentEventKind::Error { .. } => "error",
    }
}
