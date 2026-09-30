//! Public values for the portable agent runtime contract.
//!
//! One typed command, event, and receipt contract shared by every provider
//! adapter, the in-process [`AgentAdapter`] boundary, and the durable
//! per-session event log model.  Host Applications embed the runtime through
//! these values; transports, authentication, and UI stay outside them.

mod adapter;
mod commands;
mod descriptors;
mod events;
mod ids;
mod input;
mod models;
mod receipt;
mod snapshot;

pub use adapter::{
    AdapterCapabilities, AgentAdapter, AgentEventStream, AgentSessionHandle, StartOptions,
};
pub use commands::AgentCommand;
pub use descriptors::{IntegrationAuthKind, IntegrationStatus, IntegrationSummary};
pub use events::{
    AgentEvent, AgentEventKind, FileDiff, MessageStream, RunOutcome, SessionStatus, TodoItem,
    TodoItemStatus,
};
pub use ids::{CheckpointId, CommandId, ConsumerId, RequestId, RunId, Seq, SessionId};
pub use input::{
    ApprovalDecision, ApprovalKind, ApprovalOption, ApprovalRequest, Attachment, AttachmentKind,
    UserInput,
};
pub use models::{Effort, ExecutionAvailability, ModelDescriptor, ModelSelection, QuotaSummary};
pub use receipt::{AgentRuntimeError, Receipt, ReceiptCode, ReceiptOutcome};
pub use snapshot::{CheckpointDescriptor, PendingApproval, SessionSnapshot};

/// Schema version for the agent runtime contract.
pub const AGENT_RUNTIME_SCHEMA_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AccessMode;

    /// Command type tags are the stable contract spellings; every command
    /// carries its `command_id` for the matching receipt.
    #[test]
    fn command_serializes_stable_type_tags() {
        let command = AgentCommand::SessionCreate {
            command_id: CommandId::new("cmd-1"),
            cwd: "/repo".into(),
            selection: ModelSelection {
                integration_id: crate::IntegrationId::new("claude"),
                model: "claude-opus-4".to_owned(),
                effort: Some(Effort::High),
            },
            access: AccessMode::WorkspaceWrite,
        };

        let value = serde_json::to_value(&command).expect("command serializes");

        assert_eq!(value["type"], "session.create");
        assert_eq!(value["command_id"], "cmd-1");
        assert_eq!(value["selection"]["integration_id"], "claude");
        assert_eq!(value["selection"]["effort"], "high");
        assert_eq!(value["access"], "workspace-write");
        assert_eq!(command.command_id(), &CommandId::new("cmd-1"));
    }

    /// Every event serializes the envelope fields alongside its tagged
    /// payload, so the Session Event Log assigns `seq` once for all kinds.
    #[test]
    fn event_flattens_envelope_with_tagged_kind() {
        let event = AgentEvent::new(
            SessionId::new("s-1"),
            7,
            1_700_000_000.5,
            AgentEventKind::SessionStatus {
                status: SessionStatus::Working,
            },
        );

        let value = serde_json::to_value(&event).expect("event serializes");

        assert_eq!(value["schema_version"], AGENT_RUNTIME_SCHEMA_VERSION);
        assert_eq!(value["session_id"], "s-1");
        assert_eq!(value["seq"], 7);
        assert_eq!(value["type"], "session.status");
        assert_eq!(value["status"], "working");
    }

    /// Receipts serialize the `ok` discriminator beside either the success
    /// fields or the closed error code, matching the contract's two shapes.
    #[test]
    fn receipt_serializes_ok_and_err_shapes() {
        let ok = Receipt::ok(CommandId::new("c-1"), 3, Some(SessionId::new("s-1")), None);
        let value = serde_json::to_value(&ok).expect("receipt serializes");
        assert_eq!(value["ok"], true);
        assert_eq!(value["command_id"], "c-1");
        assert_eq!(value["seq"], 3);
        assert_eq!(value["session_id"], "s-1");

        let err = Receipt::err(
            CommandId::new("c-2"),
            ReceiptCode::InvalidSelection,
            "selection is not ready",
        );
        let value = serde_json::to_value(&err).expect("receipt serializes");
        assert_eq!(value["ok"], false);
        assert_eq!(value["command_id"], "c-2");
        assert_eq!(value["code"], "invalid_selection");
        assert_eq!(value["message"], "selection is not ready");
    }

    /// Closed contract enums keep stable serialized spellings; consumers
    /// written against one version must not silently rename.
    #[test]
    fn contract_enums_keep_stable_spellings() {
        assert_eq!(SessionStatus::WaitingApproval.as_str(), "waiting_approval");
        assert_eq!(
            SessionStatus::parse("compacting"),
            Some(SessionStatus::Compacting)
        );
        assert_eq!(ExecutionAvailability::NeedsAuth.as_str(), "needs_auth");
        assert_eq!(Effort::parse("max"), Some(Effort::Max));
        assert_eq!(RunOutcome::parse("cancelled"), Some(RunOutcome::Cancelled));
        assert_eq!(
            ReceiptCode::parse("already_resolved"),
            ReceiptCode::AlreadyResolved
        );
        // Unknown spellings degrade to provider_error per the consumer rule.
        assert_eq!(
            ReceiptCode::parse("future_code"),
            ReceiptCode::ProviderError
        );
    }
}
