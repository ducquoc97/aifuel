//! Durable per-user Agent Run history shared across providers.
//!
//! One SQLite database holds the allowlisted metadata schema: run and session
//! identifiers, provider context, selected and reported settings, timestamps,
//! workspace associations needed for resume, run state, stable outcome, and
//! content availability. Prompts, answers, diagnostics, and free-form provider
//! errors are never persisted. Event payloads persist only when the owning
//! manager enables content retention.
//!
//! Runs remain owned by their originating process at runtime; the store
//! exposes only terminal history. Rows for owners that exited without
//! completing are reconciled to `failed` with `owner_exited` when another
//! owner opens the database.

use crate::run_management::{event_size, now};
use crate::session_store::{PersistedSession, SessionStore};
use aifuel_core::{MAX_EVENT_BYTES_PER_RUN, RunEvent, RunEventKind, RunState, RunStatus};
use rusqlite::{Connection, OptionalExtension, params};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use helpers::{invalid_text, pid_alive};
use schema::{
    META_SCHEMA, MIGRATION_V1, MIGRATION_V2, SCHEMA, SCHEMA_VERSION, TERMINAL_EVENT_KINDS,
    TERMINAL_STATES,
};

pub use error::RunStoreError;
pub(crate) use records::{CompletedRun, StartedRun, StoredEventPage, StoredRun};

/// History writes are best-effort: an active run must not fail because its
/// metadata could not be persisted, but a silent drop hides a degraded
/// install, so failures are reported on stderr instead.
pub(crate) fn warn_store_write(error: &RunStoreError) {
    eprintln!("aifuel: run history write failed: {error}");
}

/// A shared handle to the per-user run history database.
///
/// Clones share one serialized connection, matching the manager's existing
/// `Mutex` storage pattern. WAL mode allows concurrent readers while worker
/// threads append.
#[derive(Clone)]
pub struct RunStore {
    connection: Arc<Mutex<Connection>>,
    owner_pid: u32,
}

impl RunStore {
    /// Open or create the database, apply the schema, and reconcile rows left
    /// non-terminal by owners that have exited.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RunStoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "busy_timeout", 10_000_i64)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        connection.execute_batch(META_SCHEMA)?;
        connection.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;
        let version: String = connection.query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )?;
        // Versioned migrations run before the current schema batch so legacy
        // tables gain their new columns before `CREATE TABLE IF NOT EXISTS`
        // statements become no-ops. The batch also recreates the indexes the
        // migration drops.
        match version.as_str() {
            "1" => {
                connection.execute_batch(MIGRATION_V1)?;
                connection.execute_batch(MIGRATION_V2)?;
            }
            "2" => connection.execute_batch(MIGRATION_V2)?,
            version if version == SCHEMA_VERSION.to_string() => {}
            version => return Err(RunStoreError::UnsupportedSchema(version.to_owned())),
        }
        connection.execute_batch(SCHEMA)?;
        let store = Self {
            connection: Arc::new(Mutex::new(connection)),
            owner_pid: std::process::id(),
        };
        store.reconcile_orphaned_runs()?;
        Ok(store)
    }

    /// Insert the accepted-run row before the first event is appended.
    pub(crate) fn record_started(&self, run: StartedRun) -> Result<(), RunStoreError> {
        let external_tools = run
            .external_tools
            .map(|tools| serde_json::to_string(&tools))
            .transpose()?;
        self.connection.lock().expect("run store mutex").execute(
            "INSERT INTO runs (
                    run_id, owner_pid, provider, integration, state, created_at,
                    working_directory, integration_version, platform,
                    requested_model, requested_effort,
                    external_tools, output_format, access, timeout_seconds,
                    requested_account, resume
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                run.run_id,
                self.owner_pid,
                run.provider.as_str(),
                run.integration.as_str(),
                RunState::Starting.as_str(),
                run.created_at,
                run.working_directory,
                run.integration_version,
                run.platform,
                run.requested_model,
                run.requested_effort,
                external_tools,
                run.output_format,
                run.access,
                run.timeout_seconds.map(|seconds| seconds as i64),
                run.requested_account,
                run.resume,
            ],
        )?;
        Ok(())
    }

    /// Write the immutable terminal outcome once.
    pub(crate) fn record_completed(
        &self,
        run_id: &str,
        run: CompletedRun,
    ) -> Result<(), RunStoreError> {
        let usage = run
            .usage
            .map(|usage| serde_json::to_string(&usage))
            .transpose()?;
        self.connection.lock().expect("run store mutex").execute(
            "UPDATE runs SET
                    state = ?2, status = ?3, completed_at = ?4,
                    effective_model = ?5, effective_effort = ?6,
                    session_id = ?7, local_session_id = ?8, exit_code = ?9,
                    closed_reason = ?10, reported_account = ?11,
                    content_available = ?12, output_bytes = ?13,
                    diagnostics_bytes = ?14, output_truncated = ?15,
                    diagnostics_truncated = ?16, usage = ?17
                WHERE run_id = ?1",
            params![
                run_id,
                run.state.as_str(),
                run.status.map(RunStatus::as_str),
                run.completed_at,
                run.effective_model,
                run.effective_effort,
                run.session_id,
                run.local_session_id,
                run.exit_code,
                run.closed_reason,
                run.reported_account,
                run.content_available,
                run.output_bytes as i64,
                run.diagnostics_bytes as i64,
                run.output_truncated,
                run.diagnostics_truncated,
                usage,
            ],
        )?;
        Ok(())
    }

    /// Append one ordered event. The payload is content and is written only
    /// when `retain_content` is enabled for the owning manager, and only while
    /// the run's persisted payloads stay under the per-run event budget; past
    /// it, metadata rows keep landing with `NULL` data.
    pub(crate) fn append_event(
        &self,
        event: &RunEvent,
        retain_content: bool,
    ) -> Result<(), RunStoreError> {
        let connection = self.connection.lock().expect("run store mutex");
        let data = event
            .data
            .as_deref()
            .filter(|_| retain_content)
            .filter(|data| self.payload_budget_allows(&connection, &event.run_id, data.len()));
        connection.execute(
            "INSERT OR IGNORE INTO events (run_id, seq, kind, created_at, data)
                VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                event.run_id,
                event.sequence as i64,
                event.kind.as_str(),
                event.created_at,
                data,
            ],
        )?;
        Ok(())
    }

    /// Whether `additional` payload bytes fit the per-run event budget. On a
    /// failed budget read the payload is dropped but the metadata row still
    /// lands.
    fn payload_budget_allows(
        &self,
        connection: &Connection,
        run_id: &str,
        additional: usize,
    ) -> bool {
        let used: i64 = connection
            .query_row(
                "SELECT COALESCE(SUM(LENGTH(data)), 0) FROM events WHERE run_id = ?1",
                params![run_id],
                |row| row.get(0),
            )
            .unwrap_or(i64::MAX);
        (used.max(0) as u64).saturating_add(additional as u64) <= MAX_EVENT_BYTES_PER_RUN as u64
    }

    /// Persist the integration-scoped native session association. Native
    /// session identifiers are only unique per integration, so the key is
    /// composite; the upstream provider is kept as metadata.
    pub(crate) fn upsert_session(
        &self,
        session_id: &str,
        session: &PersistedSession,
    ) -> Result<(), RunStoreError> {
        self.connection.lock().expect("run store mutex").execute(
            "INSERT OR REPLACE INTO sessions
                    (provider, integration, session_id, model, effort, working_directory, updated_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session.provider.as_str(),
                session.integration.as_str(),
                session_id,
                session.model,
                session.effort,
                session.working_directory.display().to_string(),
                now(),
            ],
        )?;
        Ok(())
    }

    /// Return the stored session association. `integration` scopes the
    /// lookup: native session ids are only unique per integration, so an
    /// unscoped query could return a different integration's row on an id
    /// collision.
    pub(crate) fn session(
        &self,
        session_id: &str,
        integration: Option<&aifuel_core::IntegrationId>,
    ) -> Result<Option<PersistedSession>, RunStoreError> {
        self.connection
            .lock()
            .expect("run store mutex")
            .query_row(
                "SELECT provider, integration, model, effort, working_directory FROM sessions
                WHERE session_id = ?1 AND (?2 IS NULL OR integration = ?2)
                ORDER BY updated_at DESC LIMIT 1",
                params![
                    session_id,
                    integration.map(|integration| integration.as_str())
                ],
                |row| {
                    let provider: String = row.get(0)?;
                    let integration: String = row.get(1)?;
                    Ok(PersistedSession {
                        provider: aifuel_core::ProviderId::new(provider),
                        integration: aifuel_core::IntegrationId::new(integration),
                        model: row.get(2)?,
                        effort: row.get(3)?,
                        working_directory: Path::new(&row.get::<_, String>(4)?).to_path_buf(),
                    })
                },
            )
            .optional()
            .map_err(RunStoreError::from)
    }

    /// Import legacy `agent-sessions.json` associations. Existing rows are
    /// refreshed in place; the JSON file remains authoritative until callers
    /// migrate to the database.
    pub fn import_sessions(&self, path: impl AsRef<Path>) -> Result<usize, RunStoreError> {
        let store = SessionStore::load(path.as_ref())
            .map_err(|error| RunStoreError::Legacy(error.to_string()))?;
        let mut imported = 0;
        for (session_id, session) in store.sessions() {
            self.upsert_session(session_id, session)?;
            imported += 1;
        }
        Ok(imported)
    }

    /// Return one terminal run's persisted metadata. Non-terminal rows belong
    /// to live owner processes and are never served to other owners. Terminal
    /// rows owned by a process that is still running also stay hidden: a run
    /// ID is authorized only for its owning process, and orphaned history
    /// opens up only once the owner has exited.
    pub(crate) fn stored_run(&self, run_id: &str) -> Result<Option<StoredRun>, RunStoreError> {
        let connection = self.connection.lock().expect("run store mutex");
        let run = connection
            .query_row(
                &format!(
                    "SELECT run_id, provider, integration, state, status, requested_model,
                        requested_effort, effective_model, effective_effort,
                        external_tools, session_id, local_session_id, exit_code,
                        created_at, completed_at,
                        content_available, output_bytes, diagnostics_bytes,
                        output_truncated, diagnostics_truncated, owner_pid,
                        closed_reason, reported_account, integration_version,
                        platform, usage
                    FROM runs WHERE run_id = ?1 AND state IN ({TERMINAL_STATES})"
                ),
                params![run_id],
                |row| {
                    let provider: String = row.get(1)?;
                    let integration: String = row.get(2)?;
                    let state: String = row.get(3)?;
                    let status: Option<String> = row.get(4)?;
                    let external_tools: Option<String> = row.get(9)?;
                    let usage: Option<String> = row.get(25)?;
                    Ok(StoredRun {
                        run_id: row.get(0)?,
                        provider: aifuel_core::ProviderId::new(provider),
                        integration: aifuel_core::IntegrationId::new(integration),
                        state: RunState::parse(&state).ok_or_else(|| invalid_text(3))?,
                        status: status
                            .as_deref()
                            .map(|status| RunStatus::parse(status).ok_or_else(|| invalid_text(4)))
                            .transpose()?,
                        requested_model: row.get(5)?,
                        requested_effort: row.get(6)?,
                        effective_model: row.get(7)?,
                        effective_effort: row.get(8)?,
                        external_tools: external_tools
                            .map(|tools| {
                                serde_json::from_str::<Vec<String>>(&tools)
                                    .map_err(|_| invalid_text(9))
                            })
                            .transpose()?,
                        session_id: row.get(10)?,
                        local_session_id: row.get(11)?,
                        exit_code: row.get(12)?,
                        created_at: row.get(13)?,
                        completed_at: row.get(14)?,
                        content_available: row.get::<_, i64>(15)? != 0,
                        output_bytes: row.get::<_, i64>(16)?.max(0) as usize,
                        diagnostics_bytes: row.get::<_, i64>(17)?.max(0) as usize,
                        output_truncated: row.get::<_, i64>(18)? != 0,
                        diagnostics_truncated: row.get::<_, i64>(19)? != 0,
                        owner_pid: row.get(20)?,
                        closed_reason: row.get(21)?,
                        reported_account: row.get(22)?,
                        integration_version: row.get(23)?,
                        platform: row.get(24)?,
                        usage: usage
                            .map(|usage| {
                                serde_json::from_str::<aifuel_core::TokenUsage>(&usage)
                                    .map_err(|_| invalid_text(25))
                            })
                            .transpose()?,
                    })
                },
            )
            .optional()?;
        Ok(run.filter(|run| self.owner_visible(run.owner_pid)))
    }

    /// Read one page of events for a terminal run. `after` is the decoded
    /// cursor sequence; every page returns at least one event when available,
    /// matching the in-memory reader.
    pub(crate) fn stored_events(
        &self,
        run_id: &str,
        after: u64,
        page_bytes: usize,
    ) -> Result<Option<StoredEventPage>, RunStoreError> {
        let connection = self.connection.lock().expect("run store mutex");
        let owner_pid: Option<i64> = connection
            .query_row(
                &format!(
                    "SELECT owner_pid FROM runs WHERE run_id = ?1 AND state IN ({TERMINAL_STATES})"
                ),
                params![run_id],
                |row| row.get(0),
            )
            .optional()?;
        if !owner_pid.is_some_and(|owner| self.owner_visible(owner)) {
            return Ok(None);
        }
        let latest_sequence: i64 = connection.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE run_id = ?1",
            params![run_id],
            |row| row.get(0),
        )?;
        let mut statement = connection.prepare(
            "SELECT seq, kind, created_at, data FROM events
            WHERE run_id = ?1 AND seq > ?2 ORDER BY seq",
        )?;
        let mut rows = statement.query(params![run_id, after as i64])?;
        let mut events = Vec::new();
        let mut used = 0usize;
        let mut has_more = false;
        while let Some(row) = rows.next()? {
            let data: Option<String> = row.get(3)?;
            let kind: String = row.get(1)?;
            let event = RunEvent {
                schema_version: aifuel_core::RUN_MANAGEMENT_SCHEMA_VERSION,
                run_id: run_id.to_owned(),
                sequence: row.get::<_, i64>(0)?.max(0) as u64,
                created_at: row.get(2)?,
                kind: RunEventKind::parse(&kind).ok_or_else(|| invalid_text(1))?,
                data,
            };
            let event_bytes = event_size(&event);
            if !events.is_empty() && used.saturating_add(event_bytes) > page_bytes {
                has_more = true;
                break;
            }
            events.push(event);
            used = used.saturating_add(event_bytes);
        }
        Ok(Some(StoredEventPage {
            events,
            latest_sequence: latest_sequence.max(0) as u64,
            has_more,
        }))
    }

    /// A persisted row is visible to this owner only when it was written by
    /// this owner process or by a process that has since exited. Terminal
    /// rows owned by another live process stay hidden, preserving the
    /// owner-local run contract.
    fn owner_visible(&self, owner_pid: i64) -> bool {
        owner_pid == i64::from(self.owner_pid) || (owner_pid > 0 && !pid_alive(owner_pid as u32))
    }

    /// Mark rows left non-terminal by exited owner processes as failed and
    /// append the terminal event their streams never received. An owner id is
    /// its process id; pid reuse can leave a stale row non-terminal until the
    /// recycled pid exits, which is harmless.
    fn reconcile_orphaned_runs(&self) -> Result<(), RunStoreError> {
        let connection = self.connection.lock().expect("run store mutex");
        let owners: Vec<i64> = {
            let mut statement = connection.prepare(&format!(
                "SELECT DISTINCT owner_pid FROM runs
                WHERE owner_pid != ?1 AND state NOT IN ({TERMINAL_STATES})"
            ))?;
            statement
                .query_map(params![self.owner_pid], |row| row.get(0))?
                .collect::<Result<_, _>>()?
        };
        for owner in owners {
            if owner <= 0 || pid_alive(owner as u32) {
                continue;
            }
            let orphaned: Vec<String> = connection
                .prepare(&format!(
                    "UPDATE runs SET state = 'failed', status = 'failed',
                        completed_at = ?1, closed_reason = 'owner_exited'
                    WHERE owner_pid = ?2 AND state NOT IN ({TERMINAL_STATES})
                    RETURNING run_id"
                ))?
                .query_map(params![now(), owner], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            for run_id in orphaned {
                connection.execute(
                    &format!(
                        "INSERT INTO events (run_id, seq, kind, created_at, data)
                        SELECT ?1, COALESCE((
                            SELECT MAX(seq) FROM events WHERE run_id = ?1
                        ), 0) + 1, 'failed', ?2, NULL
                        WHERE NOT EXISTS (
                            SELECT 1 FROM events
                            WHERE run_id = ?1 AND kind IN ({TERMINAL_EVENT_KINDS})
                        )"
                    ),
                    params![run_id, now()],
                )?;
            }
        }
        Ok(())
    }
}

mod error;
mod helpers;
mod records;
mod schema;

#[cfg(test)]
mod tests;
