use super::helpers::{bound_content, map_validation_error, now};
use super::*;

impl RunManager {
    pub(super) fn next_run_id(&self) -> String {
        let sequence = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        // The identifier intentionally contains no provider, model, or prompt
        // data. Owner scoping is enforced by the manager instance.
        format!("r{:x}-{:x}-{:x}", std::process::id(), stamp, sequence)
    }

    pub(super) fn record(&self, run_id: &str) -> Result<Arc<RunRecord>, RunManagementError> {
        self.inner
            .records
            .lock()
            .expect("run records mutex")
            .get(run_id)
            .cloned()
            .ok_or_else(|| RunManagementError::run_not_found(run_id))
    }

    pub(super) fn resolve_request(
        &self,
        request: &RunRequest,
    ) -> Result<RunRequest, RunManagementError> {
        self.resolve_request_with_adapter(request)
            .map(|(request, _)| request)
    }

    pub(super) fn resolve_request_with_adapter(
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
        if resolved.access == aifuel_core::AccessMode::ReadOnly
            && let Some(tools) = &resolved.external_tools
        {
            let policy = self
                .inner
                .policy
                .lock()
                .expect("run manager policy mutex")
                .clone();
            if let Some(tool) = tools
                .iter()
                .find(|tool| !policy.allowed_read_only_external_tools.contains(*tool))
            {
                return Err(RunManagementError::new(
                    RunManagementErrorCode::PolicyDenied,
                    format!("external MCP tool {tool:?} is not allowlisted for read-only runs"),
                ));
            }
        }
        if let Some(tools) = &resolved.external_tools {
            let mut unique = std::collections::HashSet::with_capacity(tools.len());
            if tools.iter().any(|tool| !unique.insert(tool)) {
                return Err(RunManagementError::invalid_request(
                    "external MCP tool selection contains duplicate names",
                ));
            }
        }
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

    pub(super) fn push_event(
        &self,
        record: &Arc<RunRecord>,
        kind: RunEventKind,
        data: Option<String>,
    ) {
        record.push_event(kind, data);
    }

    pub(super) fn complete(
        &self,
        record: &Arc<RunRecord>,
        result: Result<RunResult, AgentRunError>,
    ) {
        #[cfg(any(unix, windows))]
        if let Some(pending) = record
            .pending_input
            .lock()
            .expect("pending input mutex")
            .clone()
            .filter(|pending| pending.kind == RunInputKind::Permission)
        {
            self.remove_local_approval_pending(&pending.run_id, &pending.input_id);
        }
        *record.pending_input.lock().expect("pending input mutex") = None;
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
        let captured_output = record
            .output_capture
            .lock()
            .expect("run output capture mutex")
            .clone();
        let (output, output_truncated, output_bytes) = if captured_output.streamed {
            (
                Some(captured_output.text),
                captured_output.truncated,
                captured_output.observed_bytes,
            )
        } else {
            bound_content(output, MAX_ANSWER_BYTES_PER_RUN)
        };
        let (error, _error_truncated, _error_bytes) =
            bound_content(error, MAX_ANSWER_BYTES_PER_RUN);
        let (diagnostics, diagnostics_truncated, diagnostics_bytes) =
            bound_content(diagnostics, MAX_ANSWER_BYTES_PER_RUN);
        let content_bytes = output
            .as_ref()
            .map_or(0, String::len)
            .saturating_add(error.as_ref().map_or(0, String::len))
            .saturating_add(diagnostics.as_ref().map_or(0, String::len));
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
        let event_output = (!captured_output.streamed)
            .then(|| output.clone())
            .flatten();
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
        record
            .output_capture
            .lock()
            .expect("run output capture mutex")
            .text
            .clear();
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
