//! The single response returned for each command.

use crate::{AGENT_RUNTIME_SCHEMA_VERSION, CommandId, Seq, SessionId, SessionSnapshot};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;

/// The closed set of receipt error codes.
///
/// Consumers treat unknown codes as `provider_error`; the set grows only by
/// contract version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptCode {
    Unauthorized,
    UnknownSession,
    AlreadyResolved,
    InvalidSelection,
    InvalidState,
    ProviderError,
    Unsupported,
}

impl ReceiptCode {
    /// The stable wire spelling for the error category.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::UnknownSession => "unknown_session",
            Self::AlreadyResolved => "already_resolved",
            Self::InvalidSelection => "invalid_selection",
            Self::InvalidState => "invalid_state",
            Self::ProviderError => "provider_error",
            Self::Unsupported => "unsupported",
        }
    }

    /// Parse the serialized spelling written by [`ReceiptCode::as_str`].
    /// Unknown spellings map to `ProviderError`, matching the consumer rule.
    pub fn parse(value: &str) -> Self {
        match value {
            "unauthorized" => Self::Unauthorized,
            "unknown_session" => Self::UnknownSession,
            "already_resolved" => Self::AlreadyResolved,
            "invalid_selection" => Self::InvalidSelection,
            "invalid_state" => Self::InvalidState,
            "unsupported" => Self::Unsupported,
            _ => Self::ProviderError,
        }
    }
}

impl fmt::Display for ReceiptCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The runtime's answer to one [`AgentCommand`](crate::AgentCommand).
///
/// `ok` discriminates the two wire shapes: a success carries the event-log
/// `seq` plus an optional session and snapshot; a failure carries a closed
/// [`ReceiptCode`] and message.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Receipt {
    pub schema_version: u32,
    pub command_id: CommandId,
    pub ok: bool,
    #[serde(flatten)]
    pub outcome: ReceiptOutcome,
}

/// The outcome half of a [`Receipt`], flattened beside the `ok` flag.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ReceiptOutcome {
    Ok {
        seq: Seq,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snapshot: Option<Box<SessionSnapshot>>,
    },
    Err {
        code: ReceiptCode,
        message: String,
    },
}

impl Receipt {
    /// A successful answer. `session_id` and `snapshot` attach the session
    /// context commands such as `session.create` and `session.subscribe`
    /// return.
    pub fn ok(
        command_id: CommandId,
        seq: Seq,
        session_id: Option<SessionId>,
        snapshot: Option<SessionSnapshot>,
    ) -> Self {
        Self {
            schema_version: AGENT_RUNTIME_SCHEMA_VERSION,
            command_id,
            ok: true,
            outcome: ReceiptOutcome::Ok {
                seq,
                session_id,
                snapshot: snapshot.map(Box::new),
            },
        }
    }

    /// A failed answer with one closed [`ReceiptCode`].
    pub fn err(command_id: CommandId, code: ReceiptCode, message: impl Into<String>) -> Self {
        Self {
            schema_version: AGENT_RUNTIME_SCHEMA_VERSION,
            command_id,
            ok: false,
            outcome: ReceiptOutcome::Err {
                code,
                message: message.into(),
            },
        }
    }
}

/// A serializable failure at the adapter and facade boundary.
///
/// Codes come from the same closed set receipts report, so an adapter error
/// maps one-to-one onto the command's [`Receipt`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentRuntimeError {
    pub schema_version: u32,
    pub code: ReceiptCode,
    pub message: String,
}

impl AgentRuntimeError {
    pub fn new(code: ReceiptCode, message: impl Into<String>) -> Self {
        Self {
            schema_version: AGENT_RUNTIME_SCHEMA_VERSION,
            code,
            message: message.into(),
        }
    }

    pub fn provider_error(message: impl Into<String>) -> Self {
        Self::new(ReceiptCode::ProviderError, message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ReceiptCode::Unsupported, message)
    }
}

impl fmt::Display for AgentRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for AgentRuntimeError {}
