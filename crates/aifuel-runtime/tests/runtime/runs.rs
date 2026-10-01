//! Run-scoped commands: `run.start`, `run.cancel`, `approval.answer`,
//! and the per-consumer event channel.

use crate::support::{
    FakeAdapter, FakeScript, collect_run, collect_until, consumer, create, created_session,
    fake_runtime, receipt_code, run_start, subscribe, test_dir,
};
use aifuel_core::{
    AgentCommand, AgentEventKind, ApprovalDecision, ApprovalKind, ApprovalRequest, ReceiptCode,
    ReceiptOutcome, RunOutcome,
};

#[test]
fn approval_answer_round_trip_and_second_answer_loses() {
    let dir = test_dir("approval");
    let request = ApprovalRequest {
        kind: ApprovalKind::ToolPermission,
        title: "rm -rf build/".to_owned(),
        detail: "the provider wants to clean".to_owned(),
        options: vec![
            aifuel_core::ApprovalOption {
                id: "accept".to_owned(),
                label: "Accept".to_owned(),
            },
            aifuel_core::ApprovalOption {
                id: "decline".to_owned(),
                label: "Decline".to_owned(),
            },
        ],
        requires_confirm: false,
        interaction_kind: None,
        questions: Vec::new(),
        parameters: None,
        native_method: None,
    };
    let (_store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![FakeScript::Approval {
            request: request.clone(),
        }],
    );
    let consumer_a = consumer("consumer-a");
    let session_id = created_session(&runtime.dispatch(create(&dir, "fake-a"), &consumer_a));
    let events = runtime.events(&consumer_a).expect("channel");
    runtime.dispatch(subscribe(&session_id, 0), &consumer_a);
    runtime.dispatch(run_start(&session_id, "build it"), &consumer_a);

    // Wait for the request fact, then answer through the command surface.
    let collected = collect_until(&events, |event| {
        matches!(&event.kind, AgentEventKind::ApprovalRequested { .. })
    });
    let request_id = collected
        .iter()
        .find_map(|event| match &event.kind {
            AgentEventKind::ApprovalRequested { request_id, .. } => Some(request_id.clone()),
            _ => None,
        })
        .expect("the approval request fact");
    let outcome = runtime.dispatch(
        AgentCommand::ApprovalAnswer {
            command_id: crate::support::next_id(),
            session_id: session_id.clone(),
            request_id: request_id.clone(),
            decision: ApprovalDecision::OptionId("accept".to_owned()),
        },
        &consumer_a,
    );
    assert!(outcome.receipt.ok, "the first answer wins");
    // The attribution field carries the answering consumer.
    let resolved = collect_run(&events);
    let resolved_event = resolved
        .iter()
        .find_map(|event| match &event.kind {
            AgentEventKind::ApprovalResolved {
                request_id: id,
                decision,
                answered_by,
            } if *id == request_id => Some((decision.clone(), answered_by.clone())),
            _ => None,
        })
        .expect("the resolved fact");
    assert_eq!(
        resolved_event,
        (
            ApprovalDecision::OptionId("accept".to_owned()),
            "consumer-a".to_owned()
        )
    );

    // A second answer for the same request reports `already_resolved`.
    let outcome = runtime.dispatch(
        AgentCommand::ApprovalAnswer {
            command_id: crate::support::next_id(),
            session_id: session_id.clone(),
            request_id,
            decision: ApprovalDecision::OptionId("decline".to_owned()),
        },
        &consumer_a,
    );
    assert_eq!(receipt_code(&outcome), ReceiptCode::AlreadyResolved);
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_cancel_reports_cancelled_completion() {
    let dir = test_dir("cancel");
    let (_store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![FakeScript::Block { cursor: None }],
    );
    let c1 = consumer("c1");
    let session_id = created_session(&runtime.dispatch(create(&dir, "fake-a"), &c1));
    let events = runtime.events(&c1).expect("channel");
    runtime.dispatch(subscribe(&session_id, 0), &c1);
    let outcome = runtime.dispatch(run_start(&session_id, "block"), &c1);
    // The receipt names the accepted run so cancel needs no event wait.
    let run_id = match &outcome.receipt.outcome {
        ReceiptOutcome::Ok {
            run_id: Some(run_id),
            ..
        } => run_id.clone(),
        other => panic!("run.start should carry the accepted run id: {other:?}"),
    };
    // The event stream reports the same run identity.
    let started = collect_until(&events, |event| {
        matches!(&event.kind, AgentEventKind::RunStarted { .. })
    });
    let started_id = started
        .iter()
        .find_map(|event| match &event.kind {
            AgentEventKind::RunStarted { run_id, .. } => Some(run_id.clone()),
            _ => None,
        })
        .expect("the run.started fact");
    assert_eq!(run_id, started_id, "receipt and event name the same run");
    let outcome = runtime.dispatch(
        AgentCommand::RunCancel {
            command_id: crate::support::next_id(),
            session_id,
            run_id,
        },
        &c1,
    );
    assert!(outcome.receipt.ok, "cancel succeeds while the run blocks");
    let events = collect_run(&events);
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        AgentEventKind::RunCompleted {
            outcome: RunOutcome::Cancelled,
            ..
        }
    )));
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn two_consumers_each_receive_the_session_events() {
    let dir = test_dir("broadcast");
    let (_store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![FakeScript::Complete {
            deltas: vec!["x".to_owned()],
            cursor: None,
        }],
    );
    let consumer_a = consumer("consumer-a");
    let consumer_b = consumer("consumer-b");
    let session_id = created_session(&runtime.dispatch(create(&dir, "fake-a"), &consumer_a));
    let events_a = runtime.events(&consumer_a).expect("channel a");
    let events_b = runtime.events(&consumer_b).expect("channel b");
    runtime.dispatch(subscribe(&session_id, 0), &consumer_a);
    runtime.dispatch(subscribe(&session_id, 0), &consumer_b);
    runtime.dispatch(run_start(&session_id, "go"), &consumer_a);
    let a = collect_run(&events_a);
    let b = collect_run(&events_b);
    assert_eq!(a.len(), b.len(), "both consumers see the same facts");
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn events_channel_is_per_consumer() {
    // `events` returns the same consumer's channel; a second call does
    // not steal it, and different consumers are independent.
    let dir = test_dir("channels");
    let (_store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![],
    );
    let c1 = consumer("c1");
    let first = runtime.events(&c1).expect("channel");
    let second = runtime.events(&c1);
    assert!(
        second.is_none(),
        "the consumer's channel is already claimed"
    );
    assert!(runtime.events(&consumer("c2")).is_some());
    drop(first);
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
