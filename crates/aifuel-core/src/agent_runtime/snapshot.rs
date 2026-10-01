//! The materialized read model returned by `session.subscribe` and carried
//! by successful receipts.

use crate::{
    AgentEvent, ApprovalRequest, CheckpointId, ModelSelection, RequestId, RunId, Seq, SessionId,
    SessionStatus,
};
use serde::Serialize;
use std::path::PathBuf;

/// A pending Approval Request surfaced by a [`SessionSnapshot`].
///
/// Pending requests are durable in the Session Event Log: they survive
/// consumer disconnects and stay listed until resolved.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PendingApproval {
    pub request_id: RequestId,
    pub run_id: RunId,
    pub request: ApprovalRequest,
}

/// One recorded Checkpoint: the hidden git ref taken at the end of a
/// workspace-mutating Agent Run, enabling diff and restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckpointDescriptor {
    pub checkpoint_id: CheckpointId,
    pub run_id: RunId,
    /// The diffstat text recorded by `checkpoint.created`.
    pub diffstat: String,
}

/// The materialized read model of one Agent Session.
///
/// `session.subscribe` returns this snapshot plus the events after the
/// consumer's `last_seen_seq`; when the lag exceeds the runtime's replay
/// bounds, the runtime skips replay and returns a fresh snapshot instead.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionSnapshot {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub status: SessionStatus,
    /// The session's current model selection.
    pub selection: ModelSelection,
    pub cwd: PathBuf,
    /// Approval Requests raised and not yet resolved.
    pub pending_approvals: Vec<PendingApproval>,
    /// Checkpoints recorded for the session's workspace-mutating runs.
    pub checkpoints: Vec<CheckpointDescriptor>,
    /// The newest sequence assigned by the Session Event Log.
    pub head_seq: Seq,
    /// The transcript items of the most recent Agent Run only: the last
    /// turn, which is what a consumer needs to render current state on
    /// reattach. Older turns are paginated by `seq` on demand.
    pub tail: Vec<AgentEvent>,
}
