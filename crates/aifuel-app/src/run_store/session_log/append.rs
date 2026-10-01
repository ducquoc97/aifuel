//! The Session Event Log append path.
//!
//! Each append stamps the session's monotonic `seq` and `ts` inside an
//! immediate transaction so concurrent appenders never share a sequence,
//! and the `agent_sessions` projection follows the recorded fact in the
//! same write. Approval answers are scrubbed before they persist: the
//! durable row records that the request resolved and by whom, never the
//! answer material itself. The shutdown and startup sweeps mark in-flight
//! sessions `interrupted` through this same append path so the mark and
//! its recorded fact stay one atomic write.

use super::super::{RunStore, RunStoreError};
use super::agent_sessions::upsert_agent_session;
use crate::run_management::now;
use aifuel_core::{
    AGENT_RUNTIME_SCHEMA_VERSION, AgentEvent, AgentEventKind, ApprovalDecision, Effort, SessionId,
    SessionStatus,
};
use rusqlite::{Connection, TransactionBehavior, params};

/// Session statuses that mean an Agent Run is in flight. On shutdown and on
/// the post-crash startup reconcile these sessions become `interrupted`.
const IN_FLIGHT_STATUSES: &str = "'working', 'waiting_approval', 'compacting'";

impl RunStore {
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
        let mut value = serde_json::to_value(&kind)?;
        // An approval answer can carry credential material - free text,
        // structured answers, MCP elicitation content - and answers are
        // never persisted. The durable row records that the request was
        // resolved and by whom; the full decision still reaches live
        // consumers on the returned event.
        if let AgentEventKind::ApprovalResolved { decision, .. } = &kind {
            let scrubbed = match decision {
                ApprovalDecision::OptionId(id) => ApprovalDecision::OptionId(id.clone()),
                ApprovalDecision::Text(_) => ApprovalDecision::Text(String::new()),
                ApprovalDecision::Answers(answers) => ApprovalDecision::Answers(
                    answers
                        .keys()
                        .map(|key| (key.clone(), Vec::new()))
                        .collect(),
                ),
                ApprovalDecision::Elicitation(_) => {
                    ApprovalDecision::Elicitation(serde_json::Value::Null)
                }
            };
            value["decision"] = serde_json::to_value(&scrubbed)?;
        }
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
                    None,
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
}
