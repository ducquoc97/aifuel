//! Durability tests: `session.subscribe` replay, the two-axis replay
//! bound's snapshot fallback, restart reconciliation, and shutdown
//! cursor/interrupt recording - all against a real Session Event Log.

mod support;

use aifuel_app::RunStore;
use aifuel_core::{AgentCommand, AgentEvent, AgentEventKind, ReceiptCode, SessionStatus};
use aifuel_runtime::{AgentRuntime, CommandPayload};
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;
use support::{
    FakeAdapter, FakeScript, collect_run, create, created_session, fake_descriptor, fake_discovery,
    fake_runtime, next_id, receipt_code, receipt_snapshot, run_start, subscribe, test_dir,
};

#[test]
fn subscribe_replays_persisted_events_after_restart() {
    let dir = test_dir("restart");
    let store_path = dir.join("aifuel.db");
    let session_id;
    {
        let (_store, runtime) = fake_runtime(
            &dir,
            FakeAdapter::default_capabilities(),
            vec![FakeAdapter::model("fake-a", &[])],
            vec![FakeScript::Complete {
                deltas: vec!["done".to_owned()],
                cursor: None,
            }],
        );
        let outcome = runtime.dispatch(create(&dir, "fake-a"), "c1");
        session_id = created_session(&outcome);
        let events = runtime.events("c1").expect("channel");
        runtime.dispatch(subscribe(&session_id, 0), "c1");
        runtime.dispatch(run_start(&session_id, "go"), "c1");
        collect_run(&events);
        drop(runtime);
    }

    // A new runtime over the same store is the "restart": the session is
    // persisted but not live.
    let store = RunStore::open(&store_path).expect("store reopens");
    let adapter = std::sync::Arc::new(FakeAdapter::new(
        FakeAdapter::default_capabilities(),
        vec![],
    ));
    let runtime = AgentRuntime::with_adapters(
        store.clone(),
        vec![adapter],
        vec![fake_descriptor()],
        fake_discovery(&dir),
    )
    .expect("runtime reopens");

    let events = runtime.events("c2").expect("channel");
    let outcome = runtime.dispatch(subscribe(&session_id, 0), "c2");
    assert!(outcome.receipt.ok);
    let snapshot = receipt_snapshot(&outcome.receipt);
    assert_eq!(snapshot.status, SessionStatus::Idle);
    assert_eq!(snapshot.selection.model, "fake-a");
    assert!(snapshot.head_seq >= 6, "the whole run is in the log");

    // The replay arrives on the consumer channel in order, up to head_seq.
    let mut replayed: Vec<AgentEvent> = Vec::new();
    while replayed.last().map(|event| event.seq) != Some(snapshot.head_seq) {
        replayed.push(
            events
                .recv_timeout(Duration::from_secs(10))
                .expect("replayed event arrives"),
        );
    }
    assert!(matches!(
        replayed[0].kind,
        AgentEventKind::SessionCreated { .. }
    ));
    assert!(
        replayed.iter().any(|event| matches!(
            &event.kind,
            AgentEventKind::RunCompleted {
                outcome: aifuel_core::RunOutcome::Success,
                ..
            }
        )),
        "the persisted run.completed replays"
    );

    // The session is not live: starting a run is invalid_state, and
    // session.list still reports it.
    let outcome = runtime.dispatch(run_start(&session_id, "again"), "c2");
    assert_eq!(receipt_code(&outcome), ReceiptCode::InvalidState);
    let outcome = runtime.dispatch(
        AgentCommand::SessionList {
            command_id: next_id(),
        },
        "c2",
    );
    match &outcome.payload {
        CommandPayload::Sessions(sessions) => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].session_id, session_id);
        }
        _ => panic!("session.list carries the sessions payload"),
    }
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn replay_past_the_count_bound_falls_back_to_snapshot() {
    let dir = test_dir("replay-count");
    let deltas: Vec<String> = (0..300).map(|index| format!("d{index}")).collect();
    let (_store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![FakeScript::Complete {
            deltas,
            cursor: None,
        }],
    );
    let session_id = created_session(&runtime.dispatch(create(&dir, "fake-a"), "c1"));
    let events = runtime.events("c1").expect("channel");
    runtime.dispatch(subscribe(&session_id, 0), "c1");
    runtime.dispatch(run_start(&session_id, "go"), "c1");
    let collected = collect_run(&events);
    let head = collected.last().expect("events").seq;

    // A second consumer subscribing from zero lags past the count bound:
    // replay is skipped and the receipt carries the fresh snapshot.
    let second = runtime.events("c2").expect("channel");
    let outcome = runtime.dispatch(subscribe(&session_id, 0), "c2");
    assert!(outcome.receipt.ok);
    let snapshot = receipt_snapshot(&outcome.receipt);
    assert_eq!(snapshot.head_seq, head);
    assert!(
        !snapshot.tail.is_empty(),
        "the snapshot tail carries the last turn"
    );
    assert!(
        matches!(
            second.recv_timeout(Duration::from_millis(300)),
            Err(RecvTimeoutError::Timeout)
        ),
        "a truncated replay pushes nothing onto the channel"
    );
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn replay_past_the_byte_bound_falls_back_to_snapshot() {
    let dir = test_dir("replay-bytes");
    let big = "x".repeat(700 * 1024);
    let (_store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![FakeScript::Complete {
            deltas: vec![big.clone(), big],
            cursor: None,
        }],
    );
    let session_id = created_session(&runtime.dispatch(create(&dir, "fake-a"), "c1"));
    let events = runtime.events("c1").expect("channel");
    runtime.dispatch(subscribe(&session_id, 0), "c1");
    runtime.dispatch(run_start(&session_id, "go"), "c1");
    collect_run(&events);

    let second = runtime.events("c2").expect("channel");
    let outcome = runtime.dispatch(subscribe(&session_id, 0), "c2");
    assert!(outcome.receipt.ok);
    receipt_snapshot(&outcome.receipt);
    assert!(
        matches!(
            second.recv_timeout(Duration::from_millis(300)),
            Err(RecvTimeoutError::Timeout)
        ),
        "a byte-bound overflow falls back to the snapshot"
    );
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn shutdown_marks_in_flight_interrupted_and_records_cursor() {
    let dir = test_dir("shutdown");
    let store_path = dir.join("aifuel.db");
    let session_id;
    {
        let (store, runtime) = fake_runtime(
            &dir,
            FakeAdapter::default_capabilities(),
            vec![FakeAdapter::model("fake-a", &[])],
            vec![FakeScript::Block {
                cursor: Some("native-42"),
            }],
        );
        let outcome = runtime.dispatch(create(&dir, "fake-a"), "c1");
        session_id = created_session(&outcome);
        let outcome = runtime.dispatch(run_start(&session_id, "work"), "c1");
        assert!(outcome.receipt.ok);
        // Wait for the pump to project `working` before the shutdown reads
        // the cursor and marks the session.
        for _ in 0..200 {
            let working = store
                .agent_session(&session_id)
                .expect("session reads")
                .is_some_and(|session| session.status == SessionStatus::Working);
            if working {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        runtime.shutdown();
        let session = store
            .agent_session(&session_id)
            .expect("session reads")
            .expect("session exists");
        assert_eq!(session.status, SessionStatus::Interrupted);
        assert_eq!(
            session.resume_cursor.as_deref(),
            Some("native-42"),
            "the provider resume cursor persists for restart reconciliation"
        );
    }

    // Reopened, the session stays interrupted and cannot run.
    let store = RunStore::open(&store_path).expect("store reopens");
    let runtime = AgentRuntime::with_adapters(
        store.clone(),
        vec![std::sync::Arc::new(FakeAdapter::new(
            FakeAdapter::default_capabilities(),
            vec![],
        ))],
        vec![fake_descriptor()],
        fake_discovery(&dir),
    )
    .expect("runtime reopens");
    let session = store
        .agent_session(&session_id)
        .expect("session reads")
        .expect("session exists");
    assert_eq!(session.status, SessionStatus::Interrupted);
    let outcome = runtime.dispatch(run_start(&session_id, "again"), "c2");
    assert_eq!(receipt_code(&outcome), ReceiptCode::InvalidState);
    let outcome = runtime.dispatch(subscribe(&session_id, 0), "c2");
    assert!(outcome.receipt.ok);
    assert_eq!(
        receipt_snapshot(&outcome.receipt).status,
        SessionStatus::Interrupted
    );
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
