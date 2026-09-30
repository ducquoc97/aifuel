//! Command dispatch: the single seam every [`AgentCommand`] flows through.
//!
//! Each command produces exactly one contract [`Receipt`]; commands that
//! return data carry it on the outcome's `payload` for the in-process
//! host. The per-command implementations live in [`crate::commands`].
//!
//! Commands that mutate runtime or provider state deduplicate by
//! `command_id`: the receipt the first execution recorded is replayed
//! verbatim to a retry, so an ambiguous delivery never double-executes.
//! Attach and read commands are naturally idempotent and re-execute so
//! their receipts and payloads stay fresh.

use crate::runtime::AgentRuntime;
use aifuel_app::{StoredAgentSession, warn_store_write};
use aifuel_core::{
    AgentCommand, AgentRuntimeError, CommandId, ConsumerId, IntegrationSummary, ModelDescriptor,
    Receipt, ReceiptCode, Seq, SessionId,
};
use serde::Deserialize;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// The `seq` a receipt carries when the command authored no Session Event
/// Log position: `0` means exactly that - listing and lookup commands
/// answer without writing to any session's log. Contract note: consumers
/// must not treat a `0` seq as a replay cursor.
const NO_SESSION_SEQ: Seq = 0;

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

    /// The command was rejected with an error [`Receipt`]: the outcome
    /// still carries no payload, so it builds through [`Self::ok`]'s
    /// shape on purpose.
    pub(crate) fn rejected(
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

/// The fields of a recorded serialized [`Receipt`] the runtime needs to
/// answer an idempotent retry. Snapshot-carrying commands are never
/// deduplicated, so the field has no mirror here.
#[derive(Deserialize)]
struct RecordedReceipt {
    command_id: CommandId,
    ok: bool,
    #[serde(default)]
    seq: Option<Seq>,
    #[serde(default)]
    session_id: Option<SessionId>,
    #[serde(default)]
    code: Option<ReceiptCode>,
    #[serde(default)]
    message: Option<String>,
}

/// Whether the command mutates runtime or provider state and therefore
/// deduplicates by `command_id`: a retry answers the recorded receipt
/// instead of executing twice. Attach and read commands are naturally
/// idempotent and re-execute so their receipts stay fresh.
fn deduplicated(command: &AgentCommand) -> bool {
    !matches!(
        command,
        AgentCommand::SessionSubscribe { .. }
            | AgentCommand::SessionList { .. }
            | AgentCommand::IntegrationsList { .. }
            | AgentCommand::ModelsList { .. }
    )
}

/// Rebuild the recorded receipt's outcome for an idempotent retry. `None`
/// on a malformed record lets the command re-execute rather than guess.
fn recorded_outcome(receipt_json: &str) -> Option<CommandOutcome> {
    let recorded: RecordedReceipt = serde_json::from_str(receipt_json).ok()?;
    let receipt = if recorded.ok {
        Receipt::ok(
            recorded.command_id,
            recorded.seq.unwrap_or(NO_SESSION_SEQ),
            recorded.session_id,
            None,
        )
    } else {
        Receipt::err(
            recorded.command_id,
            recorded.code.unwrap_or(ReceiptCode::ProviderError),
            recorded.message.unwrap_or_default(),
        )
    };
    Some(CommandOutcome::ok(receipt))
}

impl AgentRuntime {
    /// Dispatch one command and return its outcome.
    ///
    /// This is the single seam every command flows through; it never
    /// panics across the boundary - an internal failure reports as
    /// `provider_error`.
    pub fn dispatch(&self, command: AgentCommand, consumer_id: &ConsumerId) -> CommandOutcome {
        let command_id = command.command_id().clone();
        if deduplicated(&command) {
            match self.store.command_receipt(&command_id) {
                Ok(Some(receipt_json)) => match recorded_outcome(&receipt_json) {
                    Some(outcome) => return outcome,
                    None => {
                        eprintln!(
                            "aifuel: the recorded receipt for command {command_id} could not be read; re-executing"
                        );
                    }
                },
                Ok(None) => {}
                // A dedup read failure must not block the command: the
                // store's best-effort convention warns and continues.
                Err(error) => warn_store_write(&error),
            }
        }
        let outcome = match catch_unwind(AssertUnwindSafe(|| self.handle(&command, consumer_id))) {
            Ok(outcome) => outcome,
            Err(_) => CommandOutcome::rejected(
                command_id.clone(),
                ReceiptCode::ProviderError,
                "the command failed inside the runtime",
            ),
        };
        if deduplicated(&command)
            && let Ok(receipt_json) = serde_json::to_string(&outcome.receipt)
            && let Err(error) = self.store.record_command(&command_id, &receipt_json)
        {
            warn_store_write(&error);
        }
        outcome
    }

    fn handle(&self, command: &AgentCommand, consumer_id: &ConsumerId) -> CommandOutcome {
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
            AgentCommand::CheckpointRestore { command_id, .. } => CommandOutcome::rejected(
                command_id.clone(),
                ReceiptCode::Unsupported,
                "checkpoints are not implemented at this delivery level",
            ),
            AgentCommand::IntegrationsList { command_id } => CommandOutcome::with_payload(
                Receipt::ok(command_id.clone(), NO_SESSION_SEQ, None, None),
                CommandPayload::Integrations(self.registry.integration_summaries()),
            ),
            AgentCommand::ModelsList {
                command_id,
                integration_id,
            } => match self.registry.model_descriptors(integration_id) {
                Ok(models) => CommandOutcome::with_payload(
                    Receipt::ok(command_id.clone(), NO_SESSION_SEQ, None, None),
                    CommandPayload::Models(models),
                ),
                Err(error) => {
                    CommandOutcome::rejected(command_id.clone(), error.code, error.message)
                }
            },
        }
    }
}
