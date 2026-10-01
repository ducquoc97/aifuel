//! Durability tests: `session.subscribe` replay, the two-axis replay
//! bound's snapshot fallback, restart reconciliation, resume-cursor
//! continuation on open, and shutdown cursor/interrupt recording - all
//! against a real Session Event Log.

mod support;

use aifuel_app::RunStore;
use aifuel_core::{
    AdapterCapabilities, AgentCommand, AgentEvent, AgentEventKind, ReceiptCode, SessionStatus,
};
use aifuel_runtime::{AgentRuntime, CommandPayload};
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;
use support::{
    FakeAdapter, FakeScript, collect_run, consumer, create, created_session, fake_descriptor,
    fake_discovery, fake_runtime, next_id, receipt_code, receipt_snapshot, run_start, selection,
    subscribe, test_dir,
};

/// `with_adapters` over the fake integration with one scripted adapter.
fn reopen(dir: &std::path::Path, adapter: FakeAdapter) -> (RunStore, AgentRuntime) {
    let store = RunStore::open(dir.join("aifuel.db")).expect("store reopens");
    let runtime = AgentRuntime::with_adapters(
        store.clone(),
        vec![Arc::new(adapter)],
        vec![fake_descriptor()],
        Vec::new(),
        fake_discovery(dir),
    )
    .expect("runtime reopens");
    (store, runtime)
}

/// Interrupt one in-flight session whose adapter reports `cursor` as its
/// provider session id: a run blocks, shutdown records the cursor and the
/// `interrupted` fact, and the runtime drops.
fn interrupt_session(
    dir: &std::path::Path,
    cursor: Option<&'static str>,
) -> aifuel_core::SessionId {
    let (store, runtime) = fake_runtime(
        dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![FakeScript::Block { cursor }],
    );
    let c1 = consumer("c1");
    let session_id = created_session(&runtime.dispatch(create(dir, "fake-a"), &c1));
    let outcome = runtime.dispatch(run_start(&session_id, "work"), &c1);
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
    session_id
}

#[test]
fn subscribe_replays_persisted_events_after_restart() {
    let dir = test_dir("restart");
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
        let c1 = consumer("c1");
        let outcome = runtime.dispatch(create(&dir, "fake-a"), &c1);
        session_id = created_session(&outcome);
        let events = runtime.events(&c1).expect("channel");
        runtime.dispatch(subscribe(&session_id, 0), &c1);
        runtime.dispatch(run_start(&session_id, "go"), &c1);
        collect_run(&events);
        drop(runtime);
    }

    // A new runtime over the same store is the "restart": the session is
    // persisted but not live.
    let (_store, runtime) = reopen(
        &dir,
        FakeAdapter::new(FakeAdapter::default_capabilities(), vec![]),
    );

    let c2 = consumer("c2");
    let events = runtime.events(&c2).expect("channel");
    let outcome = runtime.dispatch(subscribe(&session_id, 0), &c2);
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
    let outcome = runtime.dispatch(run_start(&session_id, "again"), &c2);
    assert_eq!(receipt_code(&outcome), ReceiptCode::InvalidState);
    let outcome = runtime.dispatch(
        AgentCommand::SessionList {
            command_id: next_id(),
        },
        &c2,
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
    let c1 = consumer("c1");
    let session_id = created_session(&runtime.dispatch(create(&dir, "fake-a"), &c1));
    let events = runtime.events(&c1).expect("channel");
    runtime.dispatch(subscribe(&session_id, 0), &c1);
    runtime.dispatch(run_start(&session_id, "go"), &c1);
    let collected = collect_run(&events);
    let head = collected.last().expect("events").seq;

    // A second consumer subscribing from zero lags past the count bound:
    // replay is skipped and the receipt carries the fresh snapshot.
    let c2 = consumer("c2");
    let second = runtime.events(&c2).expect("channel");
    let outcome = runtime.dispatch(subscribe(&session_id, 0), &c2);
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
    let c1 = consumer("c1");
    let session_id = created_session(&runtime.dispatch(create(&dir, "fake-a"), &c1));
    let events = runtime.events(&c1).expect("channel");
    runtime.dispatch(subscribe(&session_id, 0), &c1);
    runtime.dispatch(run_start(&session_id, "go"), &c1);
    collect_run(&events);

    let c2 = consumer("c2");
    let second = runtime.events(&c2).expect("channel");
    let outcome = runtime.dispatch(subscribe(&session_id, 0), &c2);
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
    let session_id = interrupt_session(&dir, Some("native-42"));
    let session = RunStore::open(dir.join("aifuel.db"))
        .expect("store reopens")
        .agent_session(&session_id)
        .expect("session reads")
        .expect("session exists");
    assert_eq!(session.status, SessionStatus::Interrupted);
    assert_eq!(
        session.resume_cursor.as_deref(),
        Some("native-42"),
        "the provider resume cursor persists for restart reconciliation"
    );

    // An adapter without the `resume` capability cannot continue the
    // session: it stays interrupted and cannot run.
    let (store, runtime) = reopen(
        &dir,
        FakeAdapter::new(
            AdapterCapabilities {
                resume: false,
                ..FakeAdapter::default_capabilities()
            },
            vec![],
        ),
    );
    let session = store
        .agent_session(&session_id)
        .expect("session reads")
        .expect("session exists");
    assert_eq!(session.status, SessionStatus::Interrupted);
    let c2 = consumer("c2");
    let outcome = runtime.dispatch(run_start(&session_id, "again"), &c2);
    assert_eq!(receipt_code(&outcome), ReceiptCode::InvalidState);
    let outcome = runtime.dispatch(subscribe(&session_id, 0), &c2);
    assert!(outcome.receipt.ok);
    assert_eq!(
        receipt_snapshot(&outcome.receipt).status,
        SessionStatus::Interrupted
    );
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn interrupted_session_with_cursor_resumes_on_open() {
    // On open, a persisted `interrupted` session holding a resume cursor
    // gets a provider-side continuation: the facade records `working` and
    // the session is live again.
    let dir = test_dir("resume");
    let session_id = interrupt_session(&dir, Some("native-42"));
    let (store, runtime) = reopen(
        &dir,
        FakeAdapter::new(
            FakeAdapter::default_capabilities(),
            vec![FakeAdapter::model("fake-a", &[])],
        )
        .with_scripts(vec![FakeScript::Complete {
            deltas: vec!["resumed".to_owned()],
            cursor: Some("native-43"),
        }]),
    );
    let session = store
        .agent_session(&session_id)
        .expect("session reads")
        .expect("session exists");
    assert_eq!(
        session.status,
        SessionStatus::Working,
        "the continuation's `working` fact is the head status"
    );

    // The resumed session is live: a run drives the adapter again and the
    // stale fresh-session `idle` prelude never rewrote the projection.
    let c1 = consumer("c1");
    let events = runtime.events(&c1).expect("channel");
    let outcome = runtime.dispatch(subscribe(&session_id, 0), &c1);
    assert!(outcome.receipt.ok);
    let outcome = runtime.dispatch(run_start(&session_id, "again"), &c1);
    assert!(
        outcome.receipt.ok,
        "the resumed session is live: {:?}",
        outcome.receipt.outcome
    );
    let collected = collect_run(&events);
    // The replayed tail shows the reconcile's `interrupted` fact followed
    // by the facade-authored `working`; the adapter's startup `idle` never
    // landed between them.
    let statuses: Vec<SessionStatus> = collected
        .iter()
        .filter_map(|event| match &event.kind {
            AgentEventKind::SessionStatus { status } => Some(*status),
            _ => None,
        })
        .collect();
    let interrupted_at = statuses
        .iter()
        .position(|status| *status == SessionStatus::Interrupted)
        .expect("the reconcile recorded the interruption");
    assert_eq!(
        statuses.get(interrupted_at + 1),
        Some(&SessionStatus::Working),
        "the facade-authored working follows the interruption"
    );
    assert_eq!(
        statuses.get(interrupted_at + 2),
        Some(&SessionStatus::Working),
        "the run's own working follows; no stale idle undid the resume"
    );
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn interrupted_sessions_redeclare_their_external_tools_on_resume() {
    // The declared tools persist with the session row: whichever runtime
    // adopts the interrupted session restarts the adapter under the same
    // enforcement - the allowlist is a fact, not a per-process memory.
    let dir = test_dir("resume-tools");
    let session_id;
    {
        let (store, runtime) = fake_runtime(
            &dir,
            AdapterCapabilities {
                external_tools: true,
                ..FakeAdapter::default_capabilities()
            },
            vec![FakeAdapter::model("fake-a", &[])],
            vec![FakeScript::Block {
                cursor: Some("native-42"),
            }],
        );
        let c1 = consumer("c1");
        let outcome = runtime.dispatch(
            AgentCommand::SessionCreate {
                command_id: next_id(),
                cwd: dir.clone(),
                selection: selection("fake-a"),
                access: aifuel_core::AccessMode::ReadOnly,
                resume_cursor: None,
                external_tools: vec!["gateway.search".to_owned()],
            },
            &c1,
        );
        session_id = created_session(&outcome);
        assert!(
            runtime
                .dispatch(run_start(&session_id, "work"), &c1)
                .receipt
                .ok
        );
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
    }

    // A fresh runtime adopts the orphaned row: the adapter's start sees
    // the same tool allowlist the first host declared.
    let resuming = Arc::new(FakeAdapter::new(
        AdapterCapabilities {
            external_tools: true,
            ..FakeAdapter::default_capabilities()
        },
        vec![FakeAdapter::model("fake-a", &[])],
    ));
    let store = RunStore::open(dir.join("aifuel.db")).expect("store reopens");
    let runtime = AgentRuntime::with_adapters(
        store.clone(),
        vec![resuming.clone()],
        vec![fake_descriptor()],
        Vec::new(),
        fake_discovery(&dir),
    )
    .expect("runtime reopens");
    assert_eq!(
        resuming.recorded_tools(),
        vec![vec!["gateway.search".to_owned()]],
        "the adopting runtime redeclares the persisted tools on start"
    );
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn interrupted_session_stays_down_when_resume_fails() {
    // A failed continuation leaves the session `interrupted` - resume was
    // attempted, never assumed.
    let dir = test_dir("resume-fail");
    let session_id = interrupt_session(&dir, Some("native-42"));
    let (store, runtime) = reopen(
        &dir,
        FakeAdapter::new(FakeAdapter::default_capabilities(), vec![]).failing_to_start(),
    );
    let session = store
        .agent_session(&session_id)
        .expect("session reads")
        .expect("session exists");
    assert_eq!(session.status, SessionStatus::Interrupted);
    let c1 = consumer("c1");
    let outcome = runtime.dispatch(run_start(&session_id, "again"), &c1);
    assert_eq!(receipt_code(&outcome), ReceiptCode::InvalidState);
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn interrupted_session_without_cursor_stays_interrupted() {
    // No persisted resume cursor means there is nothing to continue:
    // a resume-capable adapter still leaves the session interrupted.
    let dir = test_dir("resume-none");
    let session_id = interrupt_session(&dir, None);
    let (store, runtime) = reopen(
        &dir,
        FakeAdapter::new(FakeAdapter::default_capabilities(), vec![]),
    );
    let session = store
        .agent_session(&session_id)
        .expect("session reads")
        .expect("session exists");
    assert_eq!(session.status, SessionStatus::Interrupted);
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_second_runtime_leaves_a_live_owners_sessions_alone() {
    // Two runtimes sharing one store each own their sessions: opening a
    // second runtime reconciles only orphaned sessions, and its shutdown
    // marks only its own in-flight work.
    let dir = test_dir("two-owners");
    let (store_a, runtime_a) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![FakeScript::Block { cursor: None }],
    );
    let c1 = consumer("c1");
    let session_id = created_session(&runtime_a.dispatch(create(&dir, "fake-a"), &c1));
    let outcome = runtime_a.dispatch(run_start(&session_id, "work"), &c1);
    assert!(outcome.receipt.ok);
    let status = || {
        store_a
            .agent_session(&session_id)
            .expect("session reads")
            .expect("session exists")
            .status
    };
    for _ in 0..200 {
        if status() == SessionStatus::Working {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(status(), SessionStatus::Working);

    // B opens over the same store while A still lives: the startup
    // reconcile must leave A's in-flight session working.
    let (_store_b, runtime_b) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![],
    );
    assert_eq!(
        status(),
        SessionStatus::Working,
        "a live owner's session is never reconciled by another open"
    );

    // B's shutdown marks only B's sessions - A's keeps running.
    runtime_b.shutdown();
    assert_eq!(
        status(),
        SessionStatus::Working,
        "a foreign owner's shutdown leaves the session alone"
    );

    // A's own shutdown does mark it.
    runtime_a.shutdown();
    assert_eq!(status(), SessionStatus::Interrupted);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_cursor_reads_the_live_adapter_then_the_persisted_row() {
    // `AgentRuntime::resume_cursor` answers from the live adapter while the
    // session runs, then from the persisted row once it is not live.
    let dir = test_dir("cursor-read");
    let (_store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![FakeScript::Block {
            cursor: Some("native-7"),
        }],
    );
    let c1 = consumer("c1");
    let session_id = created_session(&runtime.dispatch(create(&dir, "fake-a"), &c1));
    assert_eq!(runtime.resume_cursor(&session_id), None);
    let outcome = runtime.dispatch(run_start(&session_id, "work"), &c1);
    assert!(outcome.receipt.ok);
    assert_eq!(
        runtime.resume_cursor(&session_id).as_deref(),
        Some("native-7"),
        "the live adapter's cursor answers first"
    );
    runtime.shutdown();
    drop(runtime);

    // A runtime whose adapter cannot resume never adopts the session, so
    // the read falls through to the persisted row.
    let (_store, runtime) = reopen(
        &dir,
        FakeAdapter::new(
            AdapterCapabilities {
                resume: false,
                ..FakeAdapter::default_capabilities()
            },
            vec![],
        ),
    );
    assert_eq!(
        runtime.resume_cursor(&session_id).as_deref(),
        Some("native-7"),
        "the persisted cursor answers when the session is not live"
    );
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
