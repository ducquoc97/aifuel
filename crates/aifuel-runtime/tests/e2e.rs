//! End-to-end coverage through the real `CliAdapter`: the facade drives
//! the production session worker, output handler, and interaction plumbing
//! over a scripted [`AgentExecutionAdapter`](aifuel_core::AgentExecutionAdapter),
//! so the pump's dedup and attribution behavior is verified against real
//! adapter emissions rather than the fake `AgentAdapter` double.

mod support;

use aifuel_core::{
    AgentCommand, AgentEventKind, ApprovalDecision, ReceiptCode, ReceiptOutcome, RunOutcome,
};
use support::{
    ExecScript, cli_runtime, collect_run, collect_until, create, created_session, next_id,
    run_start, subscribe, test_dir,
};

#[test]
fn cli_adapter_run_streams_deltas_and_persists_the_resume_cursor() {
    let dir = test_dir("e2e-run");
    let (store, runtime) = cli_runtime(
        &dir,
        vec![ExecScript::Succeed {
            deltas: vec!["he".to_owned(), "llo".to_owned()],
            session_id: Some("native-77"),
        }],
    );
    let session_id = created_session(&runtime.dispatch(create(&dir, "scripted-model"), "host"));
    let events = runtime.events("host").expect("consumer channel");
    assert!(
        runtime
            .dispatch(subscribe(&session_id, 0), "host")
            .receipt
            .ok
    );
    assert!(
        runtime
            .dispatch(run_start(&session_id, "hi"), "host")
            .receipt
            .ok
    );

    let collected = collect_run(&events);
    let deltas: Vec<&str> = collected
        .iter()
        .filter_map(|event| match &event.kind {
            AgentEventKind::MessageDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, ["he", "llo"], "the real output handler streams");
    assert!(
        collected.iter().any(|event| matches!(
            &event.kind,
            AgentEventKind::RunCompleted {
                outcome: RunOutcome::Success,
                ..
            }
        )),
        "the real worker reports a successful completion"
    );
    // The facade's session.created is the single copy in the log even
    // though the real adapter emits its own.
    let created = collected
        .iter()
        .filter(|event| matches!(event.kind, AgentEventKind::SessionCreated { .. }))
        .count();
    assert_eq!(created, 1, "the adapter's copy is deduplicated by the pump");

    runtime.shutdown();
    let session = store
        .agent_session(&session_id)
        .expect("session reads")
        .expect("session exists");
    assert_eq!(
        session.resume_cursor.as_deref(),
        Some("native-77"),
        "the provider session id the real run reported persists as the resume cursor"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cli_adapter_approval_round_trips_through_the_real_handler() {
    let dir = test_dir("e2e-approval");
    let (_store, runtime) = cli_runtime(
        &dir,
        vec![ExecScript::Approve {
            description: "run the build",
        }],
    );
    let session_id = created_session(&runtime.dispatch(create(&dir, "scripted-model"), "host"));
    let events = runtime.events("host").expect("consumer channel");
    runtime.dispatch(subscribe(&session_id, 0), "host");
    runtime.dispatch(run_start(&session_id, "build"), "host");

    let collected = collect_until(&events, |event| {
        matches!(event.kind, AgentEventKind::ApprovalRequested { .. })
    });
    let request_id = collected
        .iter()
        .find_map(|event| match &event.kind {
            AgentEventKind::ApprovalRequested { request_id, .. } => Some(request_id.clone()),
            _ => None,
        })
        .expect("the real interaction handler emits approval.requested");

    let outcome = runtime.dispatch(
        AgentCommand::ApprovalAnswer {
            command_id: next_id(),
            session_id: session_id.clone(),
            request_id: request_id.clone(),
            decision: ApprovalDecision::OptionId("accept".to_owned()),
        },
        "host",
    );
    assert!(outcome.receipt.ok);

    let collected = collect_run(&events);
    match collected
        .iter()
        .find(|event| matches!(event.kind, AgentEventKind::ApprovalResolved { .. }))
        .map(|event| &event.kind)
    {
        Some(AgentEventKind::ApprovalResolved { answered_by, .. }) => {
            assert_eq!(
                answered_by, "host",
                "the pump rewrites the adapter's placeholder with the consumer id"
            );
        }
        other => panic!("expected approval.resolved: {other:?}"),
    }
    // The real adapter reports the second answer as already resolved.
    let outcome = runtime.dispatch(
        AgentCommand::ApprovalAnswer {
            command_id: next_id(),
            session_id: session_id.clone(),
            request_id,
            decision: ApprovalDecision::OptionId("decline".to_owned()),
        },
        "host",
    );
    assert!(matches!(
        outcome.receipt.outcome,
        ReceiptOutcome::Err {
            code: ReceiptCode::AlreadyResolved,
            ..
        }
    ));
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
