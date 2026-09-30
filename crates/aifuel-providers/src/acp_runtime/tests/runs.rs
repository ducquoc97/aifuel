//! Prompt runs: content blocks on the wire, streamed updates mapped
//! into run events, cancellation, failure, and the workspace file
//! services.

use super::*;
use aifuel_core::{AgentAdapter, MessageStream, ReceiptCode, RunOutcome};
use tokio::io::AsyncWriteExt;

#[test]
fn a_prompt_sends_content_blocks_streams_deltas_and_completes() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent.handshake("sess-1").await;
        let prompt = agent.next_method("session/prompt").await;
        assert_eq!(prompt["params"]["sessionId"], "sess-1");
        let blocks = prompt["params"]["prompt"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "hello agent");
        agent
            .notify_update(json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "Hello"},
            }))
            .await;
        agent
            .notify_update(json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": " back"},
            }))
            .await;
        agent
            .notify_update(json!({
                "sessionUpdate": "agent_thought_chunk",
                "content": {"type": "text", "text": "thinking"},
            }))
            .await;
        agent
            .respond(&prompt, json!({"stopReason": "end_turn"}))
            .await;
        agent.park().await;
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::WorkspaceWrite))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.send(&handle, input("hello agent")).expect("send");
    let kinds = through_run_completed(&events);
    assert!(matches!(kinds[0], AgentEventKind::RunStarted { .. }));
    assert!(matches!(
        kinds[1],
        AgentEventKind::SessionStatus {
            status: SessionStatus::Working
        }
    ));
    let deltas: Vec<&str> = kinds
        .iter()
        .filter_map(|kind| match kind {
            AgentEventKind::MessageDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, ["Hello", " back", "thinking"]);
    let thinking = kinds.iter().any(|kind| {
        matches!(
            kind,
            AgentEventKind::MessageDelta {
                stream: MessageStream::Thinking,
                ..
            }
        )
    });
    assert!(thinking, "thought chunks map to the thinking stream");
    // Open streams close before the run's terminal fact.
    let completed = kinds
        .iter()
        .filter(|kind| matches!(kind, AgentEventKind::MessageCompleted { .. }))
        .count();
    assert_eq!(completed, 2, "both open streams close at turn end");
    assert!(matches!(
        kinds.last(),
        Some(AgentEventKind::RunCompleted {
            outcome: RunOutcome::Success,
            ..
        })
    ));
    assert!(matches!(
        recv(&events),
        AgentEventKind::SessionStatus {
            status: SessionStatus::Idle
        }
    ));
    adapter.stop(handle).expect("stop");
    script.join().expect("the agent script completes");
}

#[test]
fn tool_calls_report_started_then_completed() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent.handshake("sess-1").await;
        let prompt = agent.next_method("session/prompt").await;
        agent
            .notify_update(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-1",
                "title": "List files",
                "kind": "execute",
                "status": "in_progress",
            }))
            .await;
        agent
            .notify_update(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "status": "completed",
                "rawOutput": {"stdout": "a.txt"},
            }))
            .await;
        agent
            .respond(&prompt, json!({"stopReason": "end_turn"}))
            .await;
        agent.park().await;
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::WorkspaceWrite))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.send(&handle, input("list files")).expect("send");
    let kinds = through_run_completed(&events);
    let started = kinds.iter().find_map(|kind| match kind {
        AgentEventKind::ToolStarted { tool, summary, .. } => Some((tool, summary)),
        _ => None,
    });
    assert_eq!(
        started.map(|(tool, summary)| (tool.as_str(), summary.as_str())),
        Some(("execute", "List files"))
    );
    let completed = kinds.iter().find_map(|kind| match kind {
        AgentEventKind::ToolCompleted {
            tool, ok, output, ..
        } => Some((tool.clone(), *ok, output.clone())),
        _ => None,
    });
    assert_eq!(
        completed.map(|(tool, ok, output)| (tool, ok, output.unwrap_or_default())),
        Some((
            "execute".to_owned(),
            true,
            "{\"stdout\":\"a.txt\"}".to_owned()
        ))
    );
    adapter.stop(handle).expect("stop");
    script.join().expect("the agent script completes");
}

#[test]
fn a_plan_update_becomes_todos() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent.handshake("sess-1").await;
        let prompt = agent.next_method("session/prompt").await;
        agent
            .notify_update(json!({
                "sessionUpdate": "plan",
                "entries": [
                    {"content": "first", "status": "completed"},
                    {"content": "second", "status": "in_progress"},
                    {"content": "third", "status": "pending"},
                ],
            }))
            .await;
        agent
            .respond(&prompt, json!({"stopReason": "end_turn"}))
            .await;
        agent.park().await;
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::WorkspaceWrite))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.send(&handle, input("plan")).expect("send");
    let kinds = through_run_completed(&events);
    let items = kinds.iter().find_map(|kind| match kind {
        AgentEventKind::TodosUpdated { items, .. } => Some(items.clone()),
        _ => None,
    });
    let items = items.expect("a plan update produces todos");
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].content, "first");
    adapter.stop(handle).expect("stop");
    script.join().expect("the agent script completes");
}

#[test]
fn usage_updates_attach_to_run_completed() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent.handshake("sess-1").await;
        let prompt = agent.next_method("session/prompt").await;
        agent
            .notify_update(json!({
                "sessionUpdate": "usage_update",
                "used": 4321,
                "size": 200_000,
            }))
            .await;
        agent
            .respond(&prompt, json!({"stopReason": "end_turn"}))
            .await;
        agent.park().await;
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::WorkspaceWrite))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.send(&handle, input("go")).expect("send");
    let kinds = through_run_completed(&events);
    let usage = kinds.iter().find_map(|kind| match kind {
        AgentEventKind::RunCompleted { usage, .. } => usage.clone(),
        _ => None,
    });
    assert_eq!(
        usage.and_then(|usage| usage.input_tokens),
        Some(4321),
        "the reported token usage lands on run.completed"
    );
    adapter.stop(handle).expect("stop");
    script.join().expect("the agent script completes");
}

#[test]
fn cancel_sends_session_cancel_and_the_run_completes_cancelled() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent.handshake("sess-1").await;
        let prompt = agent.next_method("session/prompt").await;
        let cancel = agent.next_method("session/cancel").await;
        assert_eq!(cancel["params"]["sessionId"], "sess-1");
        agent
            .respond(&prompt, json!({"stopReason": "cancelled"}))
            .await;
        agent.park().await;
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::WorkspaceWrite))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    let run = adapter.send(&handle, input("stop me")).expect("send");
    adapter.cancel(&handle, run).expect("cancel lands");
    let kinds = through_idle(&events);
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::RunCompleted {
            outcome: RunOutcome::Cancelled,
            ..
        }
    )));
    adapter.stop(handle).expect("stop");
    script.join().expect("the agent script completes");
}

#[test]
fn a_prompt_error_fails_the_run() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent.handshake("sess-1").await;
        let prompt = agent.next_method("session/prompt").await;
        agent
            .write(json!({
                "id": prompt["id"],
                "error": {"code": -32000, "message": "the prompt exploded"},
            }))
            .await;
        agent.park().await;
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::WorkspaceWrite))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.send(&handle, input("boom")).expect("send");
    let kinds = through_idle(&events);
    assert!(
        kinds
            .iter()
            .any(|kind| matches!(kind, AgentEventKind::Error { .. }))
    );
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::RunCompleted {
            outcome: RunOutcome::Failed,
            ..
        }
    )));
    adapter.stop(handle).expect("stop");
    script.join().expect("the agent script completes");
}

#[test]
fn a_second_send_while_a_run_is_in_flight_is_rejected() {
    let (adapter, agents) = duplex_adapter();
    let (release, wait_release) = mpsc::channel::<()>();
    let script = serve(agents, |mut agent| async move {
        agent.handshake("sess-1").await;
        let prompt = agent.next_method("session/prompt").await;
        // Hold the prompt open until the second send has been tried.
        let _ = wait_release.recv();
        agent
            .respond(&prompt, json!({"stopReason": "end_turn"}))
            .await;
        agent.park().await;
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::WorkspaceWrite))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.send(&handle, input("first")).expect("send");
    let error = adapter
        .send(&handle, input("second"))
        .expect_err("one run per session at a time");
    assert_eq!(error.code, ReceiptCode::InvalidState);
    let _ = release.send(());
    let _ = through_idle(&events);
    adapter.stop(handle).expect("stop");
    script.join().expect("the agent script completes");
}

#[test]
fn fs_read_serves_a_file_inside_the_workspace() {
    let workspace = temp_workspace("fs-read");
    std::fs::write(workspace.join("note.txt"), "hello\nworld\n").expect("write");
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent.handshake("sess-1").await;
        agent
            .write(json!({
                "id": "fs-1",
                "method": "fs/read_text_file",
                "params": {"sessionId": "sess-1", "path": "note.txt"},
            }))
            .await;
        let reply = agent.next_client().await;
        assert_eq!(reply["id"], "fs-1");
        assert_eq!(reply["result"]["content"], "hello\nworld\n");
        // The agent ends; the session reports the transport loss.
    });
    let handle = adapter
        .start(
            &integration(),
            options_at(workspace.clone(), AccessMode::WorkspaceWrite),
        )
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    let _ = through_closed(&events);
    adapter.stop(handle).expect("stop");
    let _ = std::fs::remove_dir_all(&workspace);
    script.join().expect("the agent script completes");
}

#[test]
fn fs_write_is_denied_on_a_read_only_session() {
    let workspace = temp_workspace("fs-ro");
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent.handshake("sess-1").await;
        agent
            .write(json!({
                "id": "fs-2",
                "method": "fs/write_text_file",
                "params": {
                    "sessionId": "sess-1",
                    "path": "new.txt",
                    "content": "data",
                },
            }))
            .await;
        let reply = agent.next_client().await;
        assert_eq!(reply["id"], "fs-2");
        assert_eq!(reply["error"]["code"], -32000);
    });
    let handle = adapter
        .start(
            &integration(),
            options_at(workspace.clone(), AccessMode::ReadOnly),
        )
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    let _ = through_closed(&events);
    adapter.stop(handle).expect("stop");
    assert!(
        !workspace.join("new.txt").exists(),
        "a read-only session writes nothing"
    );
    let _ = std::fs::remove_dir_all(&workspace);
    script.join().expect("the agent script completes");
}

#[test]
fn fs_requests_outside_the_workspace_are_denied() {
    let workspace = temp_workspace("fs-escape");
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent.handshake("sess-1").await;
        for (id, method, path) in [
            ("r-1", "fs/read_text_file", "../escape.txt"),
            ("w-1", "fs/write_text_file", "/etc/aifuel-should-not-write"),
        ] {
            agent
                .write(json!({
                    "id": id,
                    "method": method,
                    "params": {
                        "sessionId": "sess-1",
                        "path": path,
                        "content": "x",
                    },
                }))
                .await;
            let reply = agent.next_client().await;
            assert_eq!(reply["id"].as_str(), Some(id));
            assert_eq!(
                reply["error"]["code"].as_i64(),
                Some(-32000),
                "{method} outside the workspace is refused"
            );
        }
    });
    let handle = adapter
        .start(
            &integration(),
            options_at(workspace.clone(), AccessMode::WorkspaceWrite),
        )
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    let _ = through_closed(&events);
    adapter.stop(handle).expect("stop");
    let _ = std::fs::remove_dir_all(&workspace);
    script.join().expect("the agent script completes");
}

#[test]
fn the_agent_dying_mid_run_fails_the_run_and_interrupts_the_session() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent.handshake("sess-1").await;
        let _prompt = agent.next_method("session/prompt").await;
        // The agent dies without answering: the run cannot guess an
        // outcome, so it fails and the session reports interrupted.
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::WorkspaceWrite))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.send(&handle, input("go")).expect("send");
    let kinds = through_closed(&events);
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::RunCompleted {
            outcome: RunOutcome::Failed,
            ..
        }
    )));
    let interrupted = kinds.iter().position(|kind| {
        matches!(
            kind,
            AgentEventKind::SessionStatus {
                status: SessionStatus::Interrupted
            }
        )
    });
    let closed = kinds
        .iter()
        .position(|kind| matches!(kind, AgentEventKind::SessionClosed { .. }));
    assert!(
        interrupted.is_some() && interrupted < closed,
        "interrupted lands before session.closed: {kinds:?}"
    );
    adapter.stop(handle).expect("stop");
    script.join().expect("the agent script completes");
}

#[test]
fn invalid_json_from_the_agent_fails_the_session() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent.handshake("sess-1").await;
        agent
            .writer
            .write_all(b"this is not json\n")
            .await
            .expect("write");
        agent.park().await;
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::WorkspaceWrite))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    let kinds = through_closed(&events);
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::SessionStatus {
            status: SessionStatus::Interrupted
        }
    )));
    adapter.stop(handle).expect("stop");
    script.join().expect("the agent script completes");
}
