//! Owner-local lifecycle management for explicit Agent Runs.
//!
//! A `RunManager` owns the records, cancellation tokens, event buffers, and
//! worker threads for one CLI invocation or one execution-MCP connection. It
//! does not persist prompts, answers, or provider diagnostics.

use crate::content_store::{ContentStore, PersistedContent};
use crate::selection::ExecutionPolicy;
use crate::session_store::{PersistedSession, SessionStore};
use crate::workspace_lock::{WorkspaceLockError, WorkspaceWriteLock};
use aifuel_core::{
    AgentExecutionAdapter, AgentRunError, DEFAULT_EVENT_PAGE_BYTES, MAX_ACTIVE_RUNS,
    MAX_ANSWER_BYTES_PER_RUN, MAX_COMPLETED_CONTENT, MAX_EVENT_BYTES_PER_RUN, MAX_EVENT_PAGE_BYTES,
    MAX_OWNER_CONTENT_BYTES, MAX_RUN_RECORDS, ManagedRun, ManagedRunResult, PendingRunInput,
    RUN_MANAGEMENT_SCHEMA_VERSION, ResolvedRun, RunCancellationToken, RunEvent, RunEventKind,
    RunEvents, RunInputKind, RunManagementError, RunManagementErrorCode, RunRequest, RunResult,
    RunState, RunStatus,
};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

/// Optional policy applied by an owner before a provider process is started.
///
/// `None` means the CLI's local policy is unrestricted. `Some(empty)` means no
/// repository path is admitted, which is the safe default for execution MCP.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunManagerPolicy {
    pub allowed_roots: Option<Vec<PathBuf>>,
    pub retain_content: bool,
}

impl RunManagerPolicy {
    pub const fn unrestricted() -> Self {
        Self {
            allowed_roots: None,
            retain_content: false,
        }
    }

    pub fn restricted(allowed_roots: Vec<PathBuf>) -> Self {
        Self {
            allowed_roots: Some(allowed_roots),
            retain_content: false,
        }
    }
}

/// Convert the persisted local policy to the manager's explicit root policy.
impl From<&ExecutionPolicy> for RunManagerPolicy {
    fn from(policy: &ExecutionPolicy) -> Self {
        Self {
            allowed_roots: Some(policy.allowed_roots.clone()),
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

    fn validate(&self, request: &RunRequest) -> Result<(), AgentRunError> {
        match self {
            Self::Owned(adapter) => adapter.validate(request),
            Self::Static(adapter) => adapter.validate(request),
        }
    }

    fn execute(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
    ) -> Result<RunResult, AgentRunError> {
        match self {
            Self::Owned(adapter) => adapter.execute(request, cancellation),
            Self::Static(adapter) => adapter.execute(request, cancellation),
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
    requested_model: Option<String>,
    requested_effort: Option<String>,
    external_tools: Option<Vec<String>>,
    created_at: f64,
    metadata: Mutex<RunMetadata>,
    pending_input: Mutex<Option<PendingRunInput>>,
    events: Mutex<EventBuffer>,
    cancellation: RunCancellationToken,
    retained_content_bytes: AtomicUsize,
    worker: Mutex<Option<JoinHandle<()>>>,
    workspace_lock: Mutex<Option<WorkspaceWriteLock>>,
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

impl RunManager {
    /// Construct an owner-local manager from owned adapters or the compiled
    /// static provider registry.
    pub fn new(adapters: impl Into<AgentExecutionAdapters>) -> Self {
        let adapters = match adapters.into() {
            AgentExecutionAdapters::Owned(adapters) => {
                adapters.into_iter().map(AdapterHandle::Owned).collect()
            }
            AgentExecutionAdapters::Static(adapters) => adapters
                .iter()
                .copied()
                .map(AdapterHandle::Static)
                .collect(),
        };
        let inner = Arc::new(ManagerInner {
            adapters,
            records: Mutex::new(HashMap::new()),
            completed_order: Mutex::new(VecDeque::new()),
            policy: Mutex::new(RunManagerPolicy::unrestricted()),
            retained_content_bytes: AtomicUsize::new(0),
            sessions: Mutex::new(HashMap::new()),
            session_store: Mutex::new(None),
            content_store: Mutex::new(None),
            next_id: AtomicU64::new(1),
            shutdown: AtomicBool::new(false),
        });
        Self {
            _owner: Some(Arc::new(OwnerToken {
                inner: Arc::downgrade(&inner),
            })),
            inner,
        }
    }

    /// Construct a manager with an explicit root policy.
    pub fn with_policy(
        adapters: impl Into<AgentExecutionAdapters>,
        policy: RunManagerPolicy,
    ) -> Self {
        let manager = Self::new(adapters);
        *manager
            .inner
            .policy
            .lock()
            .expect("run manager policy mutex") = policy;
        manager
    }

    /// Set the canonical workspace roots used by the execution MCP owner.
    /// This is a builder-style operation and must be called before starts.
    pub fn with_allowed_roots(self, allowed_roots: Vec<PathBuf>) -> Self {
        *self.inner.policy.lock().expect("run manager policy mutex") =
            RunManagerPolicy::restricted(allowed_roots);
        self
    }

    /// Attach private metadata-only native session persistence for this owner.
    pub fn with_session_store(self, path: impl Into<PathBuf>) -> Result<Self, String> {
        let store = SessionStore::load(path).map_err(|error| error.to_string())?;
        *self
            .inner
            .session_store
            .lock()
            .expect("session store mutex") = Some(store);
        Ok(self)
    }

    pub fn with_content_store(self, path: impl Into<PathBuf>) -> Result<Self, String> {
        let store = ContentStore::new(path).map_err(|error| error.to_string())?;
        *self
            .inner
            .content_store
            .lock()
            .expect("content store mutex") = Some(store);
        Ok(self)
    }

    /// Set policy from the application selection configuration.
    pub fn with_execution_policy(self, policy: &ExecutionPolicy) -> Self {
        *self.inner.policy.lock().expect("run manager policy mutex") = policy.into();
        self
    }

    /// Return the compiled Agent Integrations owned by this manager. Presence
    /// of an adapter is deliberately separate from Provider Discovery and
    /// account authentication.
    pub fn registered_providers(&self) -> Vec<aifuel_core::ProviderKey> {
        self.inner
            .adapters
            .iter()
            .map(AdapterHandle::provider)
            .collect()
    }

    /// Resolve and validate a request without starting an Agent Run.
    pub fn resolve_run(&self, request: &RunRequest) -> Result<ResolvedRun, RunManagementError> {
        let resolved = self.resolve_request(request)?;
        Ok(ResolvedRun {
            schema_version: RUN_MANAGEMENT_SCHEMA_VERSION,
            provider: resolved.provider,
            requested_model: resolved.model,
            requested_effort: resolved.effort,
            external_tools: resolved.external_tools,
            requested_account: resolved.account,
            output: resolved.output,
            working_directory: resolved.working_directory,
            access: resolved.access,
            resume: resolved.resume,
            timeout_seconds: resolved.timeout.map(|timeout| timeout.as_secs()),
        })
    }

    /// Accept a run and return its metadata snapshot immediately.
    pub fn start_run(&self, request: RunRequest) -> Result<ManagedRun, RunManagementError> {
        if self.inner.shutdown.load(Ordering::Acquire) {
            return Err(RunManagementError::resource_limit(
                "run manager is shutting down",
            ));
        }
        let (request, adapter) = self.resolve_request_with_adapter(&request)?;
        let workspace_lock = if request.access == aifuel_core::AccessMode::WorkspaceWrite {
            request
                .working_directory
                .as_deref()
                .map(WorkspaceWriteLock::acquire)
                .transpose()
                .map_err(|error| {
                    RunManagementError::new(
                        match error {
                            WorkspaceLockError::Busy => RunManagementErrorCode::WriteConflict,
                            WorkspaceLockError::Io(_) => RunManagementErrorCode::Internal,
                        },
                        error.to_string(),
                    )
                })?
        } else {
            None
        };

        let mut records = self.inner.records.lock().expect("run records mutex");
        let active = records
            .values()
            .filter(|record| {
                !record
                    .metadata
                    .lock()
                    .expect("run metadata mutex")
                    .state
                    .is_terminal()
            })
            .count();
        if active >= MAX_ACTIVE_RUNS {
            return Err(RunManagementError::resource_limit(
                "active Agent Run limit has been reached",
            ));
        }
        if records.len() >= MAX_RUN_RECORDS {
            return Err(RunManagementError::resource_limit(
                "owner Agent Run record limit has been reached",
            ));
        }

        let run_id = self.next_run_id();
        let provider = request.provider;
        let record = Arc::new(RunRecord::new(run_id.clone(), &request, workspace_lock));
        records.insert(run_id.clone(), Arc::clone(&record));
        drop(records);

        self.push_event(&record, RunEventKind::Started, None);
        let worker_inner = Arc::downgrade(&self.inner);
        let worker_record = Arc::clone(&record);
        let worker_request = request;
        let worker = thread::Builder::new()
            .name(format!("aifuel-run-{provider}"))
            .spawn(move || run_worker(worker_inner, worker_record, adapter, worker_request))
            .map_err(|error| {
                self.inner
                    .records
                    .lock()
                    .expect("run records mutex")
                    .remove(&run_id);
                RunManagementError::new(
                    RunManagementErrorCode::Internal,
                    format!("could not start Agent Run worker: {error}"),
                )
            })?;
        *record.worker.lock().expect("run worker mutex") = Some(worker);
        Ok(record.snapshot())
    }

    /// Return current metadata for an owner-local run.
    pub fn get_run(&self, run_id: &str) -> Result<ManagedRun, RunManagementError> {
        self.record(run_id).map(|record| record.snapshot())
    }

    /// Read a non-consuming, bounded page of ordered events.
    pub fn read_events(
        &self,
        run_id: &str,
        cursor: Option<&str>,
        page_bytes: Option<usize>,
    ) -> Result<RunEvents, RunManagementError> {
        let record = self.record(run_id)?;
        let page_bytes = page_bytes.unwrap_or(DEFAULT_EVENT_PAGE_BYTES);
        if page_bytes == 0 || page_bytes > MAX_EVENT_PAGE_BYTES {
            return Err(RunManagementError::invalid_request(format!(
                "event page must be between 1 and {MAX_EVENT_PAGE_BYTES} bytes"
            )));
        }
        let cursor_sequence = cursor
            .map(|cursor| decode_cursor(run_id, cursor))
            .transpose()?;
        let metadata = record.metadata.lock().expect("run metadata mutex");
        let terminal = metadata.state.is_terminal();
        drop(metadata);

        let events = record.events.lock().expect("run events mutex");
        let earliest = events.events.front().map(|event| event.sequence);
        let latest = events.events.back().map(|event| event.sequence);
        if let Some(sequence) = cursor_sequence
            && latest.is_some_and(|latest| sequence > latest)
        {
            return Err(RunManagementError::invalid_cursor(
                "event cursor is newer than the retained event stream",
            ));
        }
        let gap = events.gap
            || cursor_sequence.is_some_and(|sequence| {
                earliest.is_some_and(|first| sequence.saturating_add(1) < first)
            });
        let after = cursor_sequence.unwrap_or(0);
        let mut selected = Vec::new();
        let mut used = 0usize;
        for event in events.events.iter().filter(|event| event.sequence > after) {
            let event_bytes = event_size(event);
            if !selected.is_empty() && used.saturating_add(event_bytes) > page_bytes {
                break;
            }
            selected.push(event.clone());
            used = used.saturating_add(event_bytes);
        }
        let next_cursor = selected.last().and_then(|last| {
            events
                .events
                .iter()
                .any(|event| event.sequence > last.sequence)
                .then(|| encode_cursor(run_id, last.sequence))
        });
        Ok(RunEvents {
            events: selected,
            next_cursor,
            gap,
            terminal,
        })
    }

    /// Return a non-consuming bounded result projection. Active runs have no
    /// provider status yet, but their state and metadata remain inspectable.
    pub fn get_result(&self, run_id: &str) -> Result<ManagedRunResult, RunManagementError> {
        let record = self.record(run_id)?;
        let mut result = record.result_snapshot();
        if let Some(store) = self
            .inner
            .content_store
            .lock()
            .expect("content store mutex")
            .as_ref()
            && let Ok(Some(content)) = store.load(run_id)
        {
            result.output = content.output;
            result.error = content.error;
            result.diagnostics = content.diagnostics;
            result.content_available = true;
        }
        Ok(result)
    }

    pub fn request_input(
        &self,
        run_id: &str,
        kind: RunInputKind,
        description: impl Into<String>,
    ) -> Result<PendingRunInput, RunManagementError> {
        let record = self.record(run_id)?;
        let mut metadata = record.metadata.lock().expect("run metadata mutex");
        if metadata.state.is_terminal() {
            return Err(RunManagementError::new(
                RunManagementErrorCode::InputConflict,
                "run is already terminal",
            ));
        }
        let input = PendingRunInput {
            input_id: format!("input-{}-{}", std::process::id(), now()),
            run_id: run_id.to_owned(),
            kind,
            description: description.into(),
        };
        metadata.state = match input.kind {
            RunInputKind::Ordinary => RunState::WaitingForInput,
            RunInputKind::Permission => RunState::WaitingForApproval,
        };
        drop(metadata);
        *record.pending_input.lock().expect("pending input mutex") = Some(input.clone());
        self.push_event(
            &record,
            match input.kind {
                RunInputKind::Ordinary => RunEventKind::WaitingForInput,
                RunInputKind::Permission => RunEventKind::WaitingForApproval,
            },
            None,
        );
        Ok(input)
    }

    pub fn answer_input(
        &self,
        run_id: &str,
        input_id: &str,
        _response: &str,
    ) -> Result<ManagedRun, RunManagementError> {
        let record = self.record(run_id)?;
        let pending = record
            .pending_input
            .lock()
            .expect("pending input mutex")
            .clone()
            .ok_or_else(|| {
                RunManagementError::new(
                    RunManagementErrorCode::InputConflict,
                    "no pending input exists",
                )
            })?;
        if pending.input_id != input_id {
            return Err(RunManagementError::new(
                RunManagementErrorCode::InputConflict,
                "input response does not match the pending request",
            ));
        }
        if pending.kind == RunInputKind::Permission {
            return Err(RunManagementError::new(
                RunManagementErrorCode::UnsupportedCapability,
                "permission approvals must be delivered through the local terminal operation",
            ));
        }
        *record.pending_input.lock().expect("pending input mutex") = None;
        let mut metadata = record.metadata.lock().expect("run metadata mutex");
        if metadata.state.is_terminal() {
            return Err(RunManagementError::new(
                RunManagementErrorCode::InputConflict,
                "run became terminal before the input response",
            ));
        }
        metadata.state = RunState::Running;
        drop(metadata);
        self.push_event(&record, RunEventKind::Running, None);
        Ok(record.snapshot())
    }

    /// Start a new Agent Run against a known same-provider native session.
    /// Associations are owner-local until persistent session storage is enabled.
    pub fn resume_session(
        &self,
        session_id: &str,
        mut request: RunRequest,
    ) -> Result<ManagedRun, RunManagementError> {
        let local = self
            .inner
            .sessions
            .lock()
            .expect("run sessions mutex")
            .get(session_id)
            .cloned();
        let session = local
            .or_else(|| {
                self.inner
                    .session_store
                    .lock()
                    .expect("session store mutex")
                    .as_ref()
                    .and_then(|store| store.get(session_id))
                    .map(|session| SessionRecord {
                        provider: session.provider,
                        model: session.model,
                        effort: session.effort,
                        working_directory: session.working_directory,
                    })
            })
            .ok_or_else(|| {
                RunManagementError::new(
                    RunManagementErrorCode::SessionUnavailable,
                    "native Agent Session is not available to this owner",
                )
            })?;
        if request.provider != session.provider {
            return Err(RunManagementError::new(
                RunManagementErrorCode::SessionUnavailable,
                "session provider does not match the requested provider",
            ));
        }
        if request.model.is_none() {
            request.model = session.model;
        }
        if request.effort.is_none() {
            request.effort = session.effort;
        }
        request.working_directory = request
            .working_directory
            .or(Some(session.working_directory));
        request.resume = Some(session_id.to_owned());
        self.start_run(request)
    }

    /// Request cancellation. A terminal result is never rewritten.
    pub fn cancel_run(&self, run_id: &str) -> Result<ManagedRun, RunManagementError> {
        let record = self.record(run_id)?;
        let should_cancel = {
            let mut metadata = record.metadata.lock().expect("run metadata mutex");
            if metadata.state.is_terminal() {
                false
            } else {
                metadata.state = RunState::Cancelling;
                true
            }
        };
        if should_cancel {
            record.cancellation.cancel();
            self.push_event(
                &record,
                RunEventKind::StateChanged,
                Some("cancelling".to_owned()),
            );
        }
        Ok(record.snapshot())
    }

    /// Cancel active runs and join every owned worker before returning.
    pub fn shutdown(&self) {
        shutdown_inner(&self.inner);
    }
}

fn shutdown_inner(inner: &Arc<ManagerInner>) {
    if inner.shutdown.swap(true, Ordering::AcqRel) {
        return;
    }
    let records = inner
        .records
        .lock()
        .expect("run records mutex")
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for record in &records {
        let mut metadata = record.metadata.lock().expect("run metadata mutex");
        if !metadata.state.is_terminal() {
            metadata.state = RunState::Cancelling;
            record.cancellation.cancel();
        }
    }
    for record in records {
        if let Some(worker) = record.worker.lock().expect("run worker mutex").take() {
            let _ = worker.join();
        }
    }
}

impl RunManager {
    fn next_run_id(&self) -> String {
        let sequence = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        // The identifier intentionally contains no provider, model, or prompt
        // data. Owner scoping is enforced by the manager instance.
        format!("r{:x}-{:x}-{:x}", std::process::id(), stamp, sequence)
    }

    fn record(&self, run_id: &str) -> Result<Arc<RunRecord>, RunManagementError> {
        self.inner
            .records
            .lock()
            .expect("run records mutex")
            .get(run_id)
            .cloned()
            .ok_or_else(|| RunManagementError::run_not_found(run_id))
    }

    fn resolve_request(&self, request: &RunRequest) -> Result<RunRequest, RunManagementError> {
        self.resolve_request_with_adapter(request)
            .map(|(request, _)| request)
    }

    fn resolve_request_with_adapter(
        &self,
        request: &RunRequest,
    ) -> Result<(RunRequest, AdapterHandle), RunManagementError> {
        if request.prompt.trim().is_empty() {
            return Err(RunManagementError::invalid_request(
                "prompt must not be empty",
            ));
        }
        let adapter = self
            .inner
            .adapters
            .iter()
            .find(|adapter| adapter.provider() == request.provider)
            .ok_or_else(|| {
                RunManagementError::new(
                    RunManagementErrorCode::AgentUnavailable,
                    format!(
                        "provider {} has no registered Agent Integration",
                        request.provider
                    ),
                )
            })?;
        let mut resolved = request.clone();
        if let Some(directory) = &request.working_directory {
            let canonical = fs::canonicalize(directory).map_err(|error| {
                RunManagementError::invalid_request(format!(
                    "working directory is unavailable: {error}"
                ))
            })?;
            if !canonical.is_dir() {
                return Err(RunManagementError::invalid_request(
                    "working directory is not an existing directory",
                ));
            }
            self.validate_root(&canonical)?;
            resolved.working_directory = Some(canonical);
        }
        adapter.validate(&resolved).map_err(map_validation_error)?;
        Ok((resolved, adapter.clone()))
    }

    fn validate_root(&self, directory: &Path) -> Result<(), RunManagementError> {
        let policy = self
            .inner
            .policy
            .lock()
            .expect("run manager policy mutex")
            .clone();
        let Some(roots) = policy.allowed_roots else {
            return Ok(());
        };
        let admitted = roots
            .iter()
            .filter_map(|root| fs::canonicalize(root).ok())
            .any(|root| directory == root || directory.starts_with(root));
        if admitted {
            Ok(())
        } else {
            Err(RunManagementError::new(
                RunManagementErrorCode::PolicyDenied,
                "working directory is outside the configured execution roots",
            ))
        }
    }

    fn push_event(&self, record: &Arc<RunRecord>, kind: RunEventKind, data: Option<String>) {
        record.push_event(kind, data);
    }

    fn complete(&self, record: &Arc<RunRecord>, result: Result<RunResult, AgentRunError>) {
        let (
            status,
            state,
            effective_model,
            effective_effort,
            local_session_id,
            session_id,
            exit_code,
            output,
            error,
            diagnostics,
        ) = match result {
            Ok(result) => {
                if let Some(session_id) = result.session_id.as_ref() {
                    self.inner
                        .sessions
                        .lock()
                        .expect("run sessions mutex")
                        .insert(
                            session_id.clone(),
                            SessionRecord {
                                provider: result.provider_id,
                                model: result.requested_model.clone(),
                                effort: result.requested_effort.clone(),
                                working_directory: result.working_directory.clone(),
                            },
                        );
                    if let Some(store) = self
                        .inner
                        .session_store
                        .lock()
                        .expect("session store mutex")
                        .as_mut()
                    {
                        let _ = store.insert(
                            session_id.clone(),
                            PersistedSession {
                                provider: result.provider_id,
                                model: result.requested_model.clone(),
                                effort: result.requested_effort.clone(),
                                working_directory: result.working_directory.clone(),
                            },
                        );
                    }
                }
                let state = if record.cancellation.is_cancelled()
                    && result.status == RunStatus::Succeeded
                {
                    RunState::Cancelled
                } else {
                    RunState::from(result.status)
                };
                (
                    if state == RunState::Cancelled {
                        RunStatus::Cancelled
                    } else {
                        result.status
                    },
                    state,
                    result.effective_model,
                    result.effective_effort,
                    Some(result.local_session_id),
                    result.session_id,
                    result.exit_code,
                    Some(result.output),
                    result.error,
                    result.diagnostics,
                )
            }
            Err(error) => {
                let (status, state) = match error {
                    AgentRunError::Cancelled => (RunStatus::Cancelled, RunState::Cancelled),
                    AgentRunError::Timeout(_) => (RunStatus::Timeout, RunState::TimedOut),
                    _ => (RunStatus::Failed, RunState::Failed),
                };
                (
                    status,
                    state,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(error.to_string()),
                    None,
                )
            }
        };
        let (output, output_truncated, output_bytes) =
            bound_content(output, MAX_ANSWER_BYTES_PER_RUN);
        let (error, _error_truncated, error_bytes) = bound_content(error, MAX_ANSWER_BYTES_PER_RUN);
        let (diagnostics, diagnostics_truncated, diagnostics_bytes) =
            bound_content(diagnostics, MAX_ANSWER_BYTES_PER_RUN);
        let content_bytes = output_bytes
            .saturating_add(error_bytes)
            .saturating_add(diagnostics_bytes);
        let previous = self
            .inner
            .retained_content_bytes
            .fetch_add(content_bytes, Ordering::AcqRel);
        let retain_content = previous.saturating_add(content_bytes) <= MAX_OWNER_CONTENT_BYTES;
        if !retain_content {
            self.inner
                .retained_content_bytes
                .fetch_sub(content_bytes, Ordering::AcqRel);
        } else {
            record
                .retained_content_bytes
                .store(content_bytes, Ordering::Release);
        }
        let event_output = output.clone();
        let event_diagnostics = diagnostics.clone();
        {
            let mut metadata = record.metadata.lock().expect("run metadata mutex");
            if metadata.state.is_terminal() {
                return;
            }
            metadata.state = state;
            metadata.completed_at = Some(now());
            metadata.content_available = retain_content;
            let persisted = if let Some(store) = self
                .inner
                .content_store
                .lock()
                .expect("content store mutex")
                .as_ref()
            {
                store
                    .save(
                        &record.run_id,
                        &PersistedContent {
                            output: output.clone(),
                            error: error.clone(),
                            diagnostics: diagnostics.clone(),
                        },
                    )
                    .is_ok()
            } else {
                false
            };
            metadata.content_persisted = persisted;
            metadata.content_available = retain_content || persisted;
            metadata.result = ResultMetadata {
                status: Some(status),
                effective_model,
                effective_effort,
                local_session_id,
                session_id,
                exit_code,
                output: if retain_content { output } else { None },
                error: if retain_content { error } else { None },
                diagnostics: if retain_content { diagnostics } else { None },
                output_truncated,
                diagnostics_truncated,
                output_bytes,
                diagnostics_bytes,
            };
        }
        if retain_content {
            if let Some(output) = event_output {
                self.push_event(record, RunEventKind::Output, Some(output));
            }
            if let Some(diagnostics) = event_diagnostics {
                self.push_event(record, RunEventKind::Diagnostic, Some(diagnostics));
            }
        }
        let terminal_kind = match state {
            RunState::Cancelled => RunEventKind::Cancelled,
            RunState::TimedOut => RunEventKind::TimedOut,
            RunState::Succeeded => RunEventKind::Completed,
            _ => RunEventKind::Failed,
        };
        self.push_event(record, terminal_kind, None);
        let _ = record
            .workspace_lock
            .lock()
            .expect("workspace lock mutex")
            .take();
        self.register_completed(record);
    }

    fn register_completed(&self, record: &Arc<RunRecord>) {
        let mut order = self
            .inner
            .completed_order
            .lock()
            .expect("completed run order mutex");
        order.push_back(record.run_id.clone());
        while order.len() > MAX_COMPLETED_CONTENT {
            if let Some(oldest) = order.pop_front()
                && let Some(record) = self
                    .inner
                    .records
                    .lock()
                    .expect("run records mutex")
                    .get(&oldest)
                    .cloned()
            {
                self.evict_content(&record);
            }
        }
    }

    fn evict_content(&self, record: &RunRecord) {
        let bytes = record.retained_content_bytes.swap(0, Ordering::AcqRel);
        if bytes > 0 {
            self.inner
                .retained_content_bytes
                .fetch_sub(bytes, Ordering::AcqRel);
        }
        record.clear_content();
    }
}

impl Drop for OwnerToken {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            shutdown_inner(&inner);
        }
    }
}

fn run_worker(
    weak_inner: Weak<ManagerInner>,
    record: Arc<RunRecord>,
    adapter: AdapterHandle,
    request: RunRequest,
) {
    let Some(inner) = weak_inner.upgrade() else {
        return;
    };
    let manager = RunManager {
        inner,
        _owner: None,
    };
    {
        let mut metadata = record.metadata.lock().expect("run metadata mutex");
        if metadata.state == RunState::Cancelling {
            drop(metadata);
            manager.complete(&record, Err(AgentRunError::Cancelled));
            return;
        }
        metadata.state = RunState::Running;
    }
    manager.push_event(&record, RunEventKind::Running, None);
    let result = adapter.execute(&request, &record.cancellation);
    manager.complete(&record, result);
}

impl RunRecord {
    fn new(
        run_id: String,
        request: &RunRequest,
        workspace_lock: Option<WorkspaceWriteLock>,
    ) -> Self {
        Self {
            run_id,
            provider: request.provider,
            requested_model: request.model.clone(),
            requested_effort: request.effort.clone(),
            external_tools: request.external_tools.clone(),
            created_at: now(),
            metadata: Mutex::new(RunMetadata {
                state: RunState::Starting,
                completed_at: None,
                content_available: false,
                content_persisted: false,
                result: ResultMetadata::empty(),
            }),
            pending_input: Mutex::new(None),
            events: Mutex::new(EventBuffer::default()),
            cancellation: RunCancellationToken::new(),
            retained_content_bytes: AtomicUsize::new(0),
            worker: Mutex::new(None),
            workspace_lock: Mutex::new(workspace_lock),
        }
    }

    fn snapshot(&self) -> ManagedRun {
        let metadata = self.metadata.lock().expect("run metadata mutex");
        ManagedRun {
            schema_version: RUN_MANAGEMENT_SCHEMA_VERSION,
            run_id: self.run_id.clone(),
            state: metadata.state,
            provider: self.provider,
            requested_model: self.requested_model.clone(),
            requested_effort: self.requested_effort.clone(),
            external_tools: self.external_tools.clone(),
            created_at: self.created_at,
            completed_at: metadata.completed_at,
            content_available: metadata.content_available,
            pending_input: self
                .pending_input
                .lock()
                .expect("pending input mutex")
                .clone(),
        }
    }

    fn result_snapshot(&self) -> ManagedRunResult {
        let metadata = self.metadata.lock().expect("run metadata mutex");
        let result = &metadata.result;
        ManagedRunResult {
            schema_version: RUN_MANAGEMENT_SCHEMA_VERSION,
            run_id: self.run_id.clone(),
            state: metadata.state,
            provider: self.provider,
            requested_model: self.requested_model.clone(),
            requested_effort: self.requested_effort.clone(),
            effective_model: result.effective_model.clone(),
            effective_effort: result.effective_effort.clone(),
            local_session_id: result.local_session_id.clone(),
            session_id: result.session_id.clone(),
            status: result.status,
            exit_code: result.exit_code,
            output: result.output.clone(),
            error: result.error.clone(),
            diagnostics: result.diagnostics.clone(),
            content_available: metadata.content_available,
            output_truncated: result.output_truncated,
            diagnostics_truncated: result.diagnostics_truncated,
            output_bytes: result.output_bytes,
            diagnostics_bytes: result.diagnostics_bytes,
        }
    }

    fn clear_content(&self) {
        let mut metadata = self.metadata.lock().expect("run metadata mutex");
        metadata.content_available = metadata.content_persisted;
        metadata.result.output = None;
        metadata.result.error = None;
        metadata.result.diagnostics = None;
    }

    fn push_event(&self, kind: RunEventKind, data: Option<String>) {
        self.events
            .lock()
            .expect("run events mutex")
            .push(&self.run_id, kind, data);
    }
}

impl ResultMetadata {
    fn empty() -> Self {
        Self {
            status: None,
            effective_model: None,
            effective_effort: None,
            local_session_id: None,
            session_id: None,
            exit_code: None,
            output: None,
            error: None,
            diagnostics: None,
            output_truncated: false,
            diagnostics_truncated: false,
            output_bytes: 0,
            diagnostics_bytes: 0,
        }
    }
}

impl EventBuffer {
    fn push(&mut self, run_id: &str, kind: RunEventKind, data: Option<String>) {
        let (data, _) = truncate_string(data, MAX_EVENT_BYTES_PER_RUN.saturating_sub(96));
        let event = RunEvent {
            schema_version: RUN_MANAGEMENT_SCHEMA_VERSION,
            run_id: run_id.to_owned(),
            sequence: self.next_sequence,
            created_at: now(),
            kind,
            data,
        };
        self.next_sequence = self.next_sequence.saturating_add(1);
        let bytes = event_size(&event);
        while !self.events.is_empty() && self.bytes.saturating_add(bytes) > MAX_EVENT_BYTES_PER_RUN
        {
            if let Some(oldest) = self.events.pop_front() {
                self.bytes = self.bytes.saturating_sub(event_size(&oldest));
                self.gap = true;
            }
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.events.push_back(event);
    }
}

fn map_validation_error(error: AgentRunError) -> RunManagementError {
    match error {
        AgentRunError::InvalidRequest(message) => RunManagementError::invalid_request(message),
        AgentRunError::UnsupportedProvider(provider) => RunManagementError::new(
            RunManagementErrorCode::AgentUnavailable,
            format!("provider {provider} has no registered Agent Integration"),
        ),
        AgentRunError::Timeout(message) => {
            RunManagementError::new(RunManagementErrorCode::ConnectionTimeout, message)
        }
        AgentRunError::Cancelled => RunManagementError::invalid_request("request was cancelled"),
        AgentRunError::Io(error) => RunManagementError::new(
            RunManagementErrorCode::AgentUnavailable,
            format!("Agent Integration is unavailable: {error}"),
        ),
    }
}

fn event_size(event: &RunEvent) -> usize {
    event
        .data
        .as_ref()
        .map_or(0, String::len)
        .saturating_add(96)
}

fn encode_cursor(run_id: &str, sequence: u64) -> String {
    let owner = run_id
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{owner}.{sequence:x}")
}

fn decode_cursor(run_id: &str, cursor: &str) -> Result<u64, RunManagementError> {
    let (owner, sequence) = cursor
        .split_once('.')
        .ok_or_else(|| RunManagementError::invalid_cursor("event cursor is malformed"))?;
    let expected = run_id
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if owner != expected || sequence.is_empty() {
        return Err(RunManagementError::invalid_cursor(
            "event cursor belongs to another run or is malformed",
        ));
    }
    u64::from_str_radix(sequence, 16)
        .map_err(|_| RunManagementError::invalid_cursor("event cursor sequence is malformed"))
}

fn bound_content(value: Option<String>, limit: usize) -> (Option<String>, bool, usize) {
    let Some(value) = value else {
        return (None, false, 0);
    };
    let bytes = value.len();
    let (value, truncated) = truncate_string(Some(value), limit);
    (value, truncated, bytes)
}

fn truncate_string(value: Option<String>, limit: usize) -> (Option<String>, bool) {
    let Some(mut value) = value else {
        return (None, false);
    };
    if value.len() <= limit {
        return (Some(value), false);
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    (Some(value), true)
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aifuel_core::{AccessMode, ExecutionMode, OutputFormat, ProviderKey};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    struct ProbeAdapter {
        provider: ProviderKey,
        wait: Arc<AtomicBool>,
    }

    impl AgentExecutionAdapter for ProbeAdapter {
        fn provider(&self) -> ProviderKey {
            self.provider
        }

        fn execute(
            &self,
            request: &RunRequest,
            cancellation: &RunCancellationToken,
        ) -> Result<RunResult, AgentRunError> {
            while self.wait.load(Ordering::Acquire) {
                if cancellation.is_cancelled() {
                    return Err(AgentRunError::Cancelled);
                }
                thread::sleep(Duration::from_millis(1));
            }
            Ok(RunResult {
                run_id: "provider-run".to_owned(),
                local_session_id: "provider-session".to_owned(),
                session_id: Some("native-session".to_owned()),
                resumed_from: request.resume.clone(),
                provider_id: self.provider,
                requested_model: request.model.clone(),
                requested_effort: request.effort.clone(),
                effective_model: Some("effective-model".to_owned()),
                effective_effort: None,
                requested_account_id: request.account.clone(),
                account_id: None,
                execution_mode: ExecutionMode::PromptOnly,
                permission_profile: request.access,
                status: RunStatus::Succeeded,
                exit_code: Some(0),
                output: request.prompt.clone(),
                error: None,
                diagnostics: None,
                timed_out: false,
                working_directory: std::env::temp_dir(),
            })
        }
    }

    fn request() -> RunRequest {
        RunRequest {
            provider: ProviderKey::Claude,
            model: Some("model-a".to_owned()),
            effort: None,
            external_tools: None,
            account: None,
            prompt: "hello".to_owned(),
            output: OutputFormat::Text,
            working_directory: None,
            access: aifuel_core::AccessMode::ReadOnly,
            resume: None,
            timeout: None,
        }
    }

    fn wait_for_terminal(manager: &RunManager, id: &str) -> ManagedRun {
        for _ in 0..100 {
            let run = manager.get_run(id).expect("run remains owner-local");
            if run.state.is_terminal() {
                return run;
            }
            thread::sleep(Duration::from_millis(2));
        }
        panic!("run did not reach a terminal state")
    }

    #[test]
    fn start_resolve_and_result_share_the_owner_local_contract() {
        let wait = Arc::new(AtomicBool::new(false));
        let adapter = Arc::new(ProbeAdapter {
            provider: ProviderKey::Claude,
            wait,
        });
        let manager = RunManager::new(vec![adapter]);

        let resolved = manager.resolve_run(&request()).expect("request resolves");
        assert_eq!(resolved.provider, ProviderKey::Claude);
        assert_eq!(resolved.requested_model.as_deref(), Some("model-a"));
        assert!(
            !serde_json::to_string(&resolved)
                .expect("resolved DTO serializes")
                .contains("hello")
        );

        let started = manager.start_run(request()).expect("run starts");
        let terminal = wait_for_terminal(&manager, &started.run_id);
        assert_eq!(terminal.state, RunState::Succeeded);
        assert_eq!(terminal.provider, ProviderKey::Claude);
        assert!(terminal.completed_at.is_some());

        let result = manager
            .get_result(&started.run_id)
            .expect("result is available");
        assert_eq!(result.status, Some(RunStatus::Succeeded));
        assert_eq!(result.output.as_deref(), Some("hello"));
        assert_eq!(result.session_id.as_deref(), Some("native-session"));
        let events = manager
            .read_events(&started.run_id, None, None)
            .expect("events are readable");
        assert!(events.terminal);
        assert!(!events.gap);
        assert!(
            events
                .events
                .windows(2)
                .all(|events| events[0].sequence < events[1].sequence)
        );
        manager.shutdown();
    }

    #[test]
    fn same_provider_session_can_resume_with_current_run_policy() {
        let wait = Arc::new(AtomicBool::new(false));
        let adapter = Arc::new(ProbeAdapter {
            provider: ProviderKey::Claude,
            wait,
        });
        let manager = RunManager::new(vec![adapter]);
        let first = manager.start_run(request()).expect("first run starts");
        wait_for_terminal(&manager, &first.run_id);
        let resumed = manager
            .resume_session("native-session", request())
            .expect("same-provider session resumes");
        let terminal = wait_for_terminal(&manager, &resumed.run_id);
        assert_eq!(terminal.provider, ProviderKey::Claude);
        assert_ne!(first.run_id, resumed.run_id);
        manager.shutdown();
    }

    #[test]
    fn native_session_metadata_survives_owner_restart_without_prompt_content() {
        let path = std::env::temp_dir().join(format!(
            "aifuel-session-test-{}-{}.json",
            std::process::id(),
            now()
        ));
        let wait = Arc::new(AtomicBool::new(false));
        {
            let manager = RunManager::new(vec![Arc::new(ProbeAdapter {
                provider: ProviderKey::Claude,
                wait: Arc::clone(&wait),
            })])
            .with_session_store(&path)
            .expect("session store should load");
            let first = manager.start_run(request()).expect("first run starts");
            wait_for_terminal(&manager, &first.run_id);
        }
        let resumed = RunManager::new(vec![Arc::new(ProbeAdapter {
            provider: ProviderKey::Claude,
            wait,
        })])
        .with_session_store(&path)
        .expect("session store should reload")
        .resume_session("native-session", request())
        .expect("persisted same-provider session resumes");
        assert!(!resumed.run_id.is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn persistent_content_is_written_only_when_a_content_store_is_attached() {
        let directory = std::env::temp_dir().join(format!(
            "aifuel-content-test-{}-{}",
            std::process::id(),
            now()
        ));
        let manager = RunManager::new(vec![Arc::new(ProbeAdapter {
            provider: ProviderKey::Claude,
            wait: Arc::new(AtomicBool::new(false)),
        })])
        .with_content_store(&directory)
        .expect("content store should open");
        let started = manager.start_run(request()).expect("run starts");
        wait_for_terminal(&manager, &started.run_id);
        let result = manager
            .get_result(&started.run_id)
            .expect("result is available");
        assert!(result.content_available);
        assert!(
            std::fs::read_dir(&directory)
                .expect("content directory exists")
                .next()
                .is_some()
        );
        manager.shutdown();
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn ordinary_input_waits_and_resumes_while_permissions_stay_local_only() {
        let adapter = Arc::new(ProbeAdapter {
            provider: ProviderKey::Claude,
            wait: Arc::new(AtomicBool::new(true)),
        });
        let manager = RunManager::new(vec![adapter]);
        let started = manager.start_run(request()).expect("run starts");
        let ordinary = manager
            .request_input(&started.run_id, RunInputKind::Ordinary, "choose a target")
            .expect("ordinary input waits");
        assert_eq!(
            manager.get_run(&started.run_id).unwrap().state,
            RunState::WaitingForInput
        );
        manager
            .answer_input(&started.run_id, &ordinary.input_id, "workspace")
            .expect("ordinary input resumes");
        let approval = manager
            .request_input(&started.run_id, RunInputKind::Permission, "write file")
            .expect("permission request waits");
        let error = manager
            .answer_input(&started.run_id, &approval.input_id, "allow")
            .expect_err("MCP cannot approve permissions");
        assert_eq!(error.code, RunManagementErrorCode::UnsupportedCapability);
        manager.shutdown();
    }

    #[test]
    fn cancellation_is_idempotent_and_terminal_state_is_immutable() {
        let wait = Arc::new(AtomicBool::new(true));
        let adapter = Arc::new(ProbeAdapter {
            provider: ProviderKey::Claude,
            wait: Arc::clone(&wait),
        });
        let manager = RunManager::new(vec![adapter]);
        let started = manager.start_run(request()).expect("run starts");
        let cancelling = manager
            .cancel_run(&started.run_id)
            .expect("cancel is accepted");
        assert!(matches!(
            cancelling.state,
            RunState::Cancelling | RunState::Cancelled
        ));
        let terminal = wait_for_terminal(&manager, &started.run_id);
        assert_eq!(terminal.state, RunState::Cancelled);
        let repeated = manager
            .cancel_run(&started.run_id)
            .expect("repeated cancellation is idempotent");
        assert_eq!(repeated.state, RunState::Cancelled);
        wait.store(false, Ordering::Release);
        manager.shutdown();
    }

    #[test]
    fn dropping_the_owner_cancels_and_joins_active_runs() {
        let wait = Arc::new(AtomicBool::new(true));
        {
            let adapter = Arc::new(ProbeAdapter {
                provider: ProviderKey::Claude,
                wait,
            });
            let manager = RunManager::new(vec![adapter]);
            manager.start_run(request()).expect("run starts");
        }
    }

    #[test]
    fn restricted_roots_are_checked_before_provider_execution() {
        let wait = Arc::new(AtomicBool::new(false));
        let adapter = Arc::new(ProbeAdapter {
            provider: ProviderKey::Claude,
            wait,
        });
        let manager = RunManager::new(vec![adapter]).with_allowed_roots(Vec::new());
        let mut request = request();
        request.working_directory = Some(std::env::temp_dir());
        let error = manager
            .resolve_run(&request)
            .expect_err("empty MCP roots deny repository paths");
        assert_eq!(error.code, RunManagementErrorCode::PolicyDenied);
        manager.shutdown();
    }

    #[test]
    fn workspace_write_runs_conflict_across_manager_instances() {
        let workspace = std::env::temp_dir();
        let wait = Arc::new(AtomicBool::new(true));
        let first = RunManager::new(vec![Arc::new(ProbeAdapter {
            provider: ProviderKey::Claude,
            wait: Arc::clone(&wait),
        })]);
        let second = RunManager::new(vec![Arc::new(ProbeAdapter {
            provider: ProviderKey::Claude,
            wait: Arc::clone(&wait),
        })]);
        let mut request = request();
        request.access = AccessMode::WorkspaceWrite;
        request.working_directory = Some(workspace);
        let started = first
            .start_run(request.clone())
            .expect("first write starts");
        let error = second
            .start_run(request)
            .expect_err("overlapping workspace writes must conflict");
        assert_eq!(error.code, RunManagementErrorCode::WriteConflict);
        first
            .cancel_run(&started.run_id)
            .expect("first write cancels");
        wait.store(false, Ordering::Release);
        first.shutdown();
        second.shutdown();
    }
}
