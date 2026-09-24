//! Owner-local lifecycle management for explicit Agent Runs.
//!
//! A `RunManager` owns the records, cancellation tokens, event buffers, and
//! worker threads for one CLI invocation or one execution-MCP connection. It
//! does not persist prompts, answers, or provider diagnostics.

#[cfg(any(unix, windows))]
use crate::approval_ipc::{LocalApprovalDecision, LocalApprovalServer};
use crate::content_store::{ContentStore, PersistedContent};
use crate::selection::ExecutionPolicy;
use crate::session_store::{PersistedSession, SessionStore};
use crate::workspace_lock::{WorkspaceLockError, WorkspaceWriteLock};
use aifuel_core::{
    AgentExecutionAdapter, AgentInteractionHandler, AgentInteractionKind, AgentInteractionRequest,
    AgentInteractionResponse, AgentRunError, AgentRunOutputHandler, DEFAULT_EVENT_PAGE_BYTES,
    MAX_ACTIVE_RUNS, MAX_ANSWER_BYTES_PER_RUN, MAX_COMPLETED_CONTENT, MAX_EVENT_BYTES_PER_RUN,
    MAX_EVENT_PAGE_BYTES, MAX_OWNER_CONTENT_BYTES, MAX_RUN_RECORDS, ManagedRun, ManagedRunResult,
    PendingRunInput, PermissionApprovalDecision, RUN_MANAGEMENT_SCHEMA_VERSION, ResolvedRun,
    RunCancellationToken, RunEvent, RunEventKind, RunEvents, RunInputKind, RunManagementError,
    RunManagementErrorCode, RunRequest, RunResult, RunState, RunStatus, StoredSessionSelection,
};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Optional policy applied by an owner before a provider process is started.
///
/// `None` means the CLI's local policy is unrestricted. `Some(empty)` means no
/// repository path is admitted, which is the safe default for execution MCP.
/// Read-only external tools remain disabled unless their exact Gateway names
/// appear in `allowed_read_only_external_tools`, even under unrestricted roots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunManagerPolicy {
    pub allowed_roots: Option<Vec<PathBuf>>,
    pub allowed_read_only_external_tools: std::collections::HashSet<String>,
    pub retain_content: bool,
}

impl RunManagerPolicy {
    pub fn unrestricted() -> Self {
        Self {
            allowed_roots: None,
            allowed_read_only_external_tools: std::collections::HashSet::new(),
            retain_content: false,
        }
    }

    pub fn restricted(allowed_roots: Vec<PathBuf>) -> Self {
        Self {
            allowed_roots: Some(allowed_roots),
            allowed_read_only_external_tools: std::collections::HashSet::new(),
            retain_content: false,
        }
    }
}

/// Convert the persisted local policy to the manager's explicit root policy.
impl From<&ExecutionPolicy> for RunManagerPolicy {
    fn from(policy: &ExecutionPolicy) -> Self {
        Self {
            allowed_roots: Some(policy.allowed_roots.clone()),
            allowed_read_only_external_tools: policy
                .allowed_read_only_external_tools
                .iter()
                .cloned()
                .collect(),
            retain_content: policy.retain_content,
        }
    }
}

/// Inputs accepted by [`RunManager::new`].
///
/// The owned form is the public test/application seam. The static form keeps
/// the compiled provider registry zero-copy at the executable boundary.
pub enum AgentExecutionAdapters {
    Owned(Vec<Arc<dyn AgentExecutionAdapter>>),
    Static(&'static [&'static dyn AgentExecutionAdapter]),
}

impl<T> From<Vec<Arc<T>>> for AgentExecutionAdapters
where
    T: AgentExecutionAdapter + 'static,
{
    fn from(adapters: Vec<Arc<T>>) -> Self {
        Self::Owned(
            adapters
                .into_iter()
                .map(|adapter| adapter as Arc<dyn AgentExecutionAdapter>)
                .collect(),
        )
    }
}

impl From<Vec<Arc<dyn AgentExecutionAdapter>>> for AgentExecutionAdapters {
    fn from(adapters: Vec<Arc<dyn AgentExecutionAdapter>>) -> Self {
        Self::Owned(adapters)
    }
}

impl From<&'static [&'static dyn AgentExecutionAdapter]> for AgentExecutionAdapters {
    fn from(adapters: &'static [&'static dyn AgentExecutionAdapter]) -> Self {
        Self::Static(adapters)
    }
}

#[derive(Clone)]
enum AdapterHandle {
    Owned(Arc<dyn AgentExecutionAdapter>),
    Static(&'static dyn AgentExecutionAdapter),
}

impl AdapterHandle {
    fn provider(&self) -> aifuel_core::ProviderKey {
        match self {
            Self::Owned(adapter) => adapter.provider(),
            Self::Static(adapter) => adapter.provider(),
        }
    }

    fn agent_info(&self) -> aifuel_core::AgentIntegrationInfo {
        match self {
            Self::Owned(adapter) => adapter.agent_info(),
            Self::Static(adapter) => adapter.agent_info(),
        }
    }

    fn validate(&self, request: &RunRequest) -> Result<(), AgentRunError> {
        match self {
            Self::Owned(adapter) => adapter.validate(request),
            Self::Static(adapter) => adapter.validate(request),
        }
    }

    fn execute_with_output_handler(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
        output_handler: &dyn AgentRunOutputHandler,
    ) -> Result<RunResult, AgentRunError> {
        match self {
            Self::Owned(adapter) => {
                adapter.execute_with_output_handler(request, cancellation, output_handler)
            }
            Self::Static(adapter) => {
                adapter.execute_with_output_handler(request, cancellation, output_handler)
            }
        }
    }
}

/// Shared owner-local Agent Run manager.
#[derive(Clone)]
pub struct RunManager {
    inner: Arc<ManagerInner>,
    _owner: Option<Arc<OwnerToken>>,
}

/// One or more public manager handles share this owner token. Its final drop
/// closes the owner connection and cancels all child runs, including when a
/// caller forgets to call `shutdown` explicitly.
struct OwnerToken {
    inner: Weak<ManagerInner>,
}

struct ManagerInner {
    adapters: Vec<AdapterHandle>,
    records: Mutex<HashMap<String, Arc<RunRecord>>>,
    completed_order: Mutex<VecDeque<String>>,
    policy: Mutex<RunManagerPolicy>,
    retained_content_bytes: AtomicUsize,
    sessions: Mutex<HashMap<String, SessionRecord>>,
    session_store: Mutex<Option<SessionStore>>,
    content_store: Mutex<Option<ContentStore>>,
    next_id: AtomicU64,
    shutdown: AtomicBool,
    #[cfg(any(unix, windows))]
    approval_directory: Mutex<Option<PathBuf>>,
    #[cfg(any(unix, windows))]
    approval_server: Mutex<Option<LocalApprovalServer>>,
}

#[derive(Debug, Clone)]
struct SessionRecord {
    provider: aifuel_core::ProviderKey,
    model: Option<String>,
    effort: Option<String>,
    working_directory: PathBuf,
}

struct RunRecord {
    run_id: String,
    provider: aifuel_core::ProviderKey,
    access: aifuel_core::AccessMode,
    requested_model: Option<String>,
    requested_effort: Option<String>,
    external_tools: Option<Vec<String>>,
    created_at: f64,
    deadline: Option<Instant>,
    metadata: Mutex<RunMetadata>,
    pending_input: Mutex<Option<PendingRunInput>>,
    interaction_response: Mutex<Option<AgentInteractionResponse>>,
    interaction_changed: Condvar,
    output_capture: Mutex<OutputCapture>,
    events: Mutex<EventBuffer>,
    cancellation: RunCancellationToken,
    retained_content_bytes: AtomicUsize,
    worker: Mutex<Option<JoinHandle<()>>>,
    workspace_lock: Mutex<Option<WorkspaceWriteLock>>,
}

#[derive(Clone, Default)]
struct OutputCapture {
    text: String,
    observed_bytes: usize,
    truncated: bool,
    streamed: bool,
}

struct RunMetadata {
    state: RunState,
    completed_at: Option<f64>,
    content_available: bool,
    content_persisted: bool,
    result: ResultMetadata,
}

struct ResultMetadata {
    status: Option<RunStatus>,
    effective_model: Option<String>,
    effective_effort: Option<String>,
    local_session_id: Option<String>,
    session_id: Option<String>,
    exit_code: Option<i32>,
    output: Option<String>,
    error: Option<String>,
    diagnostics: Option<String>,
    output_truncated: bool,
    diagnostics_truncated: bool,
    output_bytes: usize,
    diagnostics_bytes: usize,
}

struct EventBuffer {
    events: VecDeque<RunEvent>,
    next_sequence: u64,
    bytes: usize,
    gap: bool,
}

impl Default for EventBuffer {
    fn default() -> Self {
        Self {
            events: VecDeque::new(),
            next_sequence: 1,
            bytes: 0,
            gap: false,
        }
    }
}

mod callbacks;
mod engine;
mod helpers;
mod interaction;
mod manager;
mod observation;
mod records;
mod sessions;
mod workers;

#[cfg(test)]
mod tests;
