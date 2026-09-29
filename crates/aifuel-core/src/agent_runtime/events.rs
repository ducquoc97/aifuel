//! Runtime-to-host events.
//!
//! Every emission is one typed [`AgentEvent`]. The Session Event Log assigns
//! `seq` and `ts` before delivery to any consumer; a stream that ends without
//! a terminal `run.completed` or `error` is a failure.

use crate::{
    AGENT_RUNTIME_SCHEMA_VERSION, ApprovalDecision, ApprovalRequest, CheckpointId, IntegrationId,
    ModelSelection, QuotaSummary, ReceiptCode, RequestId, RunId, Seq, SessionId, TokenUsage,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One typed event emitted for one Agent Session.
///
/// The envelope carries the identity the Session Event Log stamped; `kind`
/// carries the typed payload. This is the contract's shared-header shape:
/// `{ session_id, seq, ts }` plus one tagged variant.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentEvent {
    pub schema_version: u32,
    pub session_id: SessionId,
    /// The monotonic sequence assigned by the Session Event Log.
    pub seq: Seq,
    /// Seconds since the Unix epoch, matching `RunEvent::created_at`.
    pub ts: f64,
    #[serde(flatten)]
    pub kind: AgentEventKind,
}

impl AgentEvent {
    /// Stamp the envelope around one typed payload.
    pub fn new(session_id: SessionId, seq: Seq, ts: f64, kind: AgentEventKind) -> Self {
        Self {
            schema_version: AGENT_RUNTIME_SCHEMA_VERSION,
            session_id,
            seq,
            ts,
            kind,
        }
    }
}

/// The typed payload of one [`AgentEvent`].
///
/// The `type` tag spellings are the stable contract surface; consumers must
/// not silently rename them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AgentEventKind {
    /// A new Agent Session was created.
    #[serde(rename = "session.created")]
    SessionCreated {
        integration_id: IntegrationId,
        cwd: PathBuf,
    },
    /// The session's observable status changed.
    #[serde(rename = "session.status")]
    SessionStatus { status: SessionStatus },
    /// The session closed.
    #[serde(rename = "session.closed")]
    SessionClosed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// One Agent Run started.
    #[serde(rename = "run.started")]
    RunStarted {
        run_id: RunId,
        selection: ModelSelection,
    },
    /// One Agent Run reached a terminal outcome. A stream that ends without
    /// this or `error` is a failure.
    #[serde(rename = "run.completed")]
    RunCompleted {
        run_id: RunId,
        outcome: RunOutcome,
        /// Token accounting reported by the provider, when it reports any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
    },
    /// One streamed answer fragment.
    #[serde(rename = "message.delta")]
    MessageDelta {
        run_id: RunId,
        stream: MessageStream,
        text: String,
    },
    /// One message stream finished.
    #[serde(rename = "message.completed")]
    MessageCompleted {
        run_id: RunId,
        stream: MessageStream,
    },
    /// One provider tool call started.
    #[serde(rename = "tool.started")]
    ToolStarted {
        run_id: RunId,
        tool: String,
        summary: String,
    },
    /// One provider tool call finished.
    #[serde(rename = "tool.completed")]
    ToolCompleted {
        run_id: RunId,
        tool: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<FileDiff>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    /// The agent's task list changed.
    #[serde(rename = "todos.updated")]
    TodosUpdated { run_id: RunId, items: Vec<TodoItem> },
    /// A blocking Approval Request was raised. Pending requests are durable
    /// in the Session Event Log until resolved.
    #[serde(rename = "approval.requested")]
    ApprovalRequested {
        run_id: RunId,
        request_id: RequestId,
        request: ApprovalRequest,
    },
    /// An Approval Request was answered. `answered_by` names the consumer
    /// channel that won, so every consumer's UI stays honest.
    #[serde(rename = "approval.resolved")]
    ApprovalResolved {
        request_id: RequestId,
        decision: ApprovalDecision,
        answered_by: String,
    },
    /// A Checkpoint hidden git ref was recorded for a workspace-mutating run.
    #[serde(rename = "checkpoint.created")]
    CheckpointCreated {
        run_id: RunId,
        checkpoint_id: CheckpointId,
        diffstat: String,
    },
    /// The workspace was reset to one recorded Checkpoint.
    #[serde(rename = "checkpoint.restored")]
    CheckpointRestored { checkpoint_id: CheckpointId },
    /// A Quota Pool observation reported post-run by the integration's
    /// Monitoring Collection Contract.
    #[serde(rename = "quota.observed")]
    QuotaObserved {
        integration_id: IntegrationId,
        quota: QuotaSummary,
    },
    /// A runtime or provider failure fact.
    #[serde(rename = "error")]
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<RunId>,
        code: ReceiptCode,
        message: String,
        retryable: bool,
    },
}

/// The observable status of one Agent Session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Working,
    WaitingApproval,
    Compacting,
    /// Interrupted by shutdown or a lost provider connection. Continuation is
    /// attempted only where the adapter declares `resume`; the interruption
    /// stays recorded as a fact.
    Interrupted,
    Closed,
}

impl SessionStatus {
    /// The stable serialized spelling for this status.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::WaitingApproval => "waiting_approval",
            Self::Compacting => "compacting",
            Self::Interrupted => "interrupted",
            Self::Closed => "closed",
        }
    }

    /// Parse the serialized spelling written by [`SessionStatus::as_str`].
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "idle" => Self::Idle,
            "working" => Self::Working,
            "waiting_approval" => Self::WaitingApproval,
            "compacting" => Self::Compacting,
            "interrupted" => Self::Interrupted,
            "closed" => Self::Closed,
            _ => return None,
        })
    }
}

/// The terminal outcome of one Agent Run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Success,
    Failed,
    Cancelled,
}

impl RunOutcome {
    /// The stable serialized spelling for this outcome.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parse the serialized spelling written by [`RunOutcome::as_str`].
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "success" => Self::Success,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }
}

/// The message stream a `message.*` event belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageStream {
    Assistant,
    Thinking,
}

impl MessageStream {
    /// The stable serialized spelling for this stream.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Assistant => "assistant",
            Self::Thinking => "thinking",
        }
    }

    /// Parse the serialized spelling written by [`MessageStream::as_str`].
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "assistant" => Self::Assistant,
            "thinking" => Self::Thinking,
            _ => return None,
        })
    }
}

/// One item in the agent's reported task list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoItemStatus,
}

/// The state of one [`TodoItem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoItemStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoItemStatus {
    /// The stable serialized spelling for this state.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }

    /// Parse the serialized spelling written by [`TodoItemStatus::as_str`].
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "pending" => Self::Pending,
            "in_progress" => Self::InProgress,
            "completed" => Self::Completed,
            _ => return None,
        })
    }
}

/// The workspace change one tool call produced, when the provider reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: PathBuf,
    /// Unified diff text for the file.
    pub patch: String,
}
