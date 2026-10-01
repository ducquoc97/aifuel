//! Approval request round-trips: option shaping per access mode, the
//! answer write-back on the provider's request id, and `already_resolved`
//! semantics for late and duplicate answers.

use super::*;
use aifuel_core::{
    AccessMode, AgentAdapter, AgentEventKind, ApprovalDecision, ApprovalKind, ReceiptCode,
    RequestId, RunOutcome, SessionStatus,
};

/// The scripted request ids the fake server uses, distinct from client
/// rpc ids so cross-talk is obvious.
const SERVER_REQUEST_ID: u64 = 77;

fn command_approval(turn_id: &str) -> Value {
    json!({
        "id": SERVER_REQUEST_ID,
        "method": "item/commandExecution/requestApproval",
        "params": {
            "threadId": "thread-1",
            "turnId": turn_id,
            "itemId": "item-1",
            "reason": "needs to write output.txt",
        }
    })
}

/// Find the `approval.requested` event's request id.
fn requested_id(kinds: &[AgentEventKind]) -> RequestId {
    kinds
        .iter()
        .find_map(|kind| match kind {
            AgentEventKind::ApprovalRequested { request_id, .. } => Some(request_id.clone()),
            _ => None,
        })
        .expect("approval.requested was emitted")
}

#[test]
fn command_approval_round_trips_an_accept() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        server.handshake("thread-1").await;
        let start = server.next_method("turn/start").await;
        server
            .respond(&start, json!({"turn": {"id": "turn-1"}}))
            .await;
        server.write(command_approval("turn-1")).await;
        // The answer arrives back on the server request's own id.
        let answer = server.next_client().await;
        server
            .write(json!({
                "method": "turn/completed",
                "params": {"threadId": "thread-1", "turn": {"id": "turn-1", "status": "completed", "error": null}}
            }))
            .await;
        server.park().await;
        answer
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::WorkspaceWrite))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.send(&handle, input("build it")).expect("send");

    let mut kinds = Vec::new();
    let request_id = loop {
        let kind = recv(&events);
        let found = matches!(kind, AgentEventKind::ApprovalRequested { .. });
        kinds.push(kind);
        if found {
            break requested_id(&kinds);
        }
    };
    let AgentEventKind::ApprovalRequested { request, .. } = kinds.last().expect("requested") else {
        unreachable!()
    };
    assert_eq!(request.kind, ApprovalKind::ToolPermission);
    assert_eq!(request.title, "needs to write output.txt");
    // Workspace-write offers the full option set.
    assert_eq!(
        request
            .options
            .iter()
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        vec!["accept", "decline", "cancel"]
    );
    assert!(matches!(
        kinds[..kinds.len() - 1].last(),
        Some(AgentEventKind::SessionStatus {
            status: SessionStatus::WaitingApproval
        })
    ));

    adapter
        .answer(
            &handle,
            request_id.clone(),
            ApprovalDecision::OptionId("accept".to_owned()),
        )
        .expect("answer");
    let kinds = through_idle(&events);
    // A second answer loses to the first.
    let error = adapter
        .answer(
            &handle,
            request_id.clone(),
            ApprovalDecision::OptionId("decline".to_owned()),
        )
        .expect_err("the request already resolved");
    assert_eq!(error.code, ReceiptCode::AlreadyResolved);
    adapter.stop(handle).expect("stop");
    let answer = server.join().expect("server script finished");
    assert_eq!(answer["id"], SERVER_REQUEST_ID);
    assert_eq!(answer["result"]["decision"], "accept");
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::ApprovalResolved {
            request_id: resolved_id,
            decision: ApprovalDecision::OptionId(option),
            ..
        } if resolved_id == &request_id && option == "accept"
    )));
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::RunCompleted {
            outcome: RunOutcome::Success,
            ..
        }
    )));
}

#[test]
fn read_only_never_offers_accept() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        server.handshake("thread-1").await;
        let start = server.next_method("turn/start").await;
        server
            .respond(&start, json!({"turn": {"id": "turn-1"}}))
            .await;
        server.write(command_approval("turn-1")).await;
        let answer = server.next_client().await;
        server
            .write(json!({
                "method": "turn/completed",
                "params": {"threadId": "thread-1", "turn": {"id": "turn-1", "status": "completed", "error": null}}
            }))
            .await;
        server.park().await;
        answer
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::ReadOnly))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.send(&handle, input("try")).expect("send");

    let request_id = loop {
        if let AgentEventKind::ApprovalRequested {
            request,
            request_id,
            ..
        } = recv(&events)
        {
            assert_eq!(
                request
                    .options
                    .iter()
                    .map(|option| option.id.as_str())
                    .collect::<Vec<_>>(),
                vec!["decline", "cancel"],
                "read-only sessions cannot accept permission asks"
            );
            break request_id;
        }
    };
    // An option that was never offered is rejected before it writes.
    let error = adapter
        .answer(
            &handle,
            request_id.clone(),
            ApprovalDecision::OptionId("accept".to_owned()),
        )
        .expect_err("accept was not offered");
    assert_eq!(error.code, ReceiptCode::InvalidState);
    adapter
        .answer(
            &handle,
            request_id,
            ApprovalDecision::OptionId("decline".to_owned()),
        )
        .expect("decline");
    through_idle(&events);
    adapter.stop(handle).expect("stop");
    let answer = server.join().expect("server script finished");
    assert_eq!(answer["result"]["decision"], "decline");
}

#[test]
fn free_text_requests_take_text_answers() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        server.handshake("thread-1").await;
        let start = server.next_method("turn/start").await;
        server
            .respond(&start, json!({"turn": {"id": "turn-1"}}))
            .await;
        server
            .write(json!({
                "id": SERVER_REQUEST_ID,
                "method": "item/tool/requestUserInput",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "itemId": "item-1",
                    "questions": [{"id": "branch", "question": "Which branch?"}],
                }
            }))
            .await;
        let answer = server.next_client().await;
        server
            .write(json!({
                "method": "turn/completed",
                "params": {"threadId": "thread-1", "turn": {"id": "turn-1", "status": "completed", "error": null}}
            }))
            .await;
        server.park().await;
        answer
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::Full))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.send(&handle, input("go")).expect("send");
    let request_id = loop {
        if let AgentEventKind::ApprovalRequested {
            request,
            request_id,
            ..
        } = recv(&events)
        {
            assert_eq!(request.kind, ApprovalKind::Question);
            break request_id;
        }
    };
    adapter
        .answer(
            &handle,
            request_id,
            ApprovalDecision::Text("main".to_owned()),
        )
        .expect("text answer");
    through_idle(&events);
    adapter.stop(handle).expect("stop");
    let answer = server.join().expect("server script finished");
    assert_eq!(answer["id"], SERVER_REQUEST_ID);
    assert_eq!(answer["result"]["answers"]["branch"]["answers"][0], "main");
}

/// A multi-question `requestUserInput` takes a declared answers map and
/// writes it back verbatim - the shape free text cannot express.
#[test]
fn request_user_input_takes_the_declared_answers_map() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        server.handshake("thread-1").await;
        let start = server.next_method("turn/start").await;
        server
            .respond(&start, json!({"turn": {"id": "turn-1"}}))
            .await;
        server
            .write(json!({
                "id": SERVER_REQUEST_ID,
                "method": "item/tool/requestUserInput",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "itemId": "item-1",
                    "questions": [
                        {"id": "branch", "question": "Which branch?"},
                        {"id": "target", "question": "Which target?"},
                    ],
                }
            }))
            .await;
        let answer = server.next_client().await;
        server
            .write(json!({
                "method": "turn/completed",
                "params": {"threadId": "thread-1", "turn": {"id": "turn-1", "status": "completed", "error": null}}
            }))
            .await;
        server.park().await;
        answer
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::Full))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.send(&handle, input("go")).expect("send");
    let request_id = loop {
        if let AgentEventKind::ApprovalRequested {
            request,
            request_id,
            ..
        } = recv(&events)
        {
            assert_eq!(request.kind, ApprovalKind::Question);
            assert_eq!(
                request
                    .questions
                    .iter()
                    .map(|question| question.id.as_str())
                    .collect::<Vec<_>>(),
                vec!["branch", "target"],
                "the declared questions reach the host"
            );
            break request_id;
        }
    };
    adapter
        .answer(
            &handle,
            request_id,
            ApprovalDecision::Answers(BTreeMap::from([
                ("branch".to_owned(), vec!["main".to_owned()]),
                ("target".to_owned(), vec!["workspace".to_owned()]),
            ])),
        )
        .expect("the answers map resolves the ask");
    through_idle(&events);
    adapter.stop(handle).expect("stop");
    let answer = server.join().expect("server script finished");
    assert_eq!(answer["id"], SERVER_REQUEST_ID);
    assert_eq!(answer["result"]["answers"]["branch"]["answers"][0], "main");
    assert_eq!(
        answer["result"]["answers"]["target"]["answers"][0],
        "workspace"
    );
}

/// An `elicitation` decision returns the content JSON verbatim, the
/// shape the server's `requestedSchema` asked for.
#[test]
fn elicitation_returns_the_content_verbatim() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        server.handshake("thread-1").await;
        let start = server.next_method("turn/start").await;
        server
            .respond(&start, json!({"turn": {"id": "turn-1"}}))
            .await;
        server
            .write(json!({
                "id": SERVER_REQUEST_ID,
                "method": "mcpServer/elicitation/request",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "serverName": "docs",
                    "requestId": "elic-1",
                    "message": "pick a mode",
                    "requestedSchema": {"type": "object"},
                }
            }))
            .await;
        let answer = server.next_client().await;
        server
            .write(json!({
                "method": "turn/completed",
                "params": {"threadId": "thread-1", "turn": {"id": "turn-1", "status": "completed", "error": null}}
            }))
            .await;
        server.park().await;
        answer
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::Full))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.send(&handle, input("go")).expect("send");
    let request_id = loop {
        if let AgentEventKind::ApprovalRequested {
            request,
            request_id,
            ..
        } = recv(&events)
        {
            assert_eq!(request.kind, ApprovalKind::McpElicitation);
            break request_id;
        }
    };
    adapter
        .answer(
            &handle,
            request_id,
            ApprovalDecision::Elicitation(json!({"mode": "fast", "limit": 3})),
        )
        .expect("the elicitation content resolves the ask");
    through_idle(&events);
    adapter.stop(handle).expect("stop");
    let answer = server.join().expect("server script finished");
    assert_eq!(answer["id"], SERVER_REQUEST_ID);
    assert_eq!(answer["result"]["action"], "accept");
    assert_eq!(
        answer["result"]["content"],
        json!({"mode": "fast", "limit": 3}),
        "the elicitation content returns verbatim"
    );
}

/// An elicitation ask declares no question ids, so every answers key is
/// unasked: `answers` cannot smuggle content the server never requested.
#[test]
fn answers_for_an_ask_without_questions_are_rejected() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        server.handshake("thread-1").await;
        let start = server.next_method("turn/start").await;
        server
            .respond(&start, json!({"turn": {"id": "turn-1"}}))
            .await;
        server
            .write(json!({
                "id": SERVER_REQUEST_ID,
                "method": "mcpServer/elicitation/request",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "serverName": "docs",
                    "requestId": "elic-1",
                    "message": "pick a mode",
                    "requestedSchema": {"type": "object"},
                }
            }))
            .await;
        let answer = server.next_client().await;
        server
            .write(json!({
                "method": "turn/completed",
                "params": {"threadId": "thread-1", "turn": {"id": "turn-1", "status": "completed", "error": null}}
            }))
            .await;
        server.park().await;
        answer
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::Full))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.send(&handle, input("go")).expect("send");
    let request_id = loop {
        if let AgentEventKind::ApprovalRequested { request_id, .. } = recv(&events) {
            break request_id;
        }
    };
    let error = adapter
        .answer(
            &handle,
            request_id.clone(),
            ApprovalDecision::Answers(BTreeMap::from([(
                "mode".to_owned(),
                vec!["fast".to_owned()],
            )])),
        )
        .expect_err("answers are unasked when the ask declares no questions");
    assert_eq!(error.code, ReceiptCode::InvalidState);
    // The ask is still pending: the right decision kind resolves it.
    adapter
        .answer(
            &handle,
            request_id,
            ApprovalDecision::Elicitation(json!({"mode": "fast"})),
        )
        .expect("the elicitation content resolves the ask");
    through_idle(&events);
    adapter.stop(handle).expect("stop");
    let answer = server.join().expect("server script finished");
    assert_eq!(answer["result"]["content"], json!({"mode": "fast"}));
}

/// An answers map naming a question the ask never declared is rejected
/// rather than forwarded to the provider.
#[test]
fn answers_for_questions_never_asked_are_rejected() {
    let (adapter, servers) = duplex_adapter();
    let server = serve(servers, |mut server| async move {
        server.handshake("thread-1").await;
        let start = server.next_method("turn/start").await;
        server
            .respond(&start, json!({"turn": {"id": "turn-1"}}))
            .await;
        server
            .write(json!({
                "id": SERVER_REQUEST_ID,
                "method": "item/tool/requestUserInput",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "itemId": "item-1",
                    "questions": [{"id": "branch", "question": "Which branch?"}],
                }
            }))
            .await;
        let answer = server.next_client().await;
        server
            .write(json!({
                "method": "turn/completed",
                "params": {"threadId": "thread-1", "turn": {"id": "turn-1", "status": "completed", "error": null}}
            }))
            .await;
        server.park().await;
        answer
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::Full))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.send(&handle, input("go")).expect("send");
    let request_id = loop {
        if let AgentEventKind::ApprovalRequested { request_id, .. } = recv(&events) {
            break request_id;
        }
    };
    let error = adapter
        .answer(
            &handle,
            request_id.clone(),
            ApprovalDecision::Answers(BTreeMap::from([(
                "unasked".to_owned(),
                vec!["sneaky".to_owned()],
            )])),
        )
        .expect_err("answers for unasked questions are rejected");
    assert_eq!(error.code, ReceiptCode::InvalidState);
    // The ask is still pending: a declared answer still resolves it.
    adapter
        .answer(
            &handle,
            request_id,
            ApprovalDecision::Answers(BTreeMap::from([(
                "branch".to_owned(),
                vec!["main".to_owned()],
            )])),
        )
        .expect("the declared answer resolves the ask");
    through_idle(&events);
    adapter.stop(handle).expect("stop");
    let answer = server.join().expect("server script finished");
    assert_eq!(answer["result"]["answers"]["branch"]["answers"][0], "main");
}

#[test]
fn an_unknown_request_id_is_already_resolved() {
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
    let error = adapter
        .answer(
            &handle,
            RequestId::new("approval-not-real"),
            ApprovalDecision::OptionId("accept".to_owned()),
        )
        .expect_err("no such pending request");
    assert_eq!(error.code, ReceiptCode::AlreadyResolved);
    adapter.stop(handle).expect("stop");
    server.join().expect("server script finished");
}
