//! The `SessionSnapshot` read model assembled from the log: the projected
//! `agent_sessions` row plus the pending approvals, recorded checkpoints,
//! head sequence, and the newest Agent Run's transcript tail a reattaching
//! consumer renders. Older turns are paginated through [`super::replay`].

use super::super::helpers::invalid_text;
use super::super::{RunStore, RunStoreError};
use super::agent_sessions::stored_agent_session;
use super::replay::{head_seq, read_session_event};
use aifuel_core::{
    AGENT_RUNTIME_SCHEMA_VERSION, AgentEvent, AgentEventKind, CheckpointDescriptor, ModelSelection,
    PendingApproval, SessionId, SessionSnapshot,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeSet;

impl RunStore {
    /// Assemble the materialized read model for one Agent Session. The tail
    /// carries the transcript of the most recent Agent Run only - the last
    /// turn, which is what a consumer renders on reattach. Older turns are
    /// paginated through [`replay`](Self::replay).
    pub fn snapshot(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionSnapshot>, RunStoreError> {
        let connection = self.connection.lock().expect("run store mutex");
        let Some(session) = stored_agent_session(&connection, session_id)? else {
            return Ok(None);
        };
        Ok(Some(SessionSnapshot {
            schema_version: AGENT_RUNTIME_SCHEMA_VERSION,
            session_id: session_id.clone(),
            status: session.status,
            selection: ModelSelection {
                integration_id: session.integration,
                model: session.model.unwrap_or_default(),
                effort: session.effort,
            },
            cwd: session.cwd,
            pending_approvals: pending_approvals(&connection, session_id)?,
            checkpoints: session_checkpoints(&connection, session_id)?,
            head_seq: head_seq(&connection, session_id)?,
            tail: last_run_tail(&connection, session_id)?,
        }))
    }

    /// The Checkpoints the session recorded, oldest first: every
    /// `checkpoint.created` fact as a read-model descriptor. Feeds the
    /// `SessionSnapshot` and the `checkpoint.restore` membership check.
    pub fn session_checkpoints(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<CheckpointDescriptor>, RunStoreError> {
        let connection = self.connection.lock().expect("run store mutex");
        session_checkpoints(&connection, session_id)
    }
}

/// Pending Approval Requests: `approval.requested` facts minus
/// `approval.resolved`, oldest first. They are durable in the log, so they
/// survive consumer disconnects until resolved.
fn pending_approvals(
    connection: &Connection,
    session_id: &SessionId,
) -> Result<Vec<PendingApproval>, RunStoreError> {
    let mut statement = connection.prepare(
        "SELECT data FROM session_events
        WHERE session_id = ?1 AND kind IN ('approval.requested', 'approval.resolved')
        ORDER BY seq",
    )?;
    let mut rows = statement.query(params![session_id.as_str()])?;
    let mut resolved = BTreeSet::new();
    let mut pending = Vec::new();
    while let Some(row) = rows.next()? {
        let data: String = row.get(0)?;
        match serde_json::from_str(&data).map_err(|_| invalid_text(0))? {
            AgentEventKind::ApprovalRequested {
                run_id,
                request_id,
                request,
            } => pending.push(PendingApproval {
                request_id,
                run_id,
                request,
            }),
            AgentEventKind::ApprovalResolved { request_id, .. } => {
                resolved.insert(request_id);
            }
            _ => {}
        }
    }
    pending.retain(|approval| !resolved.contains(&approval.request_id));
    Ok(pending)
}

/// The session's recorded Checkpoints, oldest first: the
/// `checkpoint.created` facts decoded into the snapshot's read-model
/// descriptors.
fn session_checkpoints(
    connection: &Connection,
    session_id: &SessionId,
) -> Result<Vec<CheckpointDescriptor>, RunStoreError> {
    let mut statement = connection.prepare(
        "SELECT data FROM session_events
        WHERE session_id = ?1 AND kind = 'checkpoint.created' ORDER BY seq",
    )?;
    let mut rows = statement.query(params![session_id.as_str()])?;
    let mut checkpoints = Vec::new();
    while let Some(row) = rows.next()? {
        let data: String = row.get(0)?;
        if let AgentEventKind::CheckpointCreated {
            run_id,
            checkpoint_id,
            diffstat,
        } = serde_json::from_str(&data).map_err(|_| invalid_text(0))?
        {
            checkpoints.push(CheckpointDescriptor {
                checkpoint_id,
                run_id,
                diffstat,
            });
        }
    }
    Ok(checkpoints)
}

/// The transcript of the most recent Agent Run: every run-scoped event of
/// the newest `run_id` the log holds, in order.
fn last_run_tail(
    connection: &Connection,
    session_id: &SessionId,
) -> Result<Vec<AgentEvent>, RunStoreError> {
    let Some(run_id) = connection
        .query_row(
            "SELECT run_id FROM session_events
            WHERE session_id = ?1 AND run_id IS NOT NULL
            ORDER BY seq DESC LIMIT 1",
            params![session_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    else {
        return Ok(Vec::new());
    };
    let mut statement = connection.prepare(
        "SELECT seq, created_at, schema_version, data FROM session_events
        WHERE session_id = ?1 AND run_id = ?2 ORDER BY seq",
    )?;
    let mut rows = statement.query(params![session_id.as_str(), run_id])?;
    let mut events = Vec::new();
    while let Some(row) = rows.next()? {
        events.push(read_session_event(row, session_id)?.event);
    }
    Ok(events)
}
