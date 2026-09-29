//! Command dispatch: the single seam every [`AgentCommand`] flows through.
//!
//! Each command produces exactly one contract [`Receipt`]; commands that
//! return data carry it on the outcome's `payload` for the in-process
//! host. The per-command implementations live in [`crate::commands`].

use crate::runtime::AgentRuntime;
use aifuel_app::StoredAgentSession;
use aifuel_core::{
    AgentCommand, AgentRuntimeError, CommandId, IntegrationSummary, ModelDescriptor, Receipt,
    ReceiptCode,
};
use std::panic::{AssertUnwindSafe, catch_unwind};

/// The in-process result of dispatching one command: the contract
/// [`Receipt`] plus the command's payload where it produces one. The
/// future stdio bridge serializes the payload beside the receipt.
pub struct CommandOutcome {
    pub receipt: Receipt,
    pub payload: CommandPayload,
}

/// The data a command returns beyond its receipt. Listing commands carry
/// their result here; every other command carries [`CommandPayload::None`].
pub enum CommandPayload {
    /// No payload beyond the receipt.
    None,
    /// `integrations.list` result.
    Integrations(Vec<IntegrationSummary>),
    /// `models.list` result.
    Models(Vec<ModelDescriptor>),
    /// `session.list` result: the persisted Agent Session read model.
    Sessions(Vec<StoredAgentSession>),
}

impl CommandOutcome {
    pub(crate) fn ok(receipt: Receipt) -> Self {
        Self {
            receipt,
            payload: CommandPayload::None,
        }
    }

    pub(crate) fn with_payload(receipt: Receipt, payload: CommandPayload) -> Self {
        Self { receipt, payload }
    }

    pub(crate) fn err(
        command_id: CommandId,
        code: ReceiptCode,
        message: impl Into<String>,
    ) -> Self {
        Self::ok(Receipt::err(command_id, code, message))
    }
}

/// Map a store failure to the receipt catch-all code.
pub(crate) fn store_error(error: aifuel_app::RunStoreError) -> AgentRuntimeError {
    AgentRuntimeError::provider_error(format!("the session event log failed: {error}"))
}

impl AgentRuntime {
    /// Dispatch one command and return its outcome.
    ///
    /// This is the single seam every command flows through; it never
    /// panics across the boundary - an internal failure reports as
    /// `provider_error`.
    pub fn dispatch(&self, command: AgentCommand, consumer_id: &str) -> CommandOutcome {
        let command_id = command.command_id().clone();
        match catch_unwind(AssertUnwindSafe(|| self.handle(&command, consumer_id))) {
            Ok(outcome) => outcome,
            Err(_) => CommandOutcome::err(
                command_id,
                ReceiptCode::ProviderError,
                "the command failed inside the runtime",
            ),
        }
    }

    fn handle(&self, command: &AgentCommand, consumer_id: &str) -> CommandOutcome {
        match command {
            AgentCommand::SessionCreate {
                command_id,
                cwd,
                selection,
                access,
            } => self.session_create(command_id.clone(), cwd.clone(), selection.clone(), *access),
            AgentCommand::SessionSubscribe {
                command_id,
                session_id,
                last_seen_seq,
            } => self.session_subscribe(
                command_id.clone(),
                session_id.clone(),
                *last_seen_seq,
                consumer_id,
            ),
            AgentCommand::SessionList { command_id } => self.session_list(command_id.clone()),
            AgentCommand::SessionClose {
                command_id,
                session_id,
            } => self.session_close(command_id.clone(), session_id.clone()),
            AgentCommand::RunStart {
                command_id,
                session_id,
                input,
            } => self.run_start(command_id.clone(), session_id.clone(), input.clone()),
            AgentCommand::RunCancel {
                command_id,
                session_id,
                run_id,
            } => self.run_cancel(command_id.clone(), session_id.clone(), run_id.clone()),
            AgentCommand::ApprovalAnswer {
                command_id,
                session_id,
                request_id,
                decision,
            } => self.approval_answer(
                command_id.clone(),
                session_id.clone(),
                request_id.clone(),
                decision.clone(),
                consumer_id,
            ),
            AgentCommand::ModelSelect {
                command_id,
                session_id,
                selection,
            } => self.model_select(command_id.clone(), session_id.clone(), selection.clone()),
            AgentCommand::CheckpointRestore { command_id, .. } => CommandOutcome::err(
                command_id.clone(),
                ReceiptCode::Unsupported,
                "checkpoints are not implemented at this delivery level",
            ),
            AgentCommand::IntegrationsList { command_id } => CommandOutcome::with_payload(
                Receipt::ok(command_id.clone(), 0, None, None),
                CommandPayload::Integrations(self.registry.integration_summaries()),
            ),
            AgentCommand::ModelsList {
                command_id,
                integration_id,
            } => match self.registry.model_descriptors(integration_id) {
                Ok(models) => CommandOutcome::with_payload(
                    Receipt::ok(command_id.clone(), 0, None, None),
                    CommandPayload::Models(models),
                ),
                Err(error) => CommandOutcome::err(command_id.clone(), error.code, error.message),
            },
        }
    }
}
