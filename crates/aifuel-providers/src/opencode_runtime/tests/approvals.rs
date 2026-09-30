//! Permission requests: raise the contract ask, forward the provider's
//! reply vocabulary, and reject anything else.

use super::*;
use aifuel_core::{AgentAdapter, ApprovalDecision, ReceiptCode, RunOutcome};

/// One `permission.updated` ask as the serve reports it.
fn permission_event(session: &str, permission_id: &str) -> Value {
    json!({
        "type": "permission.updated",
        "properties": {
            "id": permission_id,
            "type": "bash",
            "pattern": "rm -rf *",
            "sessionID": session,
            "messageID": "msg_asst",
            "title": "delete everything",
            "metadata": {},
            "time": {"created": 1},
        },
    })
}

fn done_response() -> Value {
    json!({
        "info": {"id": "msg_asst", "tokens": {"input": 0, "output": 0}},
        "parts": [],
    })
}

#[test]
fn a_permission_request_round_trips_through_the_permissions_route() {
    let fake = FakeServe::start();
    let adapter = adapter_with(&fake);
    let (handle, events) = start_session(&adapter, &fake, options(AccessMode::WorkspaceWrite));
    expect_prelude(&events);

    fake.hold_prompt();
    let run = adapter.send(&handle, input("hi")).expect("the run starts");
    fake.push_event(permission_event(FAKE_SESSION, "per_1"));

    let requested = loop {
        match recv(&events) {
            AgentEventKind::ApprovalRequested {
                request_id,
                request,
                run_id,
            } => {
                assert_eq!(run_id, run);
                assert_eq!(request.title, "delete everything");
                let options: Vec<&str> = request
                    .options
                    .iter()
                    .map(|option| option.id.as_str())
                    .collect();
                assert_eq!(options, ["once", "always", "reject"]);
                break request_id;
            }
            _ => continue,
        }
    };
    adapter
        .answer(
            &handle,
            requested.clone(),
            ApprovalDecision::OptionId("always".to_owned()),
        )
        .expect("the answer is accepted");

    let answer = fake
        .requests()
        .into_iter()
        .find(|request| request.path.contains("/permissions/per_1"))
        .expect("the permission reply was posted");
    let body: Value = serde_json::from_str(&answer.body).expect("reply body is JSON");
    assert_eq!(body["response"], "always");

    assert!(matches!(
        recv(&events),
        AgentEventKind::ApprovalResolved { request_id, .. } if request_id == requested
    ));
    assert!(matches!(
        recv(&events),
        AgentEventKind::SessionStatus {
            status: SessionStatus::Working
        }
    ));

    fake.settle_prompt(done_response());
    let kinds = through_idle(&events);
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        AgentEventKind::RunCompleted {
            outcome: RunOutcome::Success,
            ..
        }
    )));
}

#[test]
fn answering_an_unknown_request_reports_already_resolved() {
    let fake = FakeServe::start();
    let adapter = adapter_with(&fake);
    let (handle, _events) = start_session(&adapter, &fake, options(AccessMode::WorkspaceWrite));
    let error = adapter
        .answer(
            &handle,
            aifuel_core::RequestId::new("per_nothing"),
            ApprovalDecision::OptionId("reject".to_owned()),
        )
        .expect_err("no such pending request");
    assert_eq!(error.code, ReceiptCode::AlreadyResolved);
}

#[test]
fn read_only_sessions_offer_reject_only() {
    let fake = FakeServe::start();
    let adapter = adapter_with(&fake);
    let (handle, events) = start_session(&adapter, &fake, options(AccessMode::ReadOnly));
    expect_prelude(&events);

    fake.hold_prompt();
    adapter.send(&handle, input("hi")).expect("the run starts");
    fake.push_event(permission_event(FAKE_SESSION, "per_ro"));

    let (requested, options) = loop {
        match recv(&events) {
            AgentEventKind::ApprovalRequested {
                request_id,
                request,
                ..
            } => {
                break (
                    request_id,
                    request
                        .options
                        .iter()
                        .map(|option| option.id.clone())
                        .collect::<Vec<_>>(),
                );
            }
            _ => continue,
        }
    };
    assert_eq!(options, ["reject"]);

    // An option that was never offered is rejected, not guessed.
    let error = adapter
        .answer(
            &handle,
            requested.clone(),
            ApprovalDecision::OptionId("always".to_owned()),
        )
        .expect_err("an unoffered option cannot answer");
    assert_eq!(error.code, ReceiptCode::InvalidState);

    adapter
        .answer(
            &handle,
            requested,
            ApprovalDecision::OptionId("reject".to_owned()),
        )
        .expect("reject answers");
    fake.settle_prompt(done_response());
    through_idle(&events);
}

#[test]
fn a_reply_from_elsewhere_resolves_the_pending_request() {
    let fake = FakeServe::start();
    let adapter = adapter_with(&fake);
    let (handle, events) = start_session(&adapter, &fake, options(AccessMode::WorkspaceWrite));
    expect_prelude(&events);

    fake.hold_prompt();
    adapter.send(&handle, input("hi")).expect("the run starts");
    fake.push_event(permission_event(FAKE_SESSION, "per_2"));
    loop {
        if matches!(recv(&events), AgentEventKind::ApprovalRequested { .. }) {
            break;
        }
    }

    // Another client answered; the pending request resolves honestly.
    fake.push_event(json!({
        "type": "permission.replied",
        "properties": {"sessionID": FAKE_SESSION, "permissionID": "per_2", "response": "once"},
    }));
    loop {
        if matches!(recv(&events), AgentEventKind::ApprovalResolved { .. }) {
            break;
        }
    }
    let error = adapter
        .answer(
            &handle,
            aifuel_core::RequestId::new("per_2"),
            ApprovalDecision::OptionId("reject".to_owned()),
        )
        .expect_err("the request is no longer pending");
    assert_eq!(error.code, ReceiptCode::AlreadyResolved);
    fake.settle_prompt(done_response());
    through_idle(&events);
}
