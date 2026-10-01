//! Session and turn lifecycle against the scripted app-server:
//! handshake parameters, run event ordering, cancellation, resume
//! cursors, attachments, and teardown.

use super::*;
use aifuel_core::{
    AccessMode, AgentAdapter, AgentEventKind, Attachment, AttachmentKind, ModelSelection,
    ReceiptCode, RunOutcome, SessionStatus, TodoItemStatus,
};

#[test]
fn thread_start_carries_access_cwd_and_model() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        let start = server.handshake("thread-1").await;
        // No external tools were selected, so setup must not poll MCP
        // readiness; silence on the wire proves it.
        let leaked =
            tokio::time::timeout(Duration::from_millis(400), server.next_client_opt()).await;
        assert!(
            matches!(leaked, Err(_) | Ok(None)),
            "an empty tool selection never polls mcpServerStatus/list: {leaked:?}"
        );
        server.park().await;
        start
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::ReadOnly))
        .expect("start");
    assert_eq!(handle.provider_session.as_deref(), Some("thread-1"));
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.stop(handle).expect("stop");
    let request = server.join().expect("server script finished");
    let params = &request["params"];
    assert_eq!(request["method"], "thread/start");
    assert_eq!(params["sandbox"], "read-only");
    assert_eq!(params["approvalPolicy"], "on-request");
    assert_eq!(params["model"], "codex-test-model");
    assert_eq!(params["cwd"], "/tmp");
    assert_eq!(params["ephemeral"], false);
    assert_eq!(
        params["config"],
        json!({"mcp_servers": {}}),
        "no tool selection means no managed MCP server"
    );
    assert!(matches!(
        recv(&events),
        AgentEventKind::SessionClosed { .. }
    ));
}

#[test]
fn thread_resume_uses_the_persisted_cursor() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        let start = server.handshake("thread-resumed").await;
        server.park().await;
        start
    });
    let mut options = options(AccessMode::WorkspaceWrite);
    options.resume_cursor = Some("thread-resumed".to_owned());
    let handle = adapter.start(&integration(), options).expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    assert_eq!(handle.provider_session.as_deref(), Some("thread-resumed"));
    assert_eq!(
        adapter.resume_cursor(&handle.session_id).as_deref(),
        Some("thread-resumed")
    );
    adapter.stop(handle).expect("stop");
    let request = server.join().expect("server script finished");
    assert_eq!(request["method"], "thread/resume");
    assert_eq!(request["params"]["threadId"], "thread-resumed");
    assert_eq!(request["params"]["sandbox"], "workspace-write");
}

/// A selected tool set spawns the filtered AI Fuel Gateway through the
/// thread config, and the session opens only once `mcpServerStatus/list`
/// reports exactly those tools. A host depending on external tools must
/// never silently run on a partial tool set.
#[test]
fn external_tools_register_the_gateway_and_gate_setup_on_exact_readiness() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        let init = server.next_method("initialize").await;
        server.respond(&init, json!({})).await;
        server.next_method("initialized").await;
        let start = server.next_method("thread/start").await;
        let config = start["params"]["config"].clone();
        server
            .respond(&start, json!({"thread": {"id": "thread-1"}}))
            .await;
        // The gateway is still starting: setup keeps polling inside the
        // setup deadline rather than opening the session early.
        let status = server.next_method("mcpServerStatus/list").await;
        assert_eq!(status["params"]["threadId"], "thread-1");
        assert_eq!(status["params"]["detail"], "full");
        server
            .respond(
                &status,
                json!({"data": [{"name": "aifuel-gateway", "runtimeStatus": "starting", "tools": {}}]}),
            )
            .await;
        // Connected with exactly the selected tools: setup completes.
        let status = server.next_method("mcpServerStatus/list").await;
        server
            .respond(
                &status,
                json!({"data": [{"name": "aifuel-gateway", "runtimeStatus": "connected", "tools": {
                    "docs__search": {"name": "docs__search"},
                    "repo__read": {"name": "repo__read"},
                }}]}),
            )
            .await;
        server.park().await;
        config
    });
    let mut options = options(AccessMode::ReadOnly);
    options.external_tools = vec!["docs__search".to_owned(), "repo__read".to_owned()];
    let handle = adapter.start(&integration(), options).expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.stop(handle).expect("stop");
    let config = server.join().expect("server script finished");
    assert_eq!(
        config["mcp_servers"]["aifuel-gateway"]["args"],
        json!([
            "mcp",
            "gateway",
            "--agent",
            "codex",
            "--tool",
            "docs__search",
            "--tool",
            "repo__read"
        ]),
        "the thread config launches the gateway filtered to the exact selection"
    );
}

/// The same config and readiness gate applies to `thread/resume`: a
/// resumed session is also forbidden from running without its tools.
#[test]
fn external_tools_gate_thread_resume_the_same_way() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        let init = server.next_method("initialize").await;
        server.respond(&init, json!({})).await;
        server.next_method("initialized").await;
        let resume = server.next_method("thread/resume").await;
        let config = resume["params"]["config"].clone();
        server
            .respond(&resume, json!({"thread": {"id": "thread-9"}}))
            .await;
        let status = server.next_method("mcpServerStatus/list").await;
        assert_eq!(status["params"]["threadId"], "thread-9");
        server
            .respond(
                &status,
                json!({"data": [{"name": "aifuel-gateway", "runtimeStatus": "connected", "tools": {
                    "docs__search": {"name": "docs__search"},
                }}]}),
            )
            .await;
        server.park().await;
        config
    });
    let mut options = options(AccessMode::WorkspaceWrite);
    options.resume_cursor = Some("thread-9".to_owned());
    options.external_tools = vec!["docs__search".to_owned()];
    let handle = adapter.start(&integration(), options).expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.stop(handle).expect("stop");
    let config = server.join().expect("server script finished");
    assert_eq!(
        config["mcp_servers"]["aifuel-gateway"]["args"],
        json!([
            "mcp",
            "gateway",
            "--agent",
            "codex",
            "--tool",
            "docs__search"
        ]),
        "the resume config launches the same filtered gateway"
    );
}

/// The gateway reporting a tool the host never selected fails `start`:
/// widening the tool set is as unacceptable as a missing tool.
#[test]
fn a_session_never_runs_on_a_tool_set_wider_than_selected() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        server.handshake("thread-1").await;
        let status = server.next_method("mcpServerStatus/list").await;
        server
            .respond(
                &status,
                json!({"data": [{"name": "aifuel-gateway", "runtimeStatus": "connected", "tools": {
                    "docs__search": {"name": "docs__search"},
                    "extra__tool": {"name": "extra__tool"},
                }}]}),
            )
            .await;
    });
    let mut options = options(AccessMode::ReadOnly);
    options.external_tools = vec!["docs__search".to_owned()];
    let error = adapter
        .start(&integration(), options)
        .expect_err("a wider tool set fails the session");
    assert_eq!(error.code, ReceiptCode::ProviderError);
    server.join().expect("server script finished");
}

#[test]
fn turn_lifecycle_events_follow_the_run_causal_order() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        server.handshake("thread-1").await;
        let start = server.next_method("turn/start").await;
        let turn_id = "turn-1";
        server
            .respond(&start, json!({"turn": {"id": turn_id}}))
            .await;
        for (method, params) in [
            (
                "item/agentMessage/delta",
                json!({"itemId":"m1","turnId":turn_id,"delta":"Hello"}),
            ),
            (
                "item/reasoning/textDelta",
                json!({"itemId":"r1","turnId":turn_id,"delta":"thinking"}),
            ),
            (
                "turn/plan/updated",
                json!({"turnId":turn_id,"plan":[{"step":"inspect","status":"completed"},{"step":"act","status":"inProgress"}]}),
            ),
            (
                "item/started",
                json!({"turnId":turn_id,"item":{"id":"c1","type":"commandExecution","command":"ls","status":"inProgress"}}),
            ),
            (
                "item/completed",
                json!({"turnId":turn_id,"item":{"id":"c1","type":"commandExecution","status":"completed","aggregatedOutput":"files"}}),
            ),
            (
                "item/agentMessage/delta",
                json!({"itemId":"m1","turnId":turn_id,"delta":" world"}),
            ),
            (
                "item/completed",
                json!({"turnId":turn_id,"item":{"id":"r1","type":"reasoning","text":""}}),
            ),
            (
                "item/completed",
                json!({"turnId":turn_id,"item":{"id":"m1","type":"agentMessage","text":"Hello world"}}),
            ),
            (
                "thread/tokenUsage/updated",
                json!({"threadId":"thread-1","turnId":turn_id,"tokenUsage":{"last":{"cachedInputTokens":0,"inputTokens":12,"outputTokens":7,"reasoningOutputTokens":3,"totalTokens":19},"total":{"cachedInputTokens":0,"inputTokens":12,"outputTokens":7,"reasoningOutputTokens":3,"totalTokens":19}}}),
            ),
        ] {
            server
                .write(json!({"method": method, "params": params}))
                .await;
        }
        server
            .write(json!({
                "method": "turn/completed",
                "params": {"threadId": "thread-1", "turn": {"id": turn_id, "status": "completed", "error": null}}
            }))
            .await;
        server.park().await;
        start
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::ReadOnly))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    let run = adapter.send(&handle, input("say hi")).expect("send");
    assert_eq!(run.as_str(), "turn-1");
    let kinds = through_idle(&events);
    adapter.stop(handle).expect("stop");
    let turn_request = server.join().expect("server script finished");
    assert_eq!(turn_request["params"]["sandboxPolicy"]["type"], "readOnly");
    assert_eq!(
        turn_request["params"]["input"][0],
        json!({"type": "text", "text": "say hi"})
    );
    assert_eq!(turn_request["params"]["model"], "codex-test-model");

    let shapes = kinds
        .iter()
        .map(|kind| match kind {
            AgentEventKind::SessionStatus { status } => format!("status:{:?}", status),
            AgentEventKind::RunStarted { .. } => "run.started".to_owned(),
            AgentEventKind::MessageDelta { stream, text, .. } => {
                format!("delta:{stream:?}:{text:?}")
            }
            AgentEventKind::MessageCompleted { stream, .. } => format!("completed:{stream:?}"),
            AgentEventKind::ToolStarted { tool, summary, .. } => {
                format!("tool.started:{tool}:{summary:?}")
            }
            AgentEventKind::ToolCompleted { tool, ok, .. } => format!("tool.completed:{tool}:{ok}"),
            AgentEventKind::TodosUpdated { items, .. } => format!("todos:{}", items.len()),
            AgentEventKind::RunCompleted { outcome, .. } => format!("run.completed:{:?}", outcome),
            _ => format!("{kind:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        shapes,
        vec![
            "run.started",
            "status:Working",
            "delta:Assistant:\"Hello\"",
            "delta:Thinking:\"thinking\"",
            "todos:2",
            "tool.started:command_execution:\"ls\"",
            "tool.completed:command_execution:true",
            "delta:Assistant:\" world\"",
            "completed:Thinking",
            "completed:Assistant",
            "run.completed:Success",
            "status:Idle",
        ],
        "event ordering follows the run's causal order"
    );
    let run_completed = kinds
        .iter()
        .find(|kind| matches!(kind, AgentEventKind::RunCompleted { .. }))
        .expect("run.completed");
    let AgentEventKind::RunCompleted { usage, .. } = run_completed else {
        unreachable!()
    };
    assert_eq!(
        usage
            .as_ref()
            .map(|usage| (usage.input_tokens, usage.output_tokens)),
        Some((Some(12), Some(7)))
    );
    assert!(matches!(
        &kinds[4],
        AgentEventKind::TodosUpdated { items, .. }
            if items[0].status == TodoItemStatus::Completed
                && items[1].status == TodoItemStatus::InProgress
    ));
}

#[test]
fn a_second_send_fails_while_a_run_is_in_flight() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        server.handshake("thread-1").await;
        let start = server.next_method("turn/start").await;
        server
            .respond(&start, json!({"turn": {"id": "turn-1"}}))
            .await;
        server.park().await;
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::Full))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    let run = adapter.send(&handle, input("one")).expect("send");
    assert_eq!(run.as_str(), "turn-1");
    let error = adapter
        .send(&handle, input("two"))
        .expect_err("a second concurrent run is rejected");
    assert_eq!(error.code, ReceiptCode::InvalidState);
    adapter.stop(handle).expect("stop");
    server.join().expect("server script finished");
}

#[test]
fn cancel_interrupts_the_in_flight_turn() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        server.handshake("thread-1").await;
        let start = server.next_method("turn/start").await;
        server
            .respond(&start, json!({"turn": {"id": "turn-1"}}))
            .await;
        let interrupt = server.next_method("turn/interrupt").await;
        assert_eq!(interrupt["params"]["threadId"], "thread-1");
        assert_eq!(interrupt["params"]["turnId"], "turn-1");
        server.respond(&interrupt, json!({})).await;
        server
            .write(json!({
                "method": "turn/completed",
                "params": {"threadId": "thread-1", "turn": {"id": "turn-1", "status": "interrupted", "error": null}}
            }))
            .await;
        server.park().await;
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::Full))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    let run = adapter.send(&handle, input("work")).expect("send");
    adapter.cancel(&handle, run.clone()).expect("cancel");
    let kinds = through_idle(&events);
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::RunCompleted {
            outcome: RunOutcome::Cancelled,
            ..
        }
    )));
    // Cancelling again is rejected: the run is no longer in flight.
    let error = adapter.cancel(&handle, run).expect_err("the run finished");
    assert_eq!(error.code, ReceiptCode::InvalidState);
    adapter.stop(handle).expect("stop");
    server.join().expect("server script finished");
}

#[test]
fn image_attachments_become_local_image_inputs_and_files_are_unsupported() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        server.handshake("thread-1").await;
        let start = server.next_method("turn/start").await;
        let items = start["params"]["input"].clone();
        server
            .respond(&start, json!({"turn": {"id": "turn-1"}}))
            .await;
        server
            .write(json!({
                "method": "turn/completed",
                "params": {"threadId": "thread-1", "turn": {"id": "turn-1", "status": "completed", "error": null}}
            }))
            .await;
        server.park().await;
        items
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::Full))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    let error = adapter
        .send(
            &handle,
            UserInput {
                text: String::new(),
                attachments: vec![Attachment {
                    kind: AttachmentKind::File,
                    path: PathBuf::from("/tmp/report.pdf"),
                }],
            },
        )
        .expect_err("file attachments have no carrier");
    assert_eq!(error.code, ReceiptCode::Unsupported);
    adapter
        .send(
            &handle,
            UserInput {
                text: String::new(),
                attachments: Vec::new(),
            },
        )
        .expect_err("empty input is rejected");
    let run = adapter
        .send(
            &handle,
            UserInput {
                text: "look".to_owned(),
                attachments: vec![Attachment {
                    kind: AttachmentKind::Image,
                    path: PathBuf::from("/tmp/shot.png"),
                }],
            },
        )
        .expect("image attachments send");
    assert_eq!(run.as_str(), "turn-1");
    through_idle(&events);
    adapter.stop(handle).expect("stop");
    let items = server.join().expect("server script finished");
    assert_eq!(
        items,
        json!([
            {"type": "text", "text": "look"},
            {"type": "localImage", "path": "/tmp/shot.png"}
        ])
    );
}

#[test]
fn set_selection_updates_the_next_turn_model() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        server.handshake("thread-1").await;
        let start = server.next_method("turn/start").await;
        server
            .respond(&start, json!({"turn": {"id": "turn-1"}}))
            .await;
        server
            .write(json!({
                "method": "turn/completed",
                "params": {"threadId": "thread-1", "turn": {"id": "turn-1", "status": "completed", "error": null}}
            }))
            .await;
        server.park().await;
        start
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::Full))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    let selection = ModelSelection {
        integration_id: integration().id.clone(),
        model: "other-model".to_owned(),
        effort: None,
    };
    adapter
        .set_selection(&handle, selection.clone())
        .expect("set_selection");
    adapter.send(&handle, input("go")).expect("send");
    through_idle(&events);
    adapter.stop(handle).expect("stop");
    let start = server.join().expect("server script finished");
    assert_eq!(start["params"]["model"], "other-model");
}

#[test]
fn stop_closes_the_session_and_rejects_late_commands() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        server.handshake("thread-1").await;
        server.park().await;
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::Full))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.stop(handle.clone()).expect("stop");
    assert!(matches!(
        recv(&events),
        AgentEventKind::SessionClosed { .. }
    ));
    let error = adapter
        .send(&handle, input("late"))
        .expect_err("the session is gone");
    assert_eq!(error.code, ReceiptCode::UnknownSession);
    server.join().expect("server script finished");
}

#[test]
fn a_dying_server_fails_the_run_and_closes_the_session() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        server.handshake("thread-1").await;
        let start = server.next_method("turn/start").await;
        server
            .respond(&start, json!({"turn": {"id": "turn-1"}}))
            .await;
        // Dropping the transport closes the driver's output stream.
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::Full))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.send(&handle, input("work")).expect("send");
    server.join().expect("server script finished");
    let kinds = through_closed(&events);
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::Error {
            code: ReceiptCode::ProviderError,
            ..
        }
    )));
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::RunCompleted {
            outcome: RunOutcome::Failed,
            ..
        }
    )));
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::SessionStatus {
            status: SessionStatus::Interrupted
        }
    )));
}

#[test]
fn capabilities_match_what_the_adapter_implements() {
    let adapter = CodexAdapter::new();
    assert_eq!(
        adapter.capabilities(),
        aifuel_core::AdapterCapabilities {
            streaming: true,
            resume: true,
            approvals: true,
            checkpoints: false,
            effort: true,
            images: true,
            todos: true,
            external_tools: true,
        }
    );
}

#[test]
fn unhandled_server_requests_are_rejected_not_approved() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        server.handshake("thread-1").await;
        let start = server.next_method("turn/start").await;
        server
            .respond(&start, json!({"turn": {"id": "turn-1"}}))
            .await;
        server
            .write(json!({"id": 41, "method": "item/tool/call", "params": {"turnId": "turn-1", "tool": "x", "arguments": {}}}))
            .await;
        let reply = server.next_client().await;
        server
            .write(json!({
                "method": "turn/completed",
                "params": {"threadId": "thread-1", "turn": {"id": "turn-1", "status": "completed", "error": null}}
            }))
            .await;
        server.park().await;
        reply
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::Full))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.send(&handle, input("go")).expect("send");
    through_idle(&events);
    adapter.stop(handle).expect("stop");
    let reply = server.join().expect("server script finished");
    assert_eq!(reply["id"], 41);
    assert_eq!(reply["error"]["code"], -32601);
}

#[test]
fn dropping_the_adapter_cancels_the_run_and_closes_the_session() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        server.handshake("thread-1").await;
        let start = server.next_method("turn/start").await;
        server
            .respond(&start, json!({"turn": {"id": "turn-1"}}))
            .await;
        server.park().await;
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::Full))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.send(&handle, input("work")).expect("send");
    drop(adapter);
    let kinds = through_closed(&events);
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::RunCompleted {
            outcome: RunOutcome::Cancelled,
            ..
        }
    )));
    server.join().expect("server script finished");
}

#[test]
fn an_unannounced_turn_fails_start_cleanly() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        let init = server.next_method("initialize").await;
        server.respond(&init, json!({})).await;
        server.next_method("initialized").await;
        let start = server.next_client().await;
        server
            .write(json!({"id": start["id"], "error": {"code": -1, "message": "nope"}}))
            .await;
    });
    let error = adapter
        .start(&integration(), options(AccessMode::Full))
        .expect_err("a rejected thread/start fails the session");
    assert_eq!(error.code, ReceiptCode::ProviderError);
    server.join().expect("server script finished");
}
