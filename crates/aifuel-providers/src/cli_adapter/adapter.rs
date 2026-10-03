//! The [`AgentAdapter`] contract implementation for [`CliAdapter`].
//!
//! Every method enforces the serving integration identity first, maps
//! request-side failures onto receipt codes, and reports only what the
//! wrapped execution adapter's evidence supports. Unsupported surfaces
//! (`checkpoint`, `restore_checkpoint`) answer with explicit
//! `unsupported` receipts rather than silent no-ops.

use super::{CliAdapter, descriptors, session};
use aifuel_core::{
    AdapterCapabilities, AgentAdapter, AgentEventKind, AgentEventStream, AgentRuntimeError,
    AgentSessionHandle, ApprovalDecision, CapabilityState, CheckpointId, ExecutionAvailability,
    Integration, ModelDescriptor, ModelSelection, OutputFormat, ReceiptCode, RequestId, RunId,
    RunRequest, SessionId, SessionStatus, StartOptions, UserInput,
};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

impl AgentAdapter for CliAdapter {
    fn capabilities(&self) -> AdapterCapabilities {
        self.capabilities
    }

    fn resolve(&self, selection: &ModelSelection) -> Result<ModelDescriptor, AgentRuntimeError> {
        self.ensure_serves(&selection.integration_id)?;
        let availability = self.availability();
        let descriptor = self
            .catalog_models()
            .iter()
            .find(|model| model.model_id == selection.model)
            .map(|model| {
                descriptors::catalog_descriptor(
                    &self.provider(),
                    model,
                    CapabilityState::Unknown,
                    availability,
                    None,
                )
            })
            .unwrap_or_else(|| ModelDescriptor {
                provider: self.provider(),
                model: selection.model.clone(),
                label: selection.model.clone(),
                efforts: Vec::new(),
                // No catalog evidence advertises this model, and no
                // model-specific readiness was verified.
                advertised: false,
                entitled: CapabilityState::Unknown,
                availability: ExecutionAvailability::Unknown,
                quota: None,
            });
        if let Some(effort) = selection.effort
            && !descriptor.efforts.contains(&effort)
        {
            return Err(AgentRuntimeError::new(
                ReceiptCode::InvalidSelection,
                format!(
                    "{} is not a selectable effort for {}",
                    effort.as_str(),
                    descriptor.model
                ),
            ));
        }
        Ok(descriptor)
    }

    fn list_models(
        &self,
        integration: &Integration,
    ) -> Result<Vec<ModelDescriptor>, AgentRuntimeError> {
        self.ensure_serves(&integration.id)?;
        // The CLI path carries no per-model entitlement evidence and no
        // monitoring contract, so entitlement stays `unknown` and quota `None`.
        Ok(descriptors::model_descriptors(
            self.provider(),
            self.catalog_models(),
            &BTreeMap::new(),
            self.availability(),
            None,
        ))
    }

    fn start(
        &self,
        integration: &Integration,
        options: StartOptions,
    ) -> Result<AgentSessionHandle, AgentRuntimeError> {
        self.ensure_serves(&integration.id)?;
        if options.selection.integration_id != integration.id {
            return Err(AgentRuntimeError::new(
                ReceiptCode::InvalidSelection,
                "the selection names a different integration than the session",
            ));
        }
        let cwd = std::fs::canonicalize(&options.cwd)
            .map_err(|_| session::invalid_state("the session working directory does not exist"))?;
        if !cwd.is_dir() {
            return Err(session::invalid_state(
                "the session working directory is not a directory",
            ));
        }
        match self.availability() {
            // `unknown` does not block: no evidence claimed the integration
            // could not run, and the run itself reports the outcome.
            ExecutionAvailability::Ready | ExecutionAvailability::Unknown => {}
            ExecutionAvailability::NeedsAuth => {
                return Err(AgentRuntimeError::new(
                    ReceiptCode::InvalidSelection,
                    "the integration needs authentication before it can run",
                ));
            }
            ExecutionAvailability::Unsupported => {
                return Err(AgentRuntimeError::new(
                    ReceiptCode::InvalidSelection,
                    "the integration cannot execute in this environment",
                ));
            }
        }
        // Validate access enforcement up front with the same request shape
        // a run carries; an unenforceable session fails before it exists.
        // The probe carries the exact external tool selection so an
        // execution adapter that cannot enforce it rejects `start`.
        let probe = RunRequest {
            integration: self.integration(),
            model: (!options.selection.model.is_empty()).then(|| options.selection.model.clone()),
            effort: options
                .selection
                .effort
                .map(|effort| effort.as_str().to_owned()),
            external_tools: (!options.external_tools.is_empty())
                .then(|| options.external_tools.clone()),
            account: None,
            prompt: "capability check".to_owned(),
            output: OutputFormat::Text,
            working_directory: Some(cwd.clone()),
            access: options.access,
            resume: None,
            timeout: None,
            // The probe exists for `validate`, which never spawns a
            // process, so no environment is attached.
            env: std::collections::BTreeMap::new(),
            optimize: Default::default(),
            interaction_handler: None,
        };
        self.execution
            .adapter()
            .validate(&probe)
            .map_err(session::execution_error)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let session_id = SessionId::new(format!("cli-session-{}-{id}", std::process::id()));
        let resume_cursor = options.resume_cursor;
        let session = session::CliSession::new(
            integration.id.clone(),
            cwd.clone(),
            options.selection,
            options.access,
            resume_cursor.clone(),
            options.external_tools,
            options.env,
            options.optimize,
        );
        session.emit(AgentEventKind::SessionCreated {
            integration_id: integration.id.clone(),
            cwd,
        });
        session.emit(AgentEventKind::SessionStatus {
            status: SessionStatus::Idle,
        });
        self.sessions
            .lock()
            .expect("sessions mutex")
            .insert(session_id.clone(), session);
        Ok(AgentSessionHandle {
            session_id,
            provider_session: resume_cursor,
        })
    }

    fn send(
        &self,
        handle: &AgentSessionHandle,
        input: UserInput,
    ) -> Result<RunId, AgentRuntimeError> {
        self.session(&handle.session_id)?.send_run(
            &self.execution,
            self.supports_jsonl,
            self.supports_resume,
            self.supports_interaction,
            input,
        )
    }

    fn cancel(&self, handle: &AgentSessionHandle, run: RunId) -> Result<(), AgentRuntimeError> {
        let session = self.session(&handle.session_id)?;
        let state = session.state.lock().expect("session state mutex");
        match &state.active_run {
            Some(active) if active.run_id == run => {
                active.cancellation.cancel();
                Ok(())
            }
            _ => Err(session::invalid_state(
                "no in-flight Agent Run with that id exists for this session",
            )),
        }
    }

    fn answer(
        &self,
        handle: &AgentSessionHandle,
        request: RequestId,
        decision: ApprovalDecision,
    ) -> Result<(), AgentRuntimeError> {
        self.session(&handle.session_id)?
            .answer(&request, &decision)
    }

    fn events(&self, handle: &AgentSessionHandle) -> AgentEventStream {
        crate::local_adapter::event_stream(&self.sessions, &handle.session_id)
    }

    fn checkpoint(
        &self,
        _handle: &AgentSessionHandle,
        _run: RunId,
    ) -> Result<CheckpointId, AgentRuntimeError> {
        Err(AgentRuntimeError::unsupported(
            "checkpoints are owned by the runtime, not this adapter",
        ))
    }

    fn restore_checkpoint(
        &self,
        _handle: &AgentSessionHandle,
        _checkpoint: CheckpointId,
    ) -> Result<(), AgentRuntimeError> {
        Err(AgentRuntimeError::unsupported(
            "checkpoints are owned by the runtime, not this adapter",
        ))
    }

    fn stop(&self, handle: AgentSessionHandle) -> Result<(), AgentRuntimeError> {
        crate::local_adapter::stop(&self.sessions, &handle.session_id)
    }
}
