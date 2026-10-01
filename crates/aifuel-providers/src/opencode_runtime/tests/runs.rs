//! Run lifecycle: prompt dispatch, SSE mapping, completion, abort, and
//! the guards around a busy session.

use super::*;
use aifuel_core::{AgentAdapter, MessageStream, ReceiptCode, RunOutcome};

/// The successful prompt response the fake settles with.
fn done_response() -> Value {
    json!({
        "info": {
            "id": "msg_asst",
            "tokens": {"input": 5, "output": 7, "reasoning": 0, "cache": {"read": 0, "write": 0}},
        },
        "parts": [],
    })
}

#[test]
fn a_run_streams_deltas_tools_todos_and_completes() {
    let fake = FakeServe::start();
    let adapter = adapter_with(&fake);
    let (handle, events) = start_session(&adapter, &fake, options(AccessMode::WorkspaceWrite));
    expect_prelude(&events);

    fake.hold_prompt();
    let run = adapter
        .send(&handle, input("hello"))
        .expect("the run starts");
    assert!(matches!(recv(&events), AgentEventKind::RunStarted { .. }));
    assert!(matches!(
        recv(&events),
        AgentEventKind::SessionStatus {
            status: SessionStatus::Working
        }
    ));

    // The prompt request went out with the run's message id.
    let prompt = wait_for_request(&fake, "/message");
    let prompt_body: Value = serde_json::from_str(&prompt.body).expect("prompt body is JSON");
    assert_eq!(prompt_body["messageID"], run.as_str());
    assert_eq!(prompt_body["parts"][0]["type"], "text");

    // The user-message echo is dropped, not streamed.
    fake.push_event(part_updated(text_part(
        "prt_user",
        FAKE_SESSION,
        run.as_str(),
        "hello",
        true,
    )));
    // Assistant text, cumulative on the part object.
    fake.push_event(part_updated(text_part(
        "prt_1",
        FAKE_SESSION,
        "msg_asst",
        "Hello",
        false,
    )));
    fake.push_event(part_updated(text_part(
        "prt_1",
        FAKE_SESSION,
        "msg_asst",
        "Hello world",
        true,
    )));
    // Reasoning streams on the thinking channel.
    let mut reasoning = text_part("prt_r", FAKE_SESSION, "msg_asst", "let me think", true);
    reasoning["type"] = json!("reasoning");
    fake.push_event(part_updated(reasoning));
    // A tool call runs and completes.
    fake.push_event(part_updated(tool_part(
        "prt_t",
        FAKE_SESSION,
        "msg_asst",
        "call_1",
        "bash",
        json!({"status": "running", "input": {}, "title": "list files", "time": {"start": 1}}),
    )));
    fake.push_event(part_updated(tool_part(
        "prt_t",
        FAKE_SESSION,
        "msg_asst",
        "call_1",
        "bash",
        json!({"status": "completed", "input": {}, "output": "a.rs", "title": "list files",
               "time": {"start": 1, "end": 2}}),
    )));
    // The task list updates inside the run.
    fake.push_event(json!({
        "type": "todo.updated",
        "properties": {"sessionID": FAKE_SESSION, "todos": [
            {"id": "t1", "content": "write code", "status": "in_progress", "priority": "high"},
            {"id": "t2", "content": "cancelled work", "status": "cancelled", "priority": "low"},
        ]},
    }));
    // Events for another session are dropped.
    fake.push_event(part_updated(text_part(
        "prt_other",
        "ses_other",
        "msg_x",
        "not ours",
        false,
    )));

    // The pushed bus events must land before the held prompt response;
    // once `run.completed` is emitted a late event would have no run.
    let mut kinds = through_match(&events, |kind| {
        matches!(kind, AgentEventKind::TodosUpdated { .. })
    });
    fake.settle_prompt(done_response());
    kinds.extend(through_idle(&events));

    let deltas: Vec<&str> = kinds
        .iter()
        .filter_map(|kind| match kind {
            AgentEventKind::MessageDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, ["Hello", " world", "let me think"]);
    let streams: Vec<MessageStream> = kinds
        .iter()
        .filter_map(|kind| match kind {
            AgentEventKind::MessageDelta { stream, .. } => Some(*stream),
            _ => None,
        })
        .collect();
    assert_eq!(
        streams,
        [
            MessageStream::Assistant,
            MessageStream::Assistant,
            MessageStream::Thinking
        ]
    );
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::MessageCompleted {
            stream: MessageStream::Assistant,
            ..
        }
    )));
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::ToolStarted { tool, .. } if tool == "bash"
    )));
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::ToolCompleted { ok: true, output: Some(output), .. } if output == "a.rs"
    )));
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::TodosUpdated { items, .. }
            if items.len() == 2 && items[1].status == aifuel_core::TodoItemStatus::Completed
    )));
    assert!(matches!(
        kinds
            .iter()
            .find(|kind| matches!(kind, AgentEventKind::RunCompleted { .. })),
        Some(AgentEventKind::RunCompleted {
            outcome: RunOutcome::Success,
            usage: Some(aifuel_core::TokenUsage {
                input_tokens: Some(5),
                output_tokens: Some(7),
            }),
            ..
        })
    ));
}

#[test]
fn the_response_parts_flush_covers_events_the_stream_missed() {
    let fake = FakeServe::start();
    let adapter = adapter_with(&fake);
    let (handle, events) = start_session(&adapter, &fake, options(AccessMode::WorkspaceWrite));
    expect_prelude(&events);

    fake.hold_prompt();
    adapter.send(&handle, input("hi")).expect("the run starts");
    let mut response = done_response();
    // A text part the stream never delivered still lands, once.
    response["parts"] = json!([
        {"id": "prt_late", "sessionID": FAKE_SESSION, "messageID": "msg_asst",
         "type": "text", "text": "late text", "time": {"start": 1, "end": 2}},
    ]);
    fake.settle_prompt(response);
    let kinds = through_idle(&events);
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::MessageDelta { text, .. } if text == "late text"
    )));
}

#[test]
fn a_provider_error_fails_the_run() {
    let fake = FakeServe::start();
    let adapter = adapter_with(&fake);
    let (handle, events) = start_session(&adapter, &fake, options(AccessMode::WorkspaceWrite));
    expect_prelude(&events);

    fake.hold_prompt();
    adapter.send(&handle, input("hi")).expect("the run starts");
    fake.settle_prompt(json!({
        "info": {
            "id": "msg_asst",
            "error": {"name": "ProviderAuthError", "data": {"message": "no credentials"}},
        },
        "parts": [],
    }));
    let kinds = through_idle(&events);
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::Error { message, .. } if message == "no credentials"
    )));
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::RunCompleted {
            outcome: RunOutcome::Failed,
            ..
        }
    )));
}

#[test]
fn cancel_posts_abort_and_the_run_reports_cancelled() {
    let fake = FakeServe::start();
    let adapter = adapter_with(&fake);
    let (handle, events) = start_session(&adapter, &fake, options(AccessMode::WorkspaceWrite));
    expect_prelude(&events);

    fake.hold_prompt();
    let run = adapter.send(&handle, input("hi")).expect("the run starts");
    adapter
        .cancel(&handle, run.clone())
        .expect("cancel is accepted");
    assert!(
        requested(&fake, &format!("/session/{FAKE_SESSION}/abort")),
        "the abort route was posted"
    );
    fake.settle_prompt(json!({
        "info": {
            "id": "msg_asst",
            "error": {"name": "MessageAbortedError", "data": {"message": "aborted"}},
        },
        "parts": [],
    }));
    let kinds = through_idle(&events);
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::RunCompleted {
            outcome: RunOutcome::Cancelled,
            ..
        }
    )));
}

#[test]
fn a_second_send_is_rejected_while_a_run_is_in_flight() {
    let fake = FakeServe::start();
    let adapter = adapter_with(&fake);
    let (handle, _events) = start_session(&adapter, &fake, options(AccessMode::WorkspaceWrite));

    fake.hold_prompt();
    adapter
        .send(&handle, input("first"))
        .expect("the run starts");
    let error = adapter
        .send(&handle, input("second"))
        .expect_err("one run per session");
    assert_eq!(error.code, ReceiptCode::InvalidState);
    fake.settle_prompt(done_response());
}

#[test]
fn cancel_without_a_run_is_rejected() {
    let fake = FakeServe::start();
    let adapter = adapter_with(&fake);
    let (handle, _events) = start_session(&adapter, &fake, options(AccessMode::WorkspaceWrite));
    let error = adapter
        .cancel(&handle, aifuel_core::RunId::new("msg_nothing"))
        .expect_err("no run to cancel");
    assert_eq!(error.code, ReceiptCode::InvalidState);
}

#[test]
fn events_for_other_sessions_are_dropped() {
    let fake = FakeServe::start();
    let adapter = adapter_with(&fake);
    let (handle, events) = start_session(&adapter, &fake, options(AccessMode::WorkspaceWrite));
    expect_prelude(&events);

    fake.hold_prompt();
    adapter.send(&handle, input("hi")).expect("the run starts");
    // A foreign session's part and permission events never surface.
    fake.push_event(part_updated(text_part(
        "prt_x",
        "ses_other",
        "msg_x",
        "foreign",
        false,
    )));
    fake.push_event(json!({
        "type": "permission.updated",
        "properties": {"id": "per_x", "sessionID": "ses_other", "messageID": "m",
                       "type": "bash", "title": "foreign", "metadata": {}, "time": {"created": 1}},
    }));
    fake.settle_prompt(done_response());
    let kinds = through_idle(&events);
    assert!(!kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::MessageDelta { text, .. } if text == "foreign"
    )));
    assert!(
        !kinds
            .iter()
            .any(|kind| matches!(kind, AgentEventKind::ApprovalRequested { .. }))
    );
}
