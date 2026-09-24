//! Public contracts for explicit Agent Runs.

use crate::{AgentIntegrationInfo, ProviderKey};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
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
#[derive(Clone)]
pub struct RunRequest {
    pub provider: ProviderKey,
    pub model: Option<String>,
    /// Requested model-specific effort. Adapters report an effective value
    /// only when the native provider exposes it.
    pub effort: Option<String>,
    /// Exact AI Fuel Gateway tool names requested for this run. A provider
    /// adapter may accept them only when it can enforce this snapshot.
    pub external_tools: Option<Vec<String>>,
    pub account: Option<String>,
    pub prompt: String,
    pub output: OutputFormat,
    pub working_directory: Option<PathBuf>,
    pub access: AccessMode,
    pub resume: Option<String>,
    pub timeout: Option<Duration>,
    /// Optional owner callback for provider-native questions and approvals.
    pub interaction_handler: Option<Arc<dyn AgentInteractionHandler>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentInteractionKind {
    OrdinaryInput,
    McpElicitation,
    CommandApproval,
    FileChangeApproval,
    PermissionProfileApproval,
}

/// One normalized question for an Agent Run owner to present to a user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentInputQuestion {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct AgentInteractionRequest {
    pub request_id: Value,
    pub method: String,
    pub kind: AgentInteractionKind,
    pub description: String,
    /// Provider-normalized questions safe for an owner to present directly.
    pub questions: Vec<AgentInputQuestion>,
    /// Opaque provider parameters retained for diagnostics and provider-native
    /// response handling. Owners must not parse question wire formats here.
    pub parameters: Value,
    /// Whether this native request asks to widen the permissions already
    /// established by the Agent Run. Providers normalize their wire formats
    /// into this policy signal before the application sees the request.
    pub requires_expanded_access: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionApprovalDecision {
    Accept,
    Decline,
    Cancel,
}

#[derive(Debug, Clone)]
pub enum AgentInteractionResponse {
    Answers(BTreeMap<String, Vec<String>>),
    Elicitation(serde_json::Value),
    Permission(PermissionApprovalDecision),
    PermissionProfile { permissions: Value, scope: String },
}

/// Owner callback used by provider adapters when native App Server protocols
/// request ordinary user input or permission approval.
pub trait AgentInteractionHandler: Send + Sync + std::fmt::Debug {
    fn interact(
        &self,
        request: AgentInteractionRequest,
        cancellation: &RunCancellationToken,
    ) -> Result<AgentInteractionResponse, AgentRunError>;
}

impl fmt::Debug for RunRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RunRequest")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("effort", &self.effort)
            .field("external_tools", &self.external_tools)
            .field("account", &self.account)
            .field("prompt", &"<redacted>")
            .field("output", &self.output)
            .field("working_directory", &self.working_directory)
            .field("access", &self.access)
            .field("resume", &self.resume)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
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
    pub requested_effort: Option<String>,
    pub effective_model: Option<String>,
    pub effective_effort: Option<String>,
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

/// Owner-provided sink for normalized public answer deltas during a run.
pub trait AgentRunOutputHandler: Send + Sync + fmt::Debug {
    fn on_output(&self, delta: &str);
}

/// Provider-specific implementation of the optional Agent Execution capability.
///
/// An adapter handles exactly its `provider()` and never falls back to another
/// provider. Implementations own child-process lifetime, captured output, and
/// cancellation for any process they start.
pub trait AgentExecutionAdapter: Send + Sync {
    fn provider(&self) -> ProviderKey;

    /// Return provider-owned setup instructions without inspecting local
    /// credentials or starting a native process.
    fn setup_guidance(&self) -> Option<crate::AgentSetupGuidance> {
        None
    }

    /// Return provider-owned declarations for each independently listed
    /// capability. Declarations are metadata, not evidence of current native
    /// enforcement.
    fn declared_agent_capabilities(
        &self,
    ) -> BTreeMap<crate::AgentCapability, crate::AgentCapabilityEvidence> {
        crate::AgentCapability::ALL
            .into_iter()
            .map(|capability| {
                (
                    capability,
                    crate::AgentCapabilityEvidence {
                        state: crate::CapabilityState::Unknown,
                        reason: format!(
                            "{capability:?} capability is not declared by this adapter"
                        ),
                    },
                )
            })
            .collect()
    }

    /// Return provider-owned presence, version, and capability evidence for
    /// the current local integration context. Implementations that do not
    /// inspect that context report unknown evidence by default.
    fn agent_info(&self) -> AgentIntegrationInfo {
        AgentIntegrationInfo::from_inspection(
            self.provider(),
            crate::AgentPresenceEvidence {
                state: crate::AgentPresenceState::Unknown,
                reason: "the registered adapter does not expose native presence inspection"
                    .to_owned(),
            },
            crate::AgentVersionEvidence {
                version: None,
                reason: "the registered adapter does not expose native version inspection"
                    .to_owned(),
            },
            crate::AgentAuthenticationEvidence {
                state: crate::AgentAuthenticationState::Unknown,
                reason: "authentication is not inspected during listing to avoid reading local credentials or starting an auth flow; use provider setup guidance for manual login and checks".to_owned(),
            },
            self.declared_agent_capabilities(),
        )
        .with_setup_guidance(self.setup_guidance())
    }

    /// Validate capability metadata without starting a provider process.
    fn validate(&self, request: &RunRequest) -> Result<(), AgentRunError> {
        if request.provider != self.provider() {
            return Err(AgentRunError::UnsupportedProvider(request.provider));
        }
        if request.prompt.trim().is_empty() {
            return Err(AgentRunError::InvalidRequest(
                "prompt must not be empty".to_owned(),
            ));
        }
        Ok(())
    }

    fn execute(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
    ) -> Result<RunResult, AgentRunError>;

    /// Execute while reporting normalized public answer deltas to the owner.
    /// Adapters without a live-output protocol retain their ordinary behavior.
    fn execute_with_output_handler(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
        _output_handler: &dyn AgentRunOutputHandler,
    ) -> Result<RunResult, AgentRunError> {
        self.execute(request, cancellation)
    }
}
