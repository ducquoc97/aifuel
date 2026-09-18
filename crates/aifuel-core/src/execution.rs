//! Public contracts for explicit Agent Runs.

use crate::ProviderKey;
use serde::Serialize;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum AccessMode {
    #[serde(rename = "read-only")]
    ReadOnly,
    #[serde(rename = "workspace-write")]
    WorkspaceWrite,
}

impl AccessMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "read-only" => Ok(Self::ReadOnly),
            "workspace-write" => Ok(Self::WorkspaceWrite),
            _ => Err(format!(
                "invalid access mode {value:?}; expected read-only or workspace-write"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum OutputFormat {
    Text,
    Json,
    Jsonl,
}

impl OutputFormat {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            "jsonl" => Ok(Self::Jsonl),
            _ => Err(format!(
                "invalid output format {value:?}; expected text, json, or jsonl"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ExecutionMode {
    #[serde(rename = "prompt-only")]
    PromptOnly,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Succeeded,
    Failed,
    Timeout,
    Cancelled,
}

/// One explicit request to run a provider CLI through its execution adapter.
#[derive(Debug, Clone)]
pub struct RunRequest {
    pub provider: ProviderKey,
    pub model: Option<String>,
    pub account: Option<String>,
    pub prompt: String,
    pub output: OutputFormat,
    pub working_directory: Option<PathBuf>,
    pub access: AccessMode,
    pub resume: Option<String>,
    pub timeout: Option<Duration>,
}

/// The observed outcome and local metadata for one Agent Run.
#[derive(Debug, Serialize)]
pub struct RunResult {
    pub run_id: String,
    pub local_session_id: String,
    pub session_id: Option<String>,
    pub resumed_from: Option<String>,
    pub provider_id: ProviderKey,
    pub requested_model: Option<String>,
    pub effective_model: Option<String>,
    pub requested_account_id: Option<String>,
    pub account_id: Option<String>,
    pub execution_mode: ExecutionMode,
    pub permission_profile: AccessMode,
    pub status: RunStatus,
    pub exit_code: Option<i32>,
    pub output: String,
    pub error: Option<String>,
    pub diagnostics: Option<String>,
    pub timed_out: bool,
    pub working_directory: PathBuf,
}

/// Failure to validate a request or execute its selected provider.
#[derive(Debug)]
pub enum AgentRunError {
    InvalidRequest(String),
    UnsupportedProvider(ProviderKey),
    Timeout(String),
    Cancelled,
    Io(io::Error),
}

impl fmt::Display for AgentRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) => f.write_str(message),
            Self::UnsupportedProvider(provider) => {
                write!(f, "provider {provider} has no verified agent integration")
            }
            Self::Timeout(message) => f.write_str(message),
            Self::Cancelled => f.write_str("agent run was cancelled"),
            Self::Io(error) => write!(f, "launcher I/O failed: {error}"),
        }
    }
}

impl std::error::Error for AgentRunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for AgentRunError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Shared cancellation request for a synchronous Agent Run.
///
/// The caller may keep a clone and cancel a blocking `AgentRunFacade::execute`
/// call from another thread. The selected adapter owns any provider process it
/// starts and must stop and reap it before returning. A completed result keeps
/// output captured before cancellation. Adapters populate provider session,
/// account, and effective-model fields only when the provider reports them.
#[derive(Debug, Clone, Default)]
pub struct RunCancellationToken(Arc<AtomicBool>);

impl RunCancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Repeated calls have no additional effect.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Provider-specific implementation of the optional Agent Execution capability.
///
/// An adapter handles exactly its `provider()` and never falls back to another
/// provider. Implementations own child-process lifetime, captured output, and
/// cancellation for any process they start.
pub trait AgentExecutionAdapter: Send + Sync {
    fn provider(&self) -> ProviderKey;

    fn execute(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
    ) -> Result<RunResult, AgentRunError>;
}
