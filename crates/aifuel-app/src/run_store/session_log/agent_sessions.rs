//! The `agent_sessions` projection rows: one row per Agent Session carrying
//! the current status, model selection, cwd, resume cursor, and owner.
//!
//! Appends to `session_events` keep these rows in step through
//! [`super::append`]; this module holds the row lifecycle itself - register
//! on `session.create`, owner claim on startup resume, selection and cursor
//! updates, and the reads the snapshot and reconcile sweep share.

use super::super::helpers::invalid_text;
use super::super::records::StoredAgentSession;
use super::super::{RunStore, RunStoreError};
use crate::run_management::now;
use aifuel_core::{Effort, IntegrationId, ModelSelection, SessionId, SessionStatus};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::path::{Path, PathBuf};

/// The `agent_sessions` read columns, shared by the single-row and listing
/// queries so both decode the same layout.
const AGENT_SESSION_COLUMNS: &str =
    "session_id, integration, model, effort, cwd, status, resume_cursor, owner, external_tools";

impl RunStore {
    /// Insert the Agent Session row the Session Event Log projects over,
    /// stamped with this store's owner. Called once when `session.create`
    /// is accepted; re-registering keeps the recorded status and
    /// `created_at` while refreshing the selection and owner. The declared
    /// `external_tools` persist so a startup resume can redeclare the same
    /// exact enforcement instead of continuing on the provider's full tool
    /// surface.
    pub fn record_agent_session(
        &self,
        session_id: &SessionId,
        selection: &ModelSelection,
        cwd: &Path,
        external_tools: &[String],
    ) -> Result<(), RunStoreError> {
        let connection = self.connection.lock().expect("run store mutex");
        upsert_agent_session(
            &connection,
            session_id,
            &selection.integration_id,
            Some(selection),
            cwd,
            &self.owner,
            Some(external_tools),
        )
    }

    /// Re-stamp the session's owner to this store's id when the startup
    /// resume sweep adopts it. The claim is a compare-and-swap on the owner
    /// observed at enumeration: `IS` compares NULL-safe, so the update only
    /// lands while the row still carries that dead owner - a session
    /// another runtime claimed first reports `false` and must not be
    /// attached. The row reads live-owned from then on, so this runtime's
    /// shutdown marks it and other opens leave it alone.
    pub fn claim_agent_session(
        &self,
        session_id: &SessionId,
        expected_owner: Option<&str>,
    ) -> Result<bool, RunStoreError> {
        Ok(self.connection.lock().expect("run store mutex").execute(
            "UPDATE agent_sessions SET owner = ?2, updated_at = ?3
                WHERE session_id = ?1 AND owner IS ?4",
            params![session_id.as_str(), self.owner, now(), expected_owner],
        )? > 0)
    }

    /// Update the session's current model selection (`model.select`). The
    /// projection also follows `run.started` events, so this is only needed
    /// for selection changes outside a run start.
    pub fn record_agent_session_selection(
        &self,
        session_id: &SessionId,
        selection: &ModelSelection,
    ) -> Result<(), RunStoreError> {
        self.connection.lock().expect("run store mutex").execute(
            "UPDATE agent_sessions SET integration = ?2, model = ?3, effort = ?4, updated_at = ?5
            WHERE session_id = ?1",
            params![
                session_id.as_str(),
                selection.integration_id.as_str(),
                selection.model,
                selection.effort.map(Effort::as_str),
                now(),
            ],
        )?;
        Ok(())
    }

    /// Persist the provider resume cursor a resumable adapter reports, so the
    /// startup reconcile can attempt provider-side continuation.
    pub fn record_resume_cursor(
        &self,
        session_id: &SessionId,
        cursor: Option<&str>,
    ) -> Result<(), RunStoreError> {
        self.connection.lock().expect("run store mutex").execute(
            "UPDATE agent_sessions SET resume_cursor = ?2, updated_at = ?3 WHERE session_id = ?1",
            params![session_id.as_str(), cursor, now()],
        )?;
        Ok(())
    }

    /// Read one persisted Agent Session row, or `None` when the session is
    /// not known to the log.
    pub fn agent_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<StoredAgentSession>, RunStoreError> {
        let connection = self.connection.lock().expect("run store mutex");
        stored_agent_session(&connection, session_id)
    }

    /// All persisted Agent Session rows in stable id order, backing
    /// `session.list` and the startup reconcile sweep.
    pub fn agent_sessions(&self) -> Result<Vec<StoredAgentSession>, RunStoreError> {
        let connection = self.connection.lock().expect("run store mutex");
        let mut statement = connection.prepare(&format!(
            "SELECT {AGENT_SESSION_COLUMNS} FROM agent_sessions ORDER BY session_id"
        ))?;
        let sessions = statement
            .query_map([], agent_session_row)?
            .collect::<Result<_, _>>()?;
        Ok(sessions)
    }

    /// Persisted Agent Session rows whose recorded owner is gone, backing
    /// the startup resume sweep: only an orphaned session may be
    /// reattached, so a live foreign host's sessions are never candidates
    /// and two runtimes sharing one database cannot double-attach.
    pub fn orphaned_agent_sessions(&self) -> Result<Vec<StoredAgentSession>, RunStoreError> {
        Ok(self
            .agent_sessions()?
            .into_iter()
            .filter(|session| self.session_owner_dead(session.owner.as_deref()))
            .collect())
    }
}

/// Insert-or-refresh the `agent_sessions` projection row, shared by
/// `record_agent_session` and the `session.created` projection. With
/// `selection`, the selection columns are written and refreshed on
/// conflict; without it they insert as NULL and survive conflicts
/// untouched, so an event-projected row never wipes a recorded selection.
/// `external_tools` travels with the selection: it refreshes only when the
/// selection does. Status and `created_at` persist across conflicts either
/// way; `owner` always re-stamps to the writing store.
pub(super) fn upsert_agent_session(
    connection: &Connection,
    session_id: &SessionId,
    integration: &IntegrationId,
    selection: Option<&ModelSelection>,
    cwd: &Path,
    owner: &str,
    external_tools: Option<&[String]>,
) -> Result<(), RunStoreError> {
    let (model, effort) = selection
        .map(|selection| {
            (
                Some(selection.model.as_str()),
                selection.effort.map(Effort::as_str),
            )
        })
        .unwrap_or((None, None));
    let external_tools = external_tools.map(serde_json::to_string).transpose()?;
    let update = if selection.is_some() {
        "integration = excluded.integration,
            model = excluded.model,
            effort = excluded.effort,
            cwd = excluded.cwd,
            owner = excluded.owner,
            external_tools = excluded.external_tools,
            updated_at = excluded.updated_at"
    } else {
        "integration = excluded.integration,
            cwd = excluded.cwd,
            owner = excluded.owner,
            updated_at = excluded.updated_at"
    };
    connection.execute(
        &format!(
            "INSERT INTO agent_sessions
                (session_id, integration, model, effort, cwd, status, owner, external_tools,
                 created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
            ON CONFLICT(session_id) DO UPDATE SET {update}"
        ),
        params![
            session_id.as_str(),
            integration.as_str(),
            model,
            effort,
            cwd.display().to_string(),
            SessionStatus::Idle.as_str(),
            owner,
            external_tools,
            now(),
        ],
    )?;
    Ok(())
}

pub(super) fn stored_agent_session(
    connection: &Connection,
    session_id: &SessionId,
) -> Result<Option<StoredAgentSession>, RunStoreError> {
    connection
        .query_row(
            &format!("SELECT {AGENT_SESSION_COLUMNS} FROM agent_sessions WHERE session_id = ?1"),
            params![session_id.as_str()],
            agent_session_row,
        )
        .optional()
        .map_err(RunStoreError::from)
}

fn agent_session_row(row: &Row<'_>) -> Result<StoredAgentSession, rusqlite::Error> {
    let effort: Option<String> = row.get(3)?;
    let status: String = row.get(5)?;
    let external_tools: Option<String> = row.get(8)?;
    Ok(StoredAgentSession {
        session_id: SessionId::new(row.get::<_, String>(0)?),
        integration: IntegrationId::new(row.get::<_, String>(1)?),
        model: row.get(2)?,
        effort: effort
            .as_deref()
            .map(|effort| Effort::parse(effort).ok_or_else(|| invalid_text(3)))
            .transpose()?,
        cwd: PathBuf::from(row.get::<_, String>(4)?),
        // Persisted spellings are checked, never best-effort parsed: an
        // unknown status or effort fails the read instead of guessing.
        status: SessionStatus::parse(&status).ok_or_else(|| invalid_text(5))?,
        resume_cursor: row.get(6)?,
        owner: row.get(7)?,
        external_tools: external_tools
            .map(|tools| serde_json::from_str(&tools).map_err(|_| invalid_text(8)))
            .transpose()?
            .unwrap_or_default(),
    })
}
