use super::*;
use crate::test_support::store_path;
use aifuel_core::{
    IntegrationId, ProviderId, RUN_MANAGEMENT_SCHEMA_VERSION, RunEvent, RunEventKind, RunState,
    RunStatus, TokenUsage,
};
use std::path::PathBuf;

fn started_run(run_id: &str) -> StartedRun {
    StartedRun {
        run_id: run_id.to_owned(),
        provider: ProviderId::new("claude"),
        integration: IntegrationId::new("claude"),
        created_at: now(),
        working_directory: Some("/repo".to_owned()),
        requested_model: Some("model-a".to_owned()),
        requested_effort: None,
        external_tools: Some(vec!["docs.search".to_owned()]),
        output_format: Some("text".to_owned()),
        access: Some("read-only".to_owned()),
        timeout_seconds: Some(30),
        requested_account: None,
        resume: None,
        integration_version: Some("1.2.3".to_owned()),
        platform: "linux".to_owned(),
    }
}

fn completed_run() -> CompletedRun {
    CompletedRun {
        state: RunState::Succeeded,
        status: Some(RunStatus::Succeeded),
        completed_at: now(),
        effective_model: Some("model-a".to_owned()),
        effective_effort: None,
        session_id: Some("native-session".to_owned()),
        local_session_id: Some("local-session".to_owned()),
        exit_code: Some(0),
        closed_reason: None,
        reported_account: Some("acct-1".to_owned()),
        usage: Some(TokenUsage {
            input_tokens: Some(120),
            output_tokens: Some(45),
        }),
        content_available: false,
        output_bytes: 12,
        diagnostics_bytes: 0,
        output_truncated: false,
        diagnostics_truncated: false,
    }
}

fn event(run_id: &str, sequence: u64, kind: RunEventKind, data: Option<&str>) -> RunEvent {
    RunEvent {
        schema_version: RUN_MANAGEMENT_SCHEMA_VERSION,
        run_id: run_id.to_owned(),
        sequence,
        created_at: now(),
        kind,
        data: data.map(str::to_owned),
    }
}

#[test]
fn terminal_run_metadata_roundtrips_without_content() {
    let path = store_path("roundtrip");
    let store = RunStore::open(&path).expect("store opens");
    store
        .record_started(started_run("run-1"))
        .expect("run starts");
    store
        .record_completed("run-1", completed_run())
        .expect("run completes");

    let run = store
        .stored_run("run-1")
        .expect("run reads")
        .expect("terminal run is visible");
    assert_eq!(run.provider, ProviderId::new("claude"));
    assert_eq!(run.integration, IntegrationId::new("claude"));
    assert_eq!(run.state, RunState::Succeeded);
    assert_eq!(run.session_id.as_deref(), Some("native-session"));

    let result = run.result();
    assert_eq!(result.status, Some(RunStatus::Succeeded));
    assert_eq!(
        result.account_id.as_deref(),
        Some("acct-1"),
        "reported account context survives the round trip"
    );
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(
        result.usage,
        Some(TokenUsage {
            input_tokens: Some(120),
            output_tokens: Some(45),
        }),
        "provider-reported token accounting survives the round trip"
    );
    assert_eq!(result.output_bytes, 12);
    assert_eq!(result.output, None);
    assert_eq!(result.error, None);
    assert!(!result.content_available);

    let managed = run.managed_run();
    assert_eq!(managed.state, RunState::Succeeded);
    assert_eq!(
        managed.integration_version.as_deref(),
        Some("1.2.3"),
        "the integration version that produced the run survives the round trip"
    );
    assert_eq!(managed.platform.as_deref(), Some("linux"));
    assert_eq!(managed.pending_input, None);
    let _ = std::fs::remove_file(path);
}

#[test]
fn non_terminal_runs_are_never_served() {
    let path = store_path("non-terminal");
    let store = RunStore::open(&path).expect("store opens");
    store
        .record_started(started_run("run-live"))
        .expect("run starts");
    assert!(
        store.stored_run("run-live").expect("run reads").is_none(),
        "an in-flight run row must not leak to other owners"
    );
    assert!(
        store
            .stored_events("run-live", 0, 4096)
            .expect("events read")
            .is_none()
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn event_payloads_persist_only_with_content_retention() {
    let path = store_path("retention");
    let store = RunStore::open(&path).expect("store opens");
    store
        .record_started(started_run("run-1"))
        .expect("run starts");
    store
        .append_event(
            &event("run-1", 1, RunEventKind::Output, Some("answer")),
            false,
        )
        .expect("event appends");
    store
        .append_event(&event("run-1", 2, RunEventKind::Completed, None), false)
        .expect("event appends");
    store
        .record_completed("run-1", completed_run())
        .expect("run completes");

    let page = store
        .stored_events("run-1", 0, 4096)
        .expect("events read")
        .expect("terminal events are visible");
    assert_eq!(page.events.len(), 2);
    assert_eq!(page.events[0].kind, RunEventKind::Output);
    assert_eq!(
        page.events[0].data, None,
        "payloads stay out of the database without retention opt-in"
    );
    assert_eq!(page.latest_sequence, 2);
    assert!(!page.has_more);

    let retained_path = store_path("retained");
    let retained = RunStore::open(&retained_path).expect("store opens");
    retained
        .record_started(started_run("run-1"))
        .expect("run starts");
    retained
        .append_event(
            &event("run-1", 1, RunEventKind::Output, Some("answer")),
            true,
        )
        .expect("event appends");
    retained
        .record_completed("run-1", completed_run())
        .expect("run completes");
    let page = retained
        .stored_events("run-1", 0, 4096)
        .expect("events read")
        .expect("terminal events are visible");
    assert_eq!(page.events[0].data.as_deref(), Some("answer"));
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(retained_path);
}

#[test]
fn event_pages_follow_sequence_and_cursor_limits() {
    let path = store_path("paging");
    let store = RunStore::open(&path).expect("store opens");
    store
        .record_started(started_run("run-1"))
        .expect("run starts");
    for sequence in 1..=5 {
        store
            .append_event(
                &event(
                    "run-1",
                    sequence,
                    RunEventKind::Output,
                    Some(&"x".repeat(200)),
                ),
                true,
            )
            .expect("event appends");
    }
    store
        .record_completed("run-1", completed_run())
        .expect("run completes");

    // Each event measures ~296 bytes; a 400-byte page fits exactly one.
    let first = store
        .stored_events("run-1", 0, 400)
        .expect("first page reads")
        .expect("terminal events are visible");
    assert_eq!(first.events.len(), 1);
    assert_eq!(first.events[0].sequence, 1);
    assert!(first.has_more);

    let second = store
        .stored_events("run-1", first.events[0].sequence, 4096)
        .expect("second page reads")
        .expect("terminal events are visible");
    assert_eq!(second.events.len(), 4);
    assert_eq!(second.events[0].sequence, 2);
    assert!(!second.has_more);
    let _ = std::fs::remove_file(path);
}

#[test]
fn session_keys_are_scoped_by_integration() {
    let path = store_path("sessions");
    let store = RunStore::open(&path).expect("store opens");
    let claude = PersistedSession {
        provider: ProviderId::new("claude"),
        integration: IntegrationId::new("claude"),
        model: Some("claude-model".to_owned()),
        effort: None,
        working_directory: PathBuf::from("/repo-a"),
    };
    let codex = PersistedSession {
        provider: ProviderId::new("codex"),
        integration: IntegrationId::new("codex"),
        model: Some("codex-model".to_owned()),
        effort: Some("high".to_owned()),
        working_directory: PathBuf::from("/repo-b"),
    };
    // Identical native session ids under different integrations must
    // coexist, and an integration-scoped lookup must return that
    // integration's row even when another integration's association is
    // newer.
    store.upsert_session("same-id", &codex).expect("upsert");
    store.upsert_session("same-id", &claude).expect("upsert");
    let found = store
        .session("same-id", Some(&IntegrationId::new("codex")))
        .expect("session reads")
        .expect("session exists");
    assert_eq!(found.integration, IntegrationId::new("codex"));
    assert_eq!(found.model.as_deref(), Some("codex-model"));
    let found = store
        .session("same-id", Some(&IntegrationId::new("claude")))
        .expect("session reads")
        .expect("session exists");
    assert_eq!(found.integration, IntegrationId::new("claude"));
    assert_eq!(found.model.as_deref(), Some("claude-model"));
    // The unscoped read still resolves to the most recent association.
    let found = store
        .session("same-id", None)
        .expect("session reads")
        .expect("session exists");
    assert_eq!(found.integration, IntegrationId::new("claude"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn sessions_sharing_one_provider_route_by_integration() {
    // Two integrations of the same upstream provider produce independent
    // native sessions; an id collision across them must not merge or hide
    // either association.
    let path = store_path("sessions-same-provider");
    let store = RunStore::open(&path).expect("store opens");
    let cli = PersistedSession {
        provider: ProviderId::new("anthropic"),
        integration: IntegrationId::new("anthropic-cli"),
        model: Some("cli-model".to_owned()),
        effort: None,
        working_directory: PathBuf::from("/repo-a"),
    };
    let api = PersistedSession {
        provider: ProviderId::new("anthropic"),
        integration: IntegrationId::new("anthropic-api"),
        model: Some("api-model".to_owned()),
        effort: None,
        working_directory: PathBuf::from("/repo-b"),
    };
    store.upsert_session("same-id", &cli).expect("upsert");
    store.upsert_session("same-id", &api).expect("upsert");

    let found = store
        .session("same-id", Some(&IntegrationId::new("anthropic-cli")))
        .expect("session reads")
        .expect("session exists");
    assert_eq!(found.model.as_deref(), Some("cli-model"));
    assert_eq!(found.provider, ProviderId::new("anthropic"));
    let found = store
        .session("same-id", Some(&IntegrationId::new("anthropic-api")))
        .expect("session reads")
        .expect("session exists");
    assert_eq!(found.model.as_deref(), Some("api-model"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn legacy_session_file_imports_into_the_database() {
    let path = store_path("import");
    let legacy_path = std::env::temp_dir().join(format!(
        "aifuel-legacy-sessions-{}-{}.json",
        std::process::id(),
        now().to_bits()
    ));
    let mut legacy = SessionStore::load(&legacy_path).expect("empty store loads");
    legacy
        .insert(
            "legacy-session".to_owned(),
            PersistedSession {
                provider: ProviderId::new("codex"),
                integration: IntegrationId::new("codex"),
                model: Some("gpt-codex".to_owned()),
                effort: None,
                working_directory: PathBuf::from("/repo"),
            },
        )
        .expect("legacy session writes");

    let store = RunStore::open(&path).expect("store opens");
    assert_eq!(
        store
            .import_sessions(&legacy_path)
            .expect("import succeeds"),
        1
    );
    let session = store
        .session("legacy-session", Some(&IntegrationId::new("codex")))
        .expect("session reads")
        .expect("imported session exists");
    assert_eq!(session.provider, ProviderId::new("codex"));
    assert_eq!(session.integration, IntegrationId::new("codex"));
    assert_eq!(session.model.as_deref(), Some("gpt-codex"));

    // Re-importing is idempotent and does not duplicate rows.
    assert_eq!(store.import_sessions(&legacy_path).expect("re-import"), 1);
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(legacy_path);
}

#[test]
fn live_foreign_owner_terminal_rows_stay_hidden() {
    let path = store_path("foreign-owner");
    let store = RunStore::open(&path).expect("store opens");

    // A terminal row whose owner process is still running is another
    // owner's record: the run id alone must not authorize this owner.
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("probe process spawns");
    store
        .connection
        .lock()
        .expect("run store mutex")
        .execute(
            "INSERT INTO runs (run_id, owner_pid, provider, integration, state, created_at)
            VALUES ('foreign', ?1, 'claude', 'claude', 'succeeded', 0)",
            params![child.id() as i64],
        )
        .expect("foreign row inserts");

    assert!(
        store.stored_run("foreign").expect("run reads").is_none(),
        "a live foreign owner's run must not resolve"
    );
    assert!(
        store
            .stored_events("foreign", 0, 4096)
            .expect("events read")
            .is_none(),
        "a live foreign owner's event stream must not resolve"
    );

    child.kill().expect("probe process stops");
    child.wait().expect("probe process is reaped");
    assert!(
        store.stored_run("foreign").expect("run reads").is_some(),
        "the row becomes orphaned history once the owner exits"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn persisted_payloads_stop_at_the_event_budget() {
    let path = store_path("payload-budget");
    let store = RunStore::open(&path).expect("store opens");
    store
        .record_started(started_run("run-1"))
        .expect("run starts");
    let full = "x".repeat(aifuel_core::MAX_EVENT_BYTES_PER_RUN);
    store
        .append_event(&event("run-1", 1, RunEventKind::Output, Some(&full)), true)
        .expect("event appends");
    store
        .append_event(
            &event("run-1", 2, RunEventKind::Output, Some("overflow")),
            true,
        )
        .expect("event appends");
    store
        .record_completed("run-1", completed_run())
        .expect("run completes");

    let page = store
        .stored_events("run-1", 0, usize::MAX)
        .expect("events read")
        .expect("terminal events are visible");
    assert_eq!(page.events.len(), 2, "metadata rows keep landing");
    assert_eq!(page.events[0].data.as_deref(), Some(full.as_str()));
    assert_eq!(
        page.events[1].data, None,
        "payloads past the per-run budget persist as metadata-only rows"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn reconcile_does_not_duplicate_a_terminal_event() {
    let path = store_path("reconcile-dedup");
    let store = RunStore::open(&path).expect("store opens");

    // An owner that persisted its terminal event but died before the run row
    // closed must not gain a second terminal event on reconciliation.
    let dead_pid = {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("probe process spawns");
        let pid = child.id();
        child.wait().expect("probe process exits");
        pid
    };
    {
        let connection = store.connection.lock().expect("run store mutex");
        connection
            .execute(
                "INSERT INTO runs (run_id, owner_pid, provider, integration, state, created_at)
                VALUES ('half-closed', ?1, 'claude', 'claude', 'running', 0)",
                params![dead_pid as i64],
            )
            .expect("orphan row inserts");
        connection
            .execute(
                "INSERT INTO events (run_id, seq, kind, created_at)
                VALUES ('half-closed', 7, 'completed', 0)",
                [],
            )
            .expect("terminal event inserts");
    }
    drop(store);

    let reopened = RunStore::open(&path).expect("store reopens");
    let page = reopened
        .stored_events("half-closed", 0, 4096)
        .expect("events read")
        .expect("reconciled stream is readable");
    let terminal = page
        .events
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                RunEventKind::Completed
                    | RunEventKind::Failed
                    | RunEventKind::Cancelled
                    | RunEventKind::TimedOut
            )
        })
        .count();
    assert_eq!(terminal, 1, "the stream keeps its single terminal event");
    assert_eq!(page.events[0].sequence, 7);
    let _ = std::fs::remove_file(path);
}

#[test]
fn exited_owners_reconcile_to_failed_on_next_open() {
    let path = store_path("reconcile");
    let store = RunStore::open(&path).expect("store opens");

    // Insert a row owned by a process that has already exited. The child must
    // be reaped, otherwise the zombie still reports as alive.
    let dead_pid = {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("probe process spawns");
        let pid = child.id();
        child.wait().expect("probe process exits");
        pid
    };
    store
        .connection
        .lock()
        .expect("run store mutex")
        .execute(
            "INSERT INTO runs (run_id, owner_pid, provider, integration, state, created_at)
            VALUES ('orphaned', ?1, 'claude', 'claude', 'running', 0)",
            params![dead_pid as i64],
        )
        .expect("orphan row inserts");
    drop(store);

    let reopened = RunStore::open(&path).expect("store reopens");
    let run = reopened
        .stored_run("orphaned")
        .expect("run reads")
        .expect("reconciled run is terminal and visible");
    assert_eq!(run.state, RunState::Failed);
    assert_eq!(run.status, Some(RunStatus::Failed));
    let page = reopened
        .stored_events("orphaned", 0, 4096)
        .expect("events read")
        .expect("reconciled stream is readable");
    assert_eq!(
        page.events.last().map(|event| event.kind),
        Some(RunEventKind::Failed),
        "reconciled streams end with a terminal event"
    );
    let reason: Option<String> = reopened
        .connection
        .lock()
        .expect("run store mutex")
        .query_row(
            "SELECT closed_reason FROM runs WHERE run_id = 'orphaned'",
            [],
            |row| row.get(0),
        )
        .expect("closed reason reads");
    assert_eq!(reason.as_deref(), Some("owner_exited"));

    // This process's own live rows must never be reconciled.
    reopened
        .record_started(started_run("run-live"))
        .expect("run starts");
    let reopened_again = RunStore::open(&path).expect("store reopens");
    assert!(
        reopened_again
            .stored_run("run-live")
            .expect("run reads")
            .is_none(),
        "same-pid live rows stay untouched"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn runs_completed_without_usage_report_none() {
    let path = store_path("no-usage");
    let store = RunStore::open(&path).expect("store opens");
    store
        .record_started(started_run("run-1"))
        .expect("run starts");
    store
        .record_completed(
            "run-1",
            CompletedRun {
                usage: None,
                ..completed_run()
            },
        )
        .expect("run completes");

    let run = store
        .stored_run("run-1")
        .expect("run reads")
        .expect("terminal run is visible");
    assert_eq!(run.usage, None);
    assert_eq!(
        run.result().usage,
        None,
        "a run whose provider reported no accounting must not invent counts"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn version_two_databases_gain_the_usage_column() {
    let path = store_path("migrate-v2");
    // Seed the schema a version-2 store wrote: the runs table without the
    // usage column, marked at schema version 2. Reopening it must apply the
    // versioned migration before the no-op schema batch.
    {
        let connection = rusqlite::Connection::open(&path).expect("seed db opens");
        connection
            .execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                INSERT INTO meta (key, value) VALUES ('schema_version', '2');
                CREATE TABLE runs (
                    run_id TEXT PRIMARY KEY,
                    owner_pid INTEGER NOT NULL,
                    provider TEXT NOT NULL,
                    integration TEXT NOT NULL,
                    state TEXT NOT NULL,
                    status TEXT,
                    created_at REAL NOT NULL,
                    completed_at REAL,
                    working_directory TEXT,
                    integration_version TEXT,
                    platform TEXT,
                    requested_model TEXT,
                    requested_effort TEXT,
                    effective_model TEXT,
                    effective_effort TEXT,
                    external_tools TEXT,
                    output_format TEXT,
                    access TEXT,
                    timeout_seconds INTEGER,
                    requested_account TEXT,
                    reported_account TEXT,
                    resume TEXT,
                    session_id TEXT,
                    local_session_id TEXT,
                    exit_code INTEGER,
                    closed_reason TEXT,
                    content_available INTEGER NOT NULL DEFAULT 0,
                    output_bytes INTEGER NOT NULL DEFAULT 0,
                    diagnostics_bytes INTEGER NOT NULL DEFAULT 0,
                    output_truncated INTEGER NOT NULL DEFAULT 0,
                    diagnostics_truncated INTEGER NOT NULL DEFAULT 0
                );",
            )
            .expect("seed schema applies");
        connection
            .execute(
                "INSERT INTO runs (run_id, owner_pid, provider, integration, state, created_at)
                VALUES ('old-run', ?1, 'claude', 'claude', 'succeeded', 0)",
                params![std::process::id() as i64],
            )
            .expect("seed row inserts");
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
    assert_eq!(version, "3", "the migration advances the schema version");

    // A row written before the column existed reads back as no usage.
    let old = store
        .stored_run("old-run")
        .expect("run reads")
        .expect("migrated run is visible");
    assert_eq!(old.result().usage, None);

    // New completions on the migrated database persist usage like a fresh
    // schema.
    store
        .record_started(started_run("run-1"))
        .expect("run starts");
    store
        .record_completed("run-1", completed_run())
        .expect("run completes");
    let run = store
        .stored_run("run-1")
        .expect("run reads")
        .expect("terminal run is visible");
    assert_eq!(
        run.result().usage,
        Some(TokenUsage {
            input_tokens: Some(120),
            output_tokens: Some(45),
        })
    );
    let _ = std::fs::remove_file(path);
}
