//! Bounded replay over the raw `session_events` rows.
//!
//! `replay` serves `session.subscribe` catch-up: the events after a
//! sequence, oldest first, bound by count and serialized payload bytes. A
//! page always carries at least one event when one exists, and `truncated`
//! marks lag that exceeded the bounds - the contract's snapshot-fallback
//! signal. [`super::snapshot`] reads the same row shape for the newest
//! run's transcript tail.

use super::super::helpers::invalid_text;
use super::super::records::ReplayPage;
use super::super::{RunStore, RunStoreError};
use aifuel_core::{AgentEvent, AgentEventKind, Seq, SessionId};
use rusqlite::{Connection, Row, params};

impl RunStore {
    /// The newest sequence the Session Event Log assigned for the session;
    /// `0` when no events exist yet.
    pub fn head_seq(&self, session_id: &SessionId) -> Result<Seq, RunStoreError> {
        let connection = self.connection.lock().expect("run store mutex");
        head_seq(&connection, session_id)
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
}

pub(super) fn head_seq(
    connection: &Connection,
    session_id: &SessionId,
) -> Result<Seq, RunStoreError> {
    Ok(connection
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM session_events WHERE session_id = ?1",
            params![session_id.as_str()],
            |row| row.get::<_, i64>(0),
        )?
        .max(0) as u64)
}

/// One stored row decoded into an envelope plus its serialized payload size,
/// which the byte bound accounts for.
pub(super) struct StoredSessionEvent {
    pub(super) event: AgentEvent,
    pub(super) data_len: usize,
}

pub(super) fn read_session_event(
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
