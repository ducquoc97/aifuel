//! The Session Event Log: the durable, per-Agent-Session sequence of typed
//! contract events.
//!
//! Each append stamps a monotonic `seq` scoped to the Agent Session across
//! all of its Agent Runs - unlike the per-run `events` stream - plus a
//! Unix-epoch `ts`. The log powers `session.subscribe` replay, the
//! materialized `SessionSnapshot` read model, and shutdown/startup
//! reconciliation.
//!
//! `agent_sessions` is the persisted projection those appends maintain:
//! current status, selection, cwd, and the provider resume cursor. Replay
//! reads the raw `session_events` rows only.

use super::helpers::invalid_text;
use super::records::{ReplayPage, StoredAgentSession};
use super::{RunStore, RunStoreError};
use crate::run_management::now;
use aifuel_core::{
    AGENT_RUNTIME_SCHEMA_VERSION, AgentEvent, AgentEventKind, CheckpointDescriptor, Effort,
    IntegrationId, ModelSelection, PendingApproval, Seq, SessionId, SessionSnapshot, SessionStatus,
};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Session statuses that mean an Agent Run is in flight. On shutdown and on
/// the post-crash startup reconcile these sessions become `interrupted`.
const IN_FLIGHT_STATUSES: &str = "'working', 'waiting_approval', 'compacting'";

/// The `agent_sessions` read columns, shared by the single-row and listing
/// queries so both decode the same layout.
const AGENT_SESSION_COLUMNS: &str =
    "session_id, integration, model, effort, cwd, status, resume_cursor, owner";

impl RunStore {
    /// Insert the Agent Session row the Session Event Log projects over,
    /// stamped with this store's owner. Called once when `session.create`
    /// is accepted; re-registering keeps the recorded status and
    /// `created_at` while refreshing the selection and owner.
    pub fn record_agent_session(
        &self,
        session_id: &SessionId,
        selection: &ModelSelection,
        cwd: &Path,
    ) -> Result<(), RunStoreError> {
        let connection = self.connection.lock().expect("run store mutex");
        upsert_agent_session(
            &connection,
            session_id,
            &selection.integration_id,
            Some(selection),
            cwd,
            &self.owner,
        )
    }

    /// Re-stamp the session's owner to this store's id when the startup
    /// resume sweep adopts it. The row reads live-owned from then on, so
    /// this runtime's shutdown marks it and other opens leave it alone.
    pub fn claim_agent_session(&self, session_id: &SessionId) -> Result<(), RunStoreError> {
        self.connection.lock().expect("run store mutex").execute(
            "UPDATE agent_sessions SET owner = ?2, updated_at = ?3 WHERE session_id = ?1",
            params![session_id.as_str(), self.owner, now()],
        )?;
        Ok(())
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

    /// Append one typed contract event to the session's log. The log assigns
    /// the per-session monotonic `seq` and the `ts`, and the writer's
    /// immediate transaction keeps concurrent appenders from sharing a
    /// sequence across processes.
    pub fn append(
        &self,
        session_id: &SessionId,
        kind: AgentEventKind,
    ) -> Result<AgentEvent, RunStoreError> {
        let ts = now();
        let mut connection = self.connection.lock().expect("run store mutex");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let event = self.append_event_locked(&transaction, session_id, kind, ts)?;
        transaction.commit()?;
        Ok(event)
    }

    /// Insert the event and update the persisted session projection inside
    /// the caller's transaction.
    fn append_event_locked(
        &self,
        connection: &Connection,
        session_id: &SessionId,
        kind: AgentEventKind,
        ts: f64,
    ) -> Result<AgentEvent, RunStoreError> {
        // The serialized kind is the single source for the tag and run_id
        // columns, so a renamed contract tag cannot drift the indexes.
        let value = serde_json::to_value(&kind)?;
        let tag = value["type"].as_str().unwrap_or_default().to_owned();
        let run_id = value["run_id"].as_str().map(str::to_owned);
        let data = value.to_string();
        let seq = connection
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM session_events WHERE session_id = ?1",
                params![session_id.as_str()],
                |row| row.get::<_, i64>(0),
            )?
            .max(0) as u64;
        connection.execute(
            "INSERT INTO session_events
                (session_id, seq, kind, run_id, created_at, schema_version, data)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session_id.as_str(),
                seq as i64,
                tag,
                run_id,
                ts,
                i64::from(AGENT_RUNTIME_SCHEMA_VERSION),
                data,
            ],
        )?;
        self.project_session_event(connection, session_id, &kind)?;
        Ok(AgentEvent {
            schema_version: AGENT_RUNTIME_SCHEMA_VERSION,
            session_id: session_id.clone(),
            seq,
            ts,
            kind,
        })
    }

    /// Keep the `agent_sessions` projection in step with the fact the event
    /// records. Only explicit contract facts update the projection; events
    /// for sessions without a registered row leave it untouched so stray
    /// events never conjure a session.
    fn project_session_event(
        &self,
        connection: &Connection,
        session_id: &SessionId,
        kind: &AgentEventKind,
    ) -> Result<(), RunStoreError> {
        match kind {
            AgentEventKind::SessionCreated {
                integration_id,
                cwd,
            } => {
                upsert_agent_session(
                    connection,
                    session_id,
                    integration_id,
                    None,
                    cwd,
                    &self.owner,
                )?;
            }
            AgentEventKind::SessionStatus { status } => {
                connection.execute(
                    "UPDATE agent_sessions SET status = ?2, updated_at = ?3 WHERE session_id = ?1",
                    params![session_id.as_str(), status.as_str(), now()],
                )?;
            }
            AgentEventKind::SessionClosed { .. } => {
                connection.execute(
                    "UPDATE agent_sessions SET status = ?2, updated_at = ?3 WHERE session_id = ?1",
                    params![session_id.as_str(), SessionStatus::Closed.as_str(), now()],
                )?;
            }
            AgentEventKind::RunStarted { selection, .. } => {
                connection.execute(
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
            }
            _ => {}
        }
        Ok(())
    }

    /// The newest sequence the Session Event Log assigned for the session;
    /// `0` when no events exist yet.
    pub fn head_seq(&self, session_id: &SessionId) -> Result<Seq, RunStoreError> {
        let connection = self.connection.lock().expect("run store mutex");
        head_seq(&connection, session_id)
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

    /// Read back the events after `after_seq`, oldest first, bounded by both
    /// `max_events` and `max_bytes` of serialized payload. A page always
    /// carries at least one event when one exists; `truncated` marks lag
    /// that exceeded the bounds, the contract's snapshot-fallback signal.
    pub fn replay(
        &self,
        session_id: &SessionId,
        after_seq: Seq,
        max_events: usize,
        max_bytes: usize,
    ) -> Result<ReplayPage, RunStoreError> {
        let connection = self.connection.lock().expect("run store mutex");
        let head_seq = head_seq(&connection, session_id)?;
        let mut statement = connection.prepare(
            "SELECT seq, created_at, schema_version, data FROM session_events
            WHERE session_id = ?1 AND seq > ?2 ORDER BY seq",
        )?;
        let mut rows = statement.query(params![session_id.as_str(), after_seq as i64])?;
        let mut events = Vec::new();
        let mut used = 0usize;
        let mut truncated = false;
        while let Some(row) = rows.next()? {
            if events.len() >= max_events {
                truncated = true;
                break;
            }
            let event = read_session_event(row, session_id)?;
            if !events.is_empty() && used.saturating_add(event.data_len) > max_bytes {
                truncated = true;
                break;
            }
            used = used.saturating_add(event.data_len);
            events.push(event.event);
        }
        Ok(ReplayPage {
            events,
            head_seq,
            truncated,
        })
    }

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

    /// Mark this owner's sessions with an in-flight run `interrupted`,
    /// recording the interruption as a `session.status` fact in each
    /// session's log. Called on graceful shutdown; already interrupted or
    /// idle sessions are left alone, another owner's live sessions are
    /// never touched, and the returned ids are the ones that flipped.
    pub fn mark_interrupted_on_shutdown(&self) -> Result<Vec<SessionId>, RunStoreError> {
        let ts = now();
        let mut connection = self.connection.lock().expect("run store mutex");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session_ids: Vec<SessionId> = {
            let mut statement = transaction.prepare(&format!(
                "SELECT session_id FROM agent_sessions
                WHERE owner = ?1 AND status IN ({IN_FLIGHT_STATUSES}) ORDER BY session_id"
            ))?;
            statement
                .query_map(params![self.owner], |row| {
                    Ok(SessionId::new(row.get::<_, String>(0)?))
                })?
                .collect::<Result<_, _>>()?
        };
        for session_id in &session_ids {
            // The status event carries the projection update, so the mark and
            // its recorded fact stay one atomic write.
            self.append_event_locked(
                &transaction,
                session_id,
                AgentEventKind::SessionStatus {
                    status: SessionStatus::Interrupted,
                },
                ts,
            )?;
        }
        transaction.commit()?;
        Ok(session_ids)
    }

    /// Mark in-flight sessions whose recorded owner is gone as
    /// `interrupted`, the startup-side half of shutdown marking. A live
    /// foreign host's sessions keep their driver and are left alone, so one
    /// runtime's open never interrupts another's work; the returned ids are
    /// the ones that flipped.
    pub fn reconcile_orphaned_sessions(&self) -> Result<Vec<SessionId>, RunStoreError> {
        let ts = now();
        let mut connection = self.connection.lock().expect("run store mutex");
        let orphans: Vec<SessionId> = {
            let mut statement = connection.prepare(&format!(
                "SELECT session_id, owner FROM agent_sessions
                WHERE status IN ({IN_FLIGHT_STATUSES}) ORDER BY session_id"
            ))?;
            statement
                .query_map([], |row| {
                    Ok((
                        SessionId::new(row.get::<_, String>(0)?),
                        row.get::<_, Option<String>>(1)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .filter(|(_, owner)| self.session_owner_dead(owner.as_deref()))
                .map(|(session_id, _)| session_id)
                .collect()
        };
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for session_id in &orphans {
            self.append_event_locked(
                &transaction,
                session_id,
                AgentEventKind::SessionStatus {
                    status: SessionStatus::Interrupted,
                },
                ts,
            )?;
        }
        transaction.commit()?;
        Ok(orphans)
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

/// One stored row decoded into an envelope plus its serialized payload size,
/// which the byte bound accounts for.
struct StoredSessionEvent {
    event: AgentEvent,
    data_len: usize,
}

fn read_session_event(
    row: &Row<'_>,
    session_id: &SessionId,
) -> Result<StoredSessionEvent, RunStoreError> {
    let data: String = row.get(3)?;
    let kind: AgentEventKind = serde_json::from_str(&data).map_err(|_| invalid_text(3))?;
    Ok(StoredSessionEvent {
        data_len: data.len(),
        event: AgentEvent {
            schema_version: row.get::<_, i64>(2)?.max(0) as u32,
            session_id: session_id.clone(),
            seq: row.get::<_, i64>(0)?.max(0) as u64,
            ts: row.get(1)?,
            kind,
        },
    })
}

/// Insert-or-refresh the `agent_sessions` projection row, shared by
/// `record_agent_session` and the `session.created` projection. With
/// `selection`, the selection columns are written and refreshed on
/// conflict; without it they insert as NULL and survive conflicts
/// untouched, so an event-projected row never wipes a recorded selection.
/// Status and `created_at` persist across conflicts either way; `owner`
/// always re-stamps to the writing store.
fn upsert_agent_session(
    connection: &Connection,
    session_id: &SessionId,
    integration: &IntegrationId,
    selection: Option<&ModelSelection>,
    cwd: &Path,
    owner: &str,
) -> Result<(), RunStoreError> {
    let (model, effort) = selection
        .map(|selection| {
            (
                Some(selection.model.as_str()),
                selection.effort.map(Effort::as_str),
            )
        })
        .unwrap_or((None, None));
    let update = if selection.is_some() {
        "integration = excluded.integration,
            model = excluded.model,
            effort = excluded.effort,
            cwd = excluded.cwd,
            owner = excluded.owner,
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
                (session_id, integration, model, effort, cwd, status, owner, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
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
            now(),
        ],
    )?;
    Ok(())
}

fn head_seq(connection: &Connection, session_id: &SessionId) -> Result<Seq, RunStoreError> {
    Ok(connection
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM session_events WHERE session_id = ?1",
            params![session_id.as_str()],
            |row| row.get::<_, i64>(0),
        )?
        .max(0) as u64)
}

fn stored_agent_session(
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
    })
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
