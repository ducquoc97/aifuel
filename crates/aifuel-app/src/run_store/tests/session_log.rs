//! Session Event Log tests: per-session sequence assignment, bounded replay,
//! the materialized snapshot read model, and shutdown interruption.

use super::*;
use aifuel_core::{
    AgentEventKind, ApprovalDecision, ApprovalKind, ApprovalOption, ApprovalRequest, Effort,
    MessageStream, ModelSelection, RequestId, RunId, RunOutcome, SessionId, SessionStatus,
};
use std::path::Path;

fn agent_session_selection() -> ModelSelection {
    ModelSelection {
        integration_id: IntegrationId::new("claude"),
        model: "model-a".to_owned(),
        effort: Some(Effort::High),
    }
}

fn register_agent_session(store: &RunStore, session_id: &str) -> SessionId {
    let session_id = SessionId::new(session_id);
    store
        .record_agent_session(&session_id, &agent_session_selection(), Path::new("/repo"))
        .expect("agent session registers");
    session_id
}

fn run_started(run_id: &str) -> AgentEventKind {
    AgentEventKind::RunStarted {
        run_id: RunId::new(run_id),
        selection: agent_session_selection(),
    }
}

#[test]
fn session_event_seq_is_monotonic_across_runs() {
    let path = store_path("session-seq");
    let store = RunStore::open(&path).expect("store opens");
    let session_id = register_agent_session(&store, "s-1");

    // The sequence belongs to the Agent Session: a second Agent Run must
    // continue it rather than restart at one the way per-run `events` do.
    let first = store
        .append(&session_id, run_started("run-a"))
        .expect("event appends");
    let second = store
        .append(
            &session_id,
            AgentEventKind::MessageDelta {
                run_id: RunId::new("run-a"),
                stream: MessageStream::Assistant,
                text: "chunk".to_owned(),
            },
        )
        .expect("event appends");
    let third = store
        .append(&session_id, run_started("run-b"))
        .expect("event appends");
    let fourth = store
        .append(
            &session_id,
            AgentEventKind::RunCompleted {
                run_id: RunId::new("run-b"),
                outcome: RunOutcome::Success,
                usage: None,
            },
        )
        .expect("event appends");

    assert_eq!(first.seq, 1);
    assert_eq!(second.seq, 2);
    assert_eq!(third.seq, 3);
    assert_eq!(fourth.seq, 4);
    assert!(first.ts > 0.0, "the log stamps ts at append time");
    assert_eq!(first.session_id, session_id);

    // Replayed envelopes carry the stamped identity and kind back intact.
    let page = store
        .replay(&session_id, 0, 100, usize::MAX)
        .expect("replay reads");
    assert_eq!(page.head_seq, 4);
    assert!(!page.truncated);
    assert_eq!(
        page.events
            .iter()
            .map(|event| event.seq)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert_eq!(page.events[0].kind, run_started("run-a"));
    assert_eq!(
        page.events[3].kind,
        AgentEventKind::RunCompleted {
            run_id: RunId::new("run-b"),
            outcome: RunOutcome::Success,
            usage: None,
        }
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn session_event_replay_binds_count_and_bytes() {
    let path = store_path("session-replay");
    let store = RunStore::open(&path).expect("store opens");
    let session_id = register_agent_session(&store, "s-1");
    for index in 0..5 {
        store
            .append(
                &session_id,
                AgentEventKind::MessageDelta {
                    run_id: RunId::new("run-a"),
                    stream: MessageStream::Assistant,
                    text: format!("chunk-{index}"),
                },
            )
            .expect("event appends");
    }

    // The count bound cuts the page and flags the lag: the consumer falls
    // back to a snapshot rather than trusting a partial replay.
    let page = store
        .replay(&session_id, 0, 2, usize::MAX)
        .expect("replay reads");
    assert_eq!(page.events.len(), 2);
    assert_eq!(page.head_seq, 5);
    assert!(page.truncated, "events remain past the page bound");

    // The cursor continues from the last seen seq without repeats.
    let page = store
        .replay(&session_id, 2, 100, usize::MAX)
        .expect("replay reads");
    assert_eq!(
        page.events
            .iter()
            .map(|event| event.seq)
            .collect::<Vec<_>>(),
        vec![3, 4, 5]
    );
    assert!(!page.truncated);

    // The byte bound works the same way: one event is always delivered, and
    // the flag tells the consumer more remained.
    let page = store.replay(&session_id, 0, 100, 1).expect("replay reads");
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].seq, 1);
    assert!(page.truncated);

    // Caught-up consumers see an empty, untruncated page.
    let page = store
        .replay(&session_id, 5, 100, usize::MAX)
        .expect("replay reads");
    assert!(page.events.is_empty());
    assert!(!page.truncated);
    let _ = std::fs::remove_file(path);
}

#[test]
fn snapshot_tail_returns_only_the_last_run() {
    let path = store_path("session-snapshot");
    let store = RunStore::open(&path).expect("store opens");
    let session_id = register_agent_session(&store, "s-1");
    store
        .append(&session_id, run_started("run-a"))
        .expect("event appends");
    store
        .append(
            &session_id,
            AgentEventKind::MessageDelta {
                run_id: RunId::new("run-a"),
                stream: MessageStream::Assistant,
                text: "old turn".to_owned(),
            },
        )
        .expect("event appends");
    store
        .append(&session_id, run_started("run-b"))
        .expect("event appends");
    store
        .append(
            &session_id,
            AgentEventKind::ToolStarted {
                run_id: RunId::new("run-b"),
                tool: "edit".to_owned(),
                summary: "edit file".to_owned(),
            },
        )
        .expect("event appends");

    let snapshot = store
        .snapshot(&session_id)
        .expect("snapshot reads")
        .expect("registered session has a snapshot");
    assert_eq!(snapshot.status, SessionStatus::Idle);
    assert_eq!(snapshot.selection.model, "model-a");
    assert_eq!(snapshot.selection.effort, Some(Effort::High));
    assert_eq!(snapshot.cwd, PathBuf::from("/repo"));
    assert_eq!(snapshot.head_seq, 4);
    // The tail is the last turn only: run-b's events, never run-a's.
    assert_eq!(snapshot.tail.len(), 2);
    assert_eq!(snapshot.tail[0].kind, run_started("run-b"));
    assert!(matches!(
        snapshot.tail[1].kind,
        AgentEventKind::ToolStarted { .. }
    ));
    assert!(
        snapshot
            .tail
            .iter()
            .all(|event| event.session_id == session_id)
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn shutdown_marks_in_flight_sessions_interrupted() {
    let path = store_path("session-interrupted");
    let store = RunStore::open(&path).expect("store opens");
    let working = register_agent_session(&store, "s-working");
    let idle = register_agent_session(&store, "s-idle");
    store
        .append(&working, run_started("run-a"))
        .expect("event appends");
    store
        .append(
            &working,
            AgentEventKind::SessionStatus {
                status: SessionStatus::Working,
            },
        )
        .expect("event appends");

    let marked = store
        .mark_interrupted_on_shutdown()
        .expect("shutdown reconcile runs");
    assert_eq!(marked, vec![working.clone()]);

    // The interruption is recorded as a fact in the log and projected into
    // the read model.
    let snapshot = store
        .snapshot(&working)
        .expect("snapshot reads")
        .expect("session exists");
    assert_eq!(snapshot.status, SessionStatus::Interrupted);
    let page = store
        .replay(&working, 0, 100, usize::MAX)
        .expect("replay reads");
    assert_eq!(
        page.events.last().map(|event| &event.kind),
        Some(&AgentEventKind::SessionStatus {
            status: SessionStatus::Interrupted,
        }),
        "the mark lands in the Session Event Log as a fact"
    );

    // Idle sessions are untouched, and a second sweep is a no-op.
    let snapshot = store
        .snapshot(&idle)
        .expect("snapshot reads")
        .expect("session exists");
    assert_eq!(snapshot.status, SessionStatus::Idle);
    assert!(
        store
            .mark_interrupted_on_shutdown()
            .expect("reconcile reruns")
            .is_empty()
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn pending_approvals_survive_in_the_log_until_resolved() {
    let path = store_path("session-approvals");
    let store = RunStore::open(&path).expect("store opens");
    let session_id = register_agent_session(&store, "s-1");
    store
        .append(&session_id, run_started("run-a"))
        .expect("event appends");
    let request = ApprovalRequest {
        kind: ApprovalKind::ToolPermission,
        title: "edit file".to_owned(),
        detail: "src/main.rs".to_owned(),
        options: vec![ApprovalOption {
            id: "allow".to_owned(),
            label: "Allow".to_owned(),
        }],
        requires_confirm: false,
    };
    for request_id in ["req-1", "req-2"] {
        store
            .append(
                &session_id,
                AgentEventKind::ApprovalRequested {
                    run_id: RunId::new("run-a"),
                    request_id: RequestId::new(request_id),
                    request: request.clone(),
                },
            )
            .expect("event appends");
    }
    store
        .append(
            &session_id,
            AgentEventKind::ApprovalResolved {
                request_id: RequestId::new("req-1"),
                decision: ApprovalDecision::OptionId("allow".to_owned()),
                answered_by: "dashboard".to_owned(),
            },
        )
        .expect("event appends");

    let snapshot = store
        .snapshot(&session_id)
        .expect("snapshot reads")
        .expect("session exists");
    assert_eq!(snapshot.pending_approvals.len(), 1);
    assert_eq!(
        snapshot.pending_approvals[0].request_id,
        RequestId::new("req-2"),
        "the resolved request leaves the pending set"
    );
    assert_eq!(snapshot.pending_approvals[0].request, request);
    let _ = std::fs::remove_file(path);
}

#[test]
fn resume_cursor_roundtrips_for_startup_reconcile() {
    let path = store_path("session-cursor");
    let store = RunStore::open(&path).expect("store opens");
    let session_id = register_agent_session(&store, "s-1");
    store
        .record_resume_cursor(&session_id, Some("native-thread-9"))
        .expect("cursor persists");

    let sessions = store.agent_sessions().expect("sessions list");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, session_id);
    assert_eq!(
        sessions[0].resume_cursor.as_deref(),
        Some("native-thread-9")
    );

    let session = store
        .agent_session(&session_id)
        .expect("session reads")
        .expect("session exists");
    assert_eq!(session.integration, IntegrationId::new("claude"));
    assert_eq!(session.resume_cursor.as_deref(), Some("native-thread-9"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn head_seq_reports_the_log_tip() {
    let path = store_path("head-seq");
    let store = RunStore::open(&path).expect("store opens");
    let session_id = register_agent_session(&store, "s-head");

    // No events yet: the head is zero, not an error - `session.create`
    // receipts land before the first fact exists in some flows, and `0`
    // is the contract's no-position marker.
    assert_eq!(store.head_seq(&session_id).expect("head seq reads"), 0);
    store
        .append(&session_id, run_started("run-a"))
        .expect("event appends");
    store
        .append(&session_id, run_started("run-b"))
        .expect("event appends");
    assert_eq!(store.head_seq(&session_id).expect("head seq reads"), 2);
    let _ = std::fs::remove_file(path);
}

#[test]
fn version_three_databases_gain_the_session_log_tables() {
    let path = store_path("migrate-v3");
    // Seed only the version marker a version-3 store carried; the migration
    // stamps version 4 and the schema batch creates the new tables.
    {
        let connection = rusqlite::Connection::open(&path).expect("seed db opens");
        connection
            .execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                INSERT INTO meta (key, value) VALUES ('schema_version', '3');",
            )
            .expect("seed schema applies");
    }

    let store = RunStore::open(&path).expect("store opens");
    let version: String = store
        .connection
        .lock()
        .expect("run store mutex")
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .expect("schema version reads");
    assert_eq!(
        version,
        SCHEMA_VERSION.to_string(),
        "the migration advances the schema version"
    );

    let session_id = register_agent_session(&store, "s-1");
    let event = store
        .append(&session_id, run_started("run-a"))
        .expect("event appends on migrated db");
    assert_eq!(event.seq, 1);
    let _ = std::fs::remove_file(path);
}
