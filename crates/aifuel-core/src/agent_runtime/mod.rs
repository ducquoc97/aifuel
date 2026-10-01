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
            resume_cursor: None,
            external_tools: Vec::new(),
        };

        let value = serde_json::to_value(&command).expect("command serializes");

        assert_eq!(value["type"], "session.create");
        assert_eq!(value["command_id"], "cmd-1");
        assert_eq!(value["selection"]["integration_id"], "claude");
        assert_eq!(value["selection"]["effort"], "high");
        assert_eq!(value["access"], "workspace-write");
        assert_eq!(command.command_id(), &CommandId::new("cmd-1"));
    }

    /// `session.create` fields added after the first contract default on
    /// deserialize so older hosts keep working, and serialize only when set.
    #[test]
    fn session_create_additive_fields_default_and_round_trip() {
        let minimal = serde_json::json!({
            "type": "session.create",
            "command_id": "cmd-1",
            "cwd": "/repo",
            "selection": {"integration_id": "claude", "model": "claude-opus-4"},
            "access": "read-only",
        });
        let command: AgentCommand =
            serde_json::from_value(minimal).expect("old shape still decodes");
        match command {
            AgentCommand::SessionCreate {
                resume_cursor,
                external_tools,
                ..
            } => {
                assert_eq!(resume_cursor, None);
                assert!(external_tools.is_empty());
            }
            other => panic!("expected session.create: {other:?}"),
        }

        let command = AgentCommand::SessionCreate {
            command_id: CommandId::new("cmd-2"),
            cwd: "/repo".into(),
            selection: ModelSelection {
                integration_id: crate::IntegrationId::new("claude"),
                model: "claude-opus-4".to_owned(),
                effort: None,
            },
            access: AccessMode::ReadOnly,
            resume_cursor: Some("native-42".to_owned()),
            external_tools: vec!["gateway.search".to_owned()],
        };
        let value = serde_json::to_value(&command).expect("command serializes");
        assert_eq!(value["resume_cursor"], "native-42");
        assert_eq!(
            value["external_tools"],
            serde_json::json!(["gateway.search"])
        );
        let decoded: AgentCommand = serde_json::from_value(value).expect("full shape round-trips");
        assert_eq!(decoded, command);
    }

    /// Approval decisions keep their externally tagged snake_case spellings;
    /// the multi-question and elicitation forms join `option_id`/`text`
    /// without changing them.
    #[test]
    fn approval_decision_variants_keep_stable_spellings() {
        let option_id = serde_json::to_value(ApprovalDecision::OptionId("accept".to_owned()))
            .expect("decision serializes");
        assert_eq!(option_id, serde_json::json!({"option_id": "accept"}));

        let text = serde_json::to_value(ApprovalDecision::Text("yes".to_owned()))
            .expect("decision serializes");
        assert_eq!(text, serde_json::json!({"text": "yes"}));

        let answers = ApprovalDecision::Answers(std::collections::BTreeMap::from([
            ("q1".to_owned(), vec!["a".to_owned()]),
            ("q2".to_owned(), vec!["x".to_owned(), "y".to_owned()]),
        ]));
        let value = serde_json::to_value(&answers).expect("decision serializes");
        assert_eq!(
            value,
            serde_json::json!({"answers": {"q1": ["a"], "q2": ["x", "y"]}})
        );
        let decoded: ApprovalDecision = serde_json::from_value(value).expect("answers decode");
        assert_eq!(decoded, answers);

        let elicitation =
            ApprovalDecision::Elicitation(serde_json::json!({"approved": true, "field": 7}));
        let value = serde_json::to_value(&elicitation).expect("decision serializes");
        assert_eq!(
            value,
            serde_json::json!({"elicitation": {"approved": true, "field": 7}})
        );
        let decoded: ApprovalDecision = serde_json::from_value(value).expect("elicitation decodes");
        assert_eq!(decoded, elicitation);
    }

    /// The legacy rebuild fields on Approval Requests default empty when an
    /// older payload lacks them, and carry the interaction identity through
    /// when present.
    #[test]
    fn approval_request_additive_fields_default_and_round_trip() {
        let minimal = serde_json::json!({
            "kind": "tool_permission",
            "title": "run command",
            "detail": "ls",
            "requires_confirm": false,
        });
        let request: ApprovalRequest =
            serde_json::from_value(minimal).expect("old shape still decodes");
        assert_eq!(request.interaction_kind, None);
        assert!(request.questions.is_empty());
        assert_eq!(request.parameters, None);
        assert_eq!(request.native_method, None);

        let request = ApprovalRequest {
            kind: crate::ApprovalKind::McpElicitation,
            title: "approve tool".to_owned(),
            detail: "server asks".to_owned(),
            options: Vec::new(),
            requires_confirm: false,
            interaction_kind: Some(crate::AgentInteractionKind::McpElicitation),
            questions: vec![crate::AgentInputQuestion {
                id: "q1".to_owned(),
                text: "confirm?".to_owned(),
            }],
            parameters: Some(serde_json::json!({"server": "fs"})),
            native_method: Some("elicitation/create".to_owned()),
        };
        let value = serde_json::to_value(&request).expect("request serializes");
        assert_eq!(value["interaction_kind"], "mcp_elicitation");
        assert_eq!(value["questions"][0]["id"], "q1");
        assert_eq!(value["parameters"]["server"], "fs");
        assert_eq!(value["native_method"], "elicitation/create");
        let decoded: ApprovalRequest = serde_json::from_value(value).expect("request round-trips");
        assert_eq!(decoded, request);
    }

    /// A `run.start` receipt carries the accepted run id so a host can
    /// cancel without racing `run.started`; other successes omit the field.
    #[test]
    fn run_start_receipt_carries_run_id() {
        let receipt = Receipt::ok(CommandId::new("c-1"), 4, Some(SessionId::new("s-1")), None)
            .with_run_id(RunId::new("r-9"));
        let value = serde_json::to_value(&receipt).expect("receipt serializes");
        assert_eq!(value["run_id"], "r-9");
        assert_eq!(value["session_id"], "s-1");

        let plain = Receipt::ok(CommandId::new("c-2"), 5, None, None);
        let value = serde_json::to_value(&plain).expect("receipt serializes");
        assert!(value.get("run_id").is_none(), "run_id stays absent");
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
