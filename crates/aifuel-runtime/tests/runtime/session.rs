//! Session lifecycle commands: `session.create`, `session.subscribe`,
//! `session.close`, and the session-scoped error paths.

use crate::support::{
    FakeAdapter, FakeScript, collect_run, consumer, create, created_session, fake_runtime, next_id,
    receipt_code, receipt_seq, receipt_snapshot, run_start, selection, subscribe, test_dir,
};
use aifuel_core::{
    AgentCommand, AgentEventKind, ApprovalDecision, ReceiptCode, RunId, SessionId, SessionStatus,
};

#[test]
fn session_create_subscribe_run_events_completed() {
    let dir = test_dir("flow");
    let (_store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![FakeScript::Complete {
            deltas: vec!["hello".to_owned(), " world".to_owned()],
            cursor: Some("native-9"),
        }],
    );

    let consumer_a = consumer("consumer-a");
    let outcome = runtime.dispatch(create(&dir, "fake-a"), &consumer_a);
    let session_id = created_session(&outcome);
    assert_eq!(
        receipt_seq(&outcome.receipt),
        1,
        "session.created is the log's first fact"
    );

    let events = runtime.events(&consumer_a).expect("consumer channel");
    let outcome = runtime.dispatch(subscribe(&session_id, 0), &consumer_a);
    assert!(outcome.receipt.ok);
    let snapshot = receipt_snapshot(&outcome.receipt);
    assert_eq!(snapshot.status, SessionStatus::Idle);
    assert_eq!(snapshot.selection.model, "fake-a");

    let outcome = runtime.dispatch(run_start(&session_id, "hi"), &consumer_a);
    assert!(outcome.receipt.ok, "run.start succeeds on a live session");

    let collected = collect_run(&events);
    let kinds: Vec<&AgentEventKind> = collected.iter().map(|event| &event.kind).collect();
    // The channel carries the replayed `session.created` then every live
    // fact in causal order.
    assert!(matches!(kinds[0], AgentEventKind::SessionCreated { .. }));
    assert!(matches!(
        kinds[1],
        AgentEventKind::SessionStatus {
            status: SessionStatus::Idle
        }
    ));
    assert!(matches!(kinds[2], AgentEventKind::RunStarted { .. }));
    assert!(matches!(
        kinds[3],
        AgentEventKind::SessionStatus {
            status: SessionStatus::Working
        }
    ));
    let deltas: Vec<&str> = collected
        .iter()
        .filter_map(|event| match &event.kind {
            AgentEventKind::MessageDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, ["hello", " world"]);
    assert!(
        collected.iter().any(|event| matches!(
            &event.kind,
            AgentEventKind::RunCompleted {
                outcome: aifuel_core::RunOutcome::Success,
                ..
            }
        )),
        "the run completes with success"
    );
    // Every event is stamped by the log in order for this session.
    for (index, event) in collected.iter().enumerate() {
        assert_eq!(event.seq, index as u64 + 1);
        assert_eq!(event.session_id, session_id);
    }
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn session_close_records_the_fact_and_blocks_runs() {
    let dir = test_dir("close");
    let (store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![],
    );
    let c1 = consumer("c1");
    let session_id = created_session(&runtime.dispatch(create(&dir, "fake-a"), &c1));

    let outcome = runtime.dispatch(
        AgentCommand::SessionClose {
            command_id: next_id(),
            session_id: session_id.clone(),
        },
        &c1,
    );
    assert!(outcome.receipt.ok);
    assert!(receipt_seq(&outcome.receipt) >= 2);

    let session = store
        .agent_session(&session_id)
        .expect("session reads")
        .expect("session exists");
    assert_eq!(session.status, SessionStatus::Closed);
    let outcome = runtime.dispatch(run_start(&session_id, "hi"), &c1);
    assert_eq!(receipt_code(&outcome), ReceiptCode::InvalidState);
    // Closing twice reports the state honestly rather than erroring blindly.
    let outcome = runtime.dispatch(
        AgentCommand::SessionClose {
            command_id: next_id(),
            session_id: session_id.clone(),
        },
        &c1,
    );
    assert_eq!(receipt_code(&outcome), ReceiptCode::InvalidState);
    // The log's last fact is session.closed.
    let page = store
        .replay(&session_id, 0, 100, usize::MAX)
        .expect("replay reads");
    assert!(matches!(
        page.events.last().map(|event| &event.kind),
        Some(AgentEventKind::SessionClosed { .. })
    ));
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn session_scoped_commands_report_unknown_session() {
    let dir = test_dir("unknown");
    let (_store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![],
    );
    let c1 = consumer("c1");
    let session_id = SessionId::new("no-such-session");
    for command in [
        subscribe(&session_id, 0),
        run_start(&session_id, "hi"),
        AgentCommand::RunCancel {
            command_id: next_id(),
            session_id: session_id.clone(),
            run_id: RunId::new("run-1"),
        },
        AgentCommand::ApprovalAnswer {
            command_id: next_id(),
            session_id: session_id.clone(),
            request_id: aifuel_core::RequestId::new("req-1"),
            decision: ApprovalDecision::OptionId("accept".to_owned()),
        },
        AgentCommand::ModelSelect {
            command_id: next_id(),
            session_id: session_id.clone(),
            selection: selection("fake-a"),
        },
        AgentCommand::SessionClose {
            command_id: next_id(),
            session_id: session_id.clone(),
        },
    ] {
        let outcome = runtime.dispatch(command, &c1);
        assert_eq!(receipt_code(&outcome), ReceiptCode::UnknownSession);
    }
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn session_create_with_full_access_runs() {
    // `full` is a workspace-mutating access mode and must be accepted the
    // same way `workspace-write` is.
    let dir = test_dir("full-access");
    let (_store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![],
    );
    let c1 = consumer("c1");
    let outcome = runtime.dispatch(
        AgentCommand::SessionCreate {
            command_id: next_id(),
            cwd: dir.clone(),
            selection: selection("fake-a"),
            access: aifuel_core::AccessMode::Full,
        },
        &c1,
    );
    assert!(
        outcome.receipt.ok,
        "a full-access session creates: {:?}",
        outcome.receipt.outcome
    );
    let session_id = created_session(&outcome);
    let outcome = runtime.dispatch(run_start(&session_id, "hi"), &c1);
    assert!(outcome.receipt.ok);
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn retried_command_ids_replay_the_recorded_receipt() {
    // A side-effecting command deduplicates by `command_id`: the second
    // dispatch of the same id answers the recorded receipt verbatim rather
    // than creating a second session.
    let dir = test_dir("dedup");
    let (store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![FakeScript::Complete {
            deltas: vec!["done".to_owned()],
            cursor: None,
        }],
    );
    let c1 = consumer("c1");
    let create_command = create(&dir, "fake-a");
    let first = runtime.dispatch(create_command.clone(), &c1);
    let session_id = created_session(&first);
    let replayed = runtime.dispatch(create_command, &c1);
    assert_eq!(
        replayed.receipt, first.receipt,
        "the retry answers the recorded receipt verbatim"
    );
    let sessions = store.agent_sessions().expect("sessions list");
    assert_eq!(sessions.len(), 1, "the retry did not create a session");

    // The same holds for `run.start`: the adapter sees one run, the retry
    // gets the recorded answer.
    let events = runtime.events(&c1).expect("channel");
    runtime.dispatch(subscribe(&session_id, 0), &c1);
    let run_command = run_start(&session_id, "go");
    let command_id = run_command.command_id().clone();
    let first = runtime.dispatch(run_command, &c1);
    assert!(first.receipt.ok);
    let replayed = runtime.dispatch(
        AgentCommand::RunStart {
            command_id,
            session_id: session_id.clone(),
            input: aifuel_core::UserInput {
                text: "must not execute".to_owned(),
                attachments: Vec::new(),
            },
        },
        &c1,
    );
    assert_eq!(
        replayed.receipt, first.receipt,
        "the retried run.start replays the recorded receipt"
    );
    let collected = collect_run(&events);
    let starts = collected
        .iter()
        .filter(|event| matches!(event.kind, AgentEventKind::RunStarted { .. }))
        .count();
    assert_eq!(starts, 1, "the run executed exactly once");

    // Error receipts are recorded the same way: a rejected close replays
    // `invalid_state` without re-appending `session.closed`.
    let session_id = created_session(&runtime.dispatch(create(&dir, "fake-b"), &c1));
    runtime.dispatch(
        AgentCommand::SessionClose {
            command_id: next_id(),
            session_id: session_id.clone(),
        },
        &c1,
    );
    let closed_head = store.head_seq(&session_id).expect("head seq reads");
    let close_command = AgentCommand::SessionClose {
        command_id: next_id(),
        session_id: session_id.clone(),
    };
    let first = runtime.dispatch(close_command.clone(), &c1);
    assert_eq!(receipt_code(&first), ReceiptCode::InvalidState);
    let replayed = runtime.dispatch(close_command, &c1);
    assert_eq!(replayed.receipt, first.receipt);
    assert_eq!(
        store.head_seq(&session_id).expect("head seq reads"),
        closed_head,
        "the retried close appended nothing"
    );
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
