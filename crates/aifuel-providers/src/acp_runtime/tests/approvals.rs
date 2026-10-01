//! `session/request_permission` mapping onto the shared approval
//! policy: contract options offered, provider outcomes written back,
//! and the refusals that must never become implicit approvals.

use super::*;
use aifuel_core::{AgentAdapter, ApprovalDecision, ReceiptCode, RequestId, RunOutcome};

/// The `session/request_permission` frame a tool call raises.
fn permission_request(id: &str, options: Value) -> Value {
    json!({
        "id": id,
        "method": "session/request_permission",
        "params": {
            "sessionId": "sess-1",
            "toolCall": {
                "toolCallId": "call-1",
                "title": "Run a command",
                "kind": "execute",
            },
            "options": options,
        },
    })
}

/// The request id and offered option ids out of `approval.requested`,
/// after `session.status` flips to waiting.
fn next_approval(events: &Receiver<AgentEventKind>) -> (RequestId, Vec<String>) {
    for _ in 0..16 {
        match recv(events) {
            AgentEventKind::ApprovalRequested {
                request_id,
                request,
                ..
            } => {
                return (
                    request_id,
                    request
                        .options
                        .iter()
                        .map(|option| option.id.clone())
                        .collect(),
                );
            }
            _ => continue,
        }
    }
    panic!("approval.requested never arrived")
}

#[test]
fn a_permission_request_becomes_an_approval_and_selects_allow_once() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent.handshake("sess-1").await;
        let prompt = agent.next_method("session/prompt").await;
        agent
            .write(permission_request(
                "perm-1",
                json!([
                    {"optionId": "allow-1", "name": "Allow once", "kind": "allow_once"},
                    {"optionId": "deny-1", "name": "Reject", "kind": "reject_once"},
                ]),
            ))
            .await;
        let answer = agent.next_client().await;
        assert_eq!(answer["id"], "perm-1");
        assert_eq!(
            answer["result"]["outcome"],
            json!({"outcome": "selected", "optionId": "allow-1"}),
            "accept selects the agent's allow_once option"
        );
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
    adapter.send(&handle, input("run it")).expect("send");
    let (request, offered) = next_approval(&events);
    assert_eq!(offered, ["accept", "decline", "cancel"]);
    adapter
        .answer(
            &handle,
            request.clone(),
            ApprovalDecision::OptionId("accept".to_owned()),
        )
        .expect("the answer lands");
    // The resolved fact lands inside the run's causal order.
    let mut resolved = false;
    let kinds = through_idle(&events);
    for kind in &kinds {
        if let AgentEventKind::ApprovalResolved {
            request_id,
            decision,
            ..
        } = kind
        {
            resolved = true;
            assert_eq!(*request_id, request);
            assert_eq!(*decision, ApprovalDecision::OptionId("accept".to_owned()));
        }
    }
    assert!(resolved, "approval.resolved arrived: {kinds:?}");
    adapter.stop(handle).expect("stop");
    script.join().expect("the agent script completes");
}

#[test]
fn a_read_only_session_offers_no_accept() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent.handshake("sess-1").await;
        let prompt = agent.next_method("session/prompt").await;
        agent
            .write(permission_request(
                "perm-1",
                json!([
                    {"optionId": "allow-1", "name": "Allow once", "kind": "allow_once"},
                    {"optionId": "deny-1", "name": "Reject", "kind": "reject_once"},
                ]),
            ))
            .await;
        let answer = agent.next_client().await;
        assert_eq!(
            answer["result"]["outcome"],
            json!({"outcome": "selected", "optionId": "deny-1"}),
            "decline selects the reject option"
        );
        agent
            .respond(&prompt, json!({"stopReason": "end_turn"}))
            .await;
        agent.park().await;
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::ReadOnly))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.send(&handle, input("run it")).expect("send");
    let (request, offered) = next_approval(&events);
    assert_eq!(
        offered,
        ["decline", "cancel"],
        "a read-only run never offers accept"
    );
    let error = adapter
        .answer(
            &handle,
            request.clone(),
            ApprovalDecision::OptionId("accept".to_owned()),
        )
        .expect_err("accept was not offered");
    assert_eq!(error.code, ReceiptCode::InvalidState);
    adapter
        .answer(
            &handle,
            request,
            ApprovalDecision::OptionId("decline".to_owned()),
        )
        .expect("decline lands");
    let _ = through_idle(&events);
    adapter.stop(handle).expect("stop");
    script.join().expect("the agent script completes");
}

#[test]
fn allow_always_alone_cannot_answer_accept() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent.handshake("sess-1").await;
        let prompt = agent.next_method("session/prompt").await;
        agent
            .write(permission_request(
                "perm-1",
                json!([
                    {"optionId": "forever", "name": "Always", "kind": "allow_always"},
                    {"optionId": "no", "name": "Reject", "kind": "reject_once"},
                ]),
            ))
            .await;
        let answer = agent.next_client().await;
        assert_eq!(
            answer["result"]["outcome"],
            json!({"outcome": "cancelled"}),
            "cancel carries the protocol's cancelled outcome"
        );
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
    adapter.send(&handle, input("run it")).expect("send");
    let (request, offered) = next_approval(&events);
    assert_eq!(
        offered,
        ["decline", "cancel"],
        "an allow_always-only ask widens access, so accept stays unoffered"
    );
    adapter
        .answer(
            &handle,
            request,
            ApprovalDecision::OptionId("cancel".to_owned()),
        )
        .expect("cancel lands");
    let _ = through_idle(&events);
    adapter.stop(handle).expect("stop");
    script.join().expect("the agent script completes");
}

#[test]
fn a_permission_request_outside_a_prompt_is_rejected() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent.handshake("sess-1").await;
        agent
            .write(permission_request(
                "perm-0",
                json!([
                    {"optionId": "allow-1", "name": "Allow", "kind": "allow_once"},
                ]),
            ))
            .await;
        let reply = agent.next_client().await;
        assert_eq!(reply["id"], "perm-0");
        assert_eq!(
            reply["error"]["code"].as_i64(),
            Some(-32601),
            "a runless permission ask is answered with a protocol error"
        );
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::WorkspaceWrite))
        .expect("start");
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    let _ = through_closed(&events);
    adapter.stop(handle).expect("stop");
    script.join().expect("the agent script completes");
}

#[test]
fn cancelling_a_run_cancels_its_pending_permissions() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent.handshake("sess-1").await;
        let prompt = agent.next_method("session/prompt").await;
        agent
            .write(permission_request(
                "perm-1",
                json!([
                    {"optionId": "allow-1", "name": "Allow once", "kind": "allow_once"},
                    {"optionId": "deny-1", "name": "Reject", "kind": "reject_once"},
                ]),
            ))
            .await;
        let _cancel = agent.next_method("session/cancel").await;
        // The spec requires a `cancelled` outcome on every parked
        // permission request when the turn is cancelled.
        let answer = agent.next_client().await;
        assert_eq!(answer["id"], "perm-1");
        assert_eq!(answer["result"]["outcome"], json!({"outcome": "cancelled"}));
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
    let run = adapter.send(&handle, input("run it")).expect("send");
    let (request, _offered) = next_approval(&events);
    adapter.cancel(&handle, run).expect("cancel lands");
    let kinds = through_idle(&events);
    // The parked request resolves so a host holding it sees the same
    // answer the agent received.
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::ApprovalResolved { request_id, .. } if *request_id == request
    )));
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
