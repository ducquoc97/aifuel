//! Public values for the owner-local Agent Run management contract.
//!
//! The application crate owns the run registry and worker lifecycle.  These
//! values are deliberately kept in the core crate so the CLI and execution
//! MCP endpoint serialize exactly the same contract.

use crate::{
    AccessMode, AgentInputQuestion, AgentInteractionKind, OutputFormat, ProviderKey, RunStatus,
};
use serde::Serialize;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;

/// Schema version for the execution/run-management contract.
pub const RUN_MANAGEMENT_SCHEMA_VERSION: u32 = 1;

/// Maximum number of runs that may be active for one owner.
pub const MAX_ACTIVE_RUNS: usize = 4;
/// Maximum number of accepted run records retained by one owner.
pub const MAX_RUN_RECORDS: usize = 1024;
/// Maximum number of completed results that may retain content for one owner.
pub const MAX_COMPLETED_CONTENT: usize = 32;
/// Maximum bytes retained in one run's event buffer.
pub const MAX_EVENT_BYTES_PER_RUN: usize = 8 * 1024 * 1024;
/// Maximum bytes retained for one run's final answer.
pub const MAX_ANSWER_BYTES_PER_RUN: usize = 8 * 1024 * 1024;
/// Maximum bytes retained for one owner's event and result content.
pub const MAX_OWNER_CONTENT_BYTES: usize = 128 * 1024 * 1024;
/// Default event read page size.
pub const DEFAULT_EVENT_PAGE_BYTES: usize = 256 * 1024;
/// Maximum event read page size.
pub const MAX_EVENT_PAGE_BYTES: usize = 1024 * 1024;

/// The observable lifecycle state of one managed Agent Run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Starting,
    Running,
    WaitingForInput,
    WaitingForApproval,
    Cancelling,
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
}

impl RunState {
    /// Return whether this state cannot change again.
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::TimedOut | Self::Cancelled
        )
    }
}

impl From<RunStatus> for RunState {
    fn from(status: RunStatus) -> Self {
        match status {
            RunStatus::Succeeded => Self::Succeeded,
            RunStatus::Failed => Self::Failed,
            RunStatus::Timeout => Self::TimedOut,
            RunStatus::Cancelled => Self::Cancelled,
        }
    }
}

/// Stable categories returned by the run-management boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunManagementErrorCode {
    InvalidRequest,
    UnsupportedCapability,
    PolicyDenied,
    AgentUnavailable,
    AuthenticationRequired,
    ModelUnavailable,
    ProviderFailed,
    ConnectionTimeout,
    DeadlineExceeded,
    ResourceLimit,
    WriteConflict,
    SessionUnavailable,
    RunNotFound,
    InvalidCursor,
    InputConflict,
    Internal,
}

impl RunManagementErrorCode {
    /// The stable wire spelling for the error category.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::UnsupportedCapability => "unsupported_capability",
            Self::PolicyDenied => "policy_denied",
            Self::AgentUnavailable => "agent_unavailable",
            Self::AuthenticationRequired => "authentication_required",
            Self::ModelUnavailable => "model_unavailable",
            Self::ProviderFailed => "provider_failed",
            Self::ConnectionTimeout => "connection_timeout",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::ResourceLimit => "resource_limit",
            Self::WriteConflict => "write_conflict",
            Self::SessionUnavailable => "session_unavailable",
            Self::RunNotFound => "run_not_found",
            Self::InvalidCursor => "invalid_cursor",
            Self::InputConflict => "input_conflict",
            Self::Internal => "internal",
        }
    }
}

impl fmt::Display for RunManagementErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A serializable, stable error at the public run-management seam.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunManagementError {
    pub schema_version: u32,
    pub code: RunManagementErrorCode,
    pub message: String,
}

impl RunManagementError {
    pub fn new(code: RunManagementErrorCode, message: impl Into<String>) -> Self {
        Self {
            schema_version: RUN_MANAGEMENT_SCHEMA_VERSION,
            code,
            message: message.into(),
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(RunManagementErrorCode::InvalidRequest, message)
    }

    pub fn run_not_found(run_id: &str) -> Self {
        Self::new(
            RunManagementErrorCode::RunNotFound,
            format!("managed run {run_id:?} was not found"),
        )
    }

    pub fn invalid_cursor(message: impl Into<String>) -> Self {
        Self::new(RunManagementErrorCode::InvalidCursor, message)
    }

    pub fn resource_limit(message: impl Into<String>) -> Self {
        Self::new(RunManagementErrorCode::ResourceLimit, message)
    }
}

impl fmt::Display for RunManagementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for RunManagementError {}

/// A metadata-only snapshot of one owner-local Agent Run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ManagedRun {
    pub schema_version: u32,
    pub run_id: String,
    pub state: RunState,
    pub provider: ProviderKey,
    pub requested_model: Option<String>,
    pub requested_effort: Option<String>,
    pub external_tools: Option<Vec<String>>,
    pub created_at: f64,
    pub completed_at: Option<f64>,
    pub content_available: bool,
    pub pending_input: Option<PendingRunInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunInputKind {
    Ordinary,
    Permission,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingRunInput {
    pub input_id: String,
    pub run_id: String,
    pub kind: RunInputKind,
    pub interaction_kind: AgentInteractionKind,
    pub description: String,
    pub native_method: Option<String>,
    /// Typed provider questions for presentation by the owner.
    pub questions: Vec<AgentInputQuestion>,
    pub question_ids: Vec<String>,
    /// Opaque native parameters retained separately for diagnostics and
    /// provider-specific context, not for parsing ordinary question text.
    pub parameters: Option<serde_json::Value>,
    pub requires_expanded_access: bool,
}

/// The safe, serializable result of resolving a request. Prompt content is
/// intentionally omitted from this DTO; resolution validates the prompt but
/// does not echo or retain it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResolvedRun {
    pub schema_version: u32,
    pub provider: ProviderKey,
    pub requested_model: Option<String>,
    pub requested_effort: Option<String>,
    pub external_tools: Option<Vec<String>>,
    pub requested_account: Option<String>,
    pub output: OutputFormat,
    pub working_directory: Option<PathBuf>,
    pub access: AccessMode,
    pub resume: Option<String>,
    pub timeout_seconds: Option<u64>,
}

/// Metadata-only selection remembered for a native Agent Session.
///
/// A missing model or effort means the prior run requested the provider's
/// native default; it does not claim a concrete effective value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoredSessionSelection {
    pub schema_version: u32,
    pub session_id: String,
    pub provider: ProviderKey,
    pub requested_model: Option<String>,
    pub requested_effort: Option<String>,
}

/// The public event categories emitted by the run manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunEventKind {
    Started,
    Running,
    WaitingForInput,
    WaitingForApproval,
    StateChanged,
    Output,
    Diagnostic,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

/// One ordered, public event.  The manager never emits prompts or hidden
/// reasoning as an event payload.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunEvent {
    pub schema_version: u32,
    pub run_id: String,
    pub sequence: u64,
    pub created_at: f64,
    pub kind: RunEventKind,
    pub data: Option<String>,
}

/// One non-consuming page from a run's retained event stream.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunEvents {
    pub events: Vec<RunEvent>,
    pub next_cursor: Option<String>,
    pub gap: bool,
    pub terminal: bool,
}

/// A bounded projection of a provider's final result.
///
/// `output` and `diagnostics` are retained only in memory and are truncated at
/// the documented per-run limits.  The terminal state and stable status stay
/// available even after content is evicted.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ManagedRunResult {
    pub schema_version: u32,
    pub run_id: String,
    pub state: RunState,
    pub provider: ProviderKey,
    pub requested_model: Option<String>,
    pub requested_effort: Option<String>,
    pub effective_model: Option<String>,
    pub effective_effort: Option<String>,
    pub local_session_id: Option<String>,
    pub session_id: Option<String>,
    pub status: Option<RunStatus>,
    pub exit_code: Option<i32>,
    pub output: Option<String>,
    pub error: Option<String>,
    pub diagnostics: Option<String>,
    pub content_available: bool,
    pub output_truncated: bool,
    pub diagnostics_truncated: bool,
    pub output_bytes: usize,
    pub diagnostics_bytes: usize,
}
