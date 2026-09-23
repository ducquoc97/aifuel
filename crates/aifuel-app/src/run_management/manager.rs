use super::callbacks::ManagedRunOutputHandler;
use super::interaction::OwnerInteractionHandler;
use super::workers::run_worker;
use super::*;
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
            #[cfg(any(unix, windows))]
            approval_directory: Mutex::new(None),
            #[cfg(any(unix, windows))]
            approval_server: Mutex::new(None),
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
        self.inner
            .policy
            .lock()
            .expect("run manager policy mutex")
            .allowed_roots = Some(allowed_roots);
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

    #[cfg(any(unix, windows))]
    pub fn with_local_approval_channel(self, directory: impl AsRef<Path>) -> Result<Self, String> {
        *self
            .inner
            .approval_directory
            .lock()
            .expect("approval directory mutex") = Some(directory.as_ref().to_path_buf());
        Ok(self)
    }

    #[cfg(any(unix, windows))]
    pub(super) fn register_local_approval_pending(
        &self,
        pending: &PendingRunInput,
    ) -> Result<(), AgentRunError> {
        if self.inner.shutdown.load(Ordering::Acquire) {
            return Err(AgentRunError::Cancelled);
        }
        let directory = self
            .inner
            .approval_directory
            .lock()
            .expect("approval directory mutex")
            .clone();
        let Some(directory) = directory else {
            return Ok(());
        };

        let mut server = self
            .inner
            .approval_server
            .lock()
            .expect("approval server mutex");
        if server.is_none() {
            let inner = Arc::downgrade(&self.inner);
            let owner =
                LocalApprovalServer::start(&directory, move |run_id, input_id, decision| {
                    let inner = inner
                        .upgrade()
                        .ok_or_else(|| "execution owner has closed".to_owned())?;
                    let manager = RunManager {
                        inner,
                        _owner: None,
                    };
                    let decision = match decision {
                        LocalApprovalDecision::Accept => PermissionApprovalDecision::Accept,
                        LocalApprovalDecision::Decline => PermissionApprovalDecision::Decline,
                        LocalApprovalDecision::Cancel => PermissionApprovalDecision::Cancel,
                    };
                    if manager.record(&run_id).is_err() {
                        manager.remove_local_approval_pending(&run_id, &input_id);
                        return Ok(false);
                    }
                    manager
                        .approve_locally(&run_id, &input_id, decision)
                        .map(|_| true)
                        .map_err(|error| error.to_string())
                })
                .map_err(|error| {
                    AgentRunError::InvalidRequest(format!(
                        "could not start local approval channel: {error}"
                    ))
                })?;
            *server = Some(owner);
        }
        server
            .as_ref()
            .expect("local approval server was started")
            .register_pending(&pending.run_id, &pending.input_id)
            .map_err(|error| {
                AgentRunError::InvalidRequest(format!(
                    "could not register pending local approval: {error}"
                ))
            })
    }

    #[cfg(any(unix, windows))]
    pub(super) fn remove_local_approval_pending(&self, run_id: &str, input_id: &str) {
        if let Some(server) = self
            .inner
            .approval_server
            .lock()
            .expect("approval server mutex")
            .as_ref()
        {
            server.remove_pending(run_id, input_id);
        }
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

    /// List compiled Agent Integrations and their independently reported
    /// native presence, version, and capability evidence.
    pub fn list_agents(
        &self,
        provider_filter: Option<aifuel_core::ProviderKey>,
    ) -> Vec<aifuel_core::AgentIntegrationInfo> {
        self.inner
            .adapters
            .iter()
            .filter(|adapter| provider_filter.is_none_or(|provider| adapter.provider() == provider))
            .map(|adapter| adapter.agent_info())
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
        let deadline = request
            .timeout
            .map(|timeout| {
                Instant::now()
                    .checked_add(timeout)
                    .ok_or_else(|| RunManagementError::invalid_request("run timeout is too large"))
            })
            .transpose()?;
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
        let record = Arc::new(RunRecord::new(
            run_id.clone(),
            &request,
            deadline,
            workspace_lock,
        ));
        let output_handler: Arc<dyn AgentRunOutputHandler> = Arc::new(ManagedRunOutputHandler {
            record: Arc::downgrade(&record),
        });
        records.insert(run_id.clone(), Arc::clone(&record));
        drop(records);

        self.push_event(&record, RunEventKind::Started, None);
        let worker_inner = Arc::downgrade(&self.inner);
        let worker_record = Arc::clone(&record);
        let mut worker_request = request;
        worker_request.interaction_handler = Some(Arc::new(OwnerInteractionHandler {
            inner: Arc::downgrade(&self.inner),
            record: Arc::downgrade(&record),
        }));
        let worker = thread::Builder::new()
            .name(format!("aifuel-run-{provider}"))
            .spawn(move || {
                run_worker(
                    worker_inner,
                    worker_record,
                    adapter,
                    worker_request,
                    output_handler,
                )
            })
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
}
