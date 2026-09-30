//! Host-to-runtime commands.
//!
//! Every command carries a `command_id`; the runtime returns exactly one
//! [`Receipt`](crate::Receipt) per command, making retries idempotent even
//! when delivery is ambiguous.

use crate::{
    AccessMode, ApprovalDecision, CheckpointId, CommandId, IntegrationId, ModelSelection,
    RequestId, RunId, Seq, SessionId, UserInput,
};
use serde::Serialize;
use std::path::PathBuf;

/// One command submitted by a Host Application to the runtime.
///
/// The `type` tag spellings are the stable contract surface; consumers must
/// not silently rename them.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum AgentCommand {
    /// Create an Agent Session bound to one Provider Integration.
    #[serde(rename = "session.create")]
    SessionCreate {
        command_id: CommandId,
        cwd: PathBuf,
        selection: ModelSelection,
        access: AccessMode,
    },
    /// Attach to an existing session and replay events after `last_seen_seq`.
    #[serde(rename = "session.subscribe")]
    SessionSubscribe {
        command_id: CommandId,
        session_id: SessionId,
        last_seen_seq: Seq,
    },
    /// List known sessions.
    #[serde(rename = "session.list")]
    SessionList { command_id: CommandId },
    /// Close one session.
    #[serde(rename = "session.close")]
    SessionClose {
        command_id: CommandId,
        session_id: SessionId,
    },
    /// Start one Agent Run in a session.
    #[serde(rename = "run.start")]
    RunStart {
        command_id: CommandId,
        session_id: SessionId,
        input: UserInput,
    },
    /// Request cancellation of one in-flight Agent Run.
    #[serde(rename = "run.cancel")]
    RunCancel {
        command_id: CommandId,
        session_id: SessionId,
        run_id: RunId,
    },
    /// Answer one pending Approval Request. The first answer wins; later
    /// answers receive `already_resolved`.
    #[serde(rename = "approval.answer")]
    ApprovalAnswer {
        command_id: CommandId,
        session_id: SessionId,
        request_id: RequestId,
        decision: ApprovalDecision,
    },
    /// Change the session's model selection. The runtime resolves the
    /// selection first; one that is not `ready` fails with
    /// `invalid_selection`.
    #[serde(rename = "model.select")]
    ModelSelect {
        command_id: CommandId,
        session_id: SessionId,
        selection: ModelSelection,
    },
    /// Reset the session workspace to one recorded Checkpoint. Explicit
    /// because it discards working state.
    #[serde(rename = "checkpoint.restore")]
    CheckpointRestore {
        command_id: CommandId,
        session_id: SessionId,
        checkpoint_id: CheckpointId,
    },
    /// List the registered Provider Integrations.
    #[serde(rename = "integrations.list")]
    IntegrationsList { command_id: CommandId },
    /// List one integration's merged model descriptors.
    #[serde(rename = "models.list")]
    ModelsList {
        command_id: CommandId,
        integration_id: IntegrationId,
    },
}

impl AgentCommand {
    /// The id the matching [`Receipt`](crate::Receipt) echoes.
    pub const fn command_id(&self) -> &CommandId {
        match self {
            Self::SessionCreate { command_id, .. }
            | Self::SessionSubscribe { command_id, .. }
            | Self::SessionList { command_id }
            | Self::SessionClose { command_id, .. }
            | Self::RunStart { command_id, .. }
            | Self::RunCancel { command_id, .. }
            | Self::ApprovalAnswer { command_id, .. }
            | Self::ModelSelect { command_id, .. }
            | Self::CheckpointRestore { command_id, .. }
            | Self::IntegrationsList { command_id }
            | Self::ModelsList { command_id, .. } => command_id,
        }
    }
}
