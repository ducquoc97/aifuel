//! The [`AgentAdapter`] contract implementation for [`ClaudeAdapter`].
//!
//! Every method enforces the serving integration identity first, maps
//! request-side failures onto receipt codes, and reports only what the
//! provider's wire evidence supports. Unsupported surfaces
//! (`checkpoint`, `restore_checkpoint`) answer with explicit
//! `unsupported` receipts rather than silent no-ops.

use super::session::ClaudeSession;
use super::{ClaudeAdapter, descriptors, session};
use aifuel_core::{
    AdapterCapabilities, AgentAdapter, AgentEventStream, AgentRuntimeError, AgentSessionHandle,
    ApprovalDecision, CapabilityState, CheckpointId, Effort, ExecutionAvailability, Integration,
    ModelDescriptor, ModelSelection, ReceiptCode, RequestId, RunId, SessionId, StartOptions,
    UserInput,
};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

/// The effort levels `--effort` accepts on every Claude model. The
/// contract's closed effort set maps one-to-one; the provider's extra
/// `xhigh` rung is not selectable through the contract and is never
/// offered.
const SELECTABLE_EFFORTS: &[Effort] = &[Effort::Low, Effort::Medium, Effort::High, Effort::Max];

impl AgentAdapter for ClaudeAdapter {
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
                // The `--effort` flag accepts these spellings for every
                // model; that is the only effort evidence the provider
                // exposes.
                efforts: SELECTABLE_EFFORTS.to_vec(),
                // Claude exposes no model catalog interface, so no
                // selection is ever advertised here.
                advertised: false,
                entitled: CapabilityState::Unknown,
                availability,
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
        // Claude exposes no catalog interface and no per-model
        // entitlement evidence, so the advertised list stays empty
        // rather than fabricating model ids.
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
            // `unknown` does not block: no evidence claimed the
            // integration could not run, and the run itself reports
            // the outcome.
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
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let session_id = SessionId::new(format!("claude-session-{}-{id}", std::process::id()));
        let setup = session::SessionSetup {
            cwd: cwd.clone(),
            access: options.access,
            model: (!options.selection.model.is_empty()).then(|| options.selection.model.clone()),
            effort: options.selection.effort,
            resume_cursor: options.resume_cursor,
        };
        let session = ClaudeSession::new(
            integration.id.clone(),
            cwd,
            options.access,
            options.selection,
        );
        let (setup_tx, setup_rx) = mpsc::channel();
        session.start_driver(self.connector.clone(), setup, setup_tx)?;
        let report = setup_rx.recv_timeout(session::SESSION_SETUP_TIMEOUT + Duration::from_secs(5));
        match report {
            Ok(Ok(())) => {
                session.announce();
                let provider_session = session.resume_cursor();
                self.sessions
                    .lock()
                    .expect("sessions mutex")
                    .insert(session_id.as_str().to_owned(), session);
                Ok(AgentSessionHandle {
                    session_id,
                    provider_session,
                })
            }
            Ok(Err(message)) => {
                session.close_transport();
                Err(AgentRuntimeError::provider_error(message))
            }
            Err(_) => {
                session.close_transport();
                Err(AgentRuntimeError::provider_error(
                    "the session driver did not report setup",
                ))
            }
        }
    }

    fn send(
        &self,
        handle: &AgentSessionHandle,
        input: UserInput,
    ) -> Result<RunId, AgentRuntimeError> {
        self.session(&handle.session_id)?.send_turn(input)
    }

    fn cancel(&self, handle: &AgentSessionHandle, run: RunId) -> Result<(), AgentRuntimeError> {
        self.session(&handle.session_id)?.cancel(&run)
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
        let sessions = self.sessions.lock().expect("sessions mutex");
        match sessions.get(handle.session_id.as_str()) {
            Some(session) => session
                .take_events()
                .map(|events| Box::new(events.into_iter()) as AgentEventStream)
                .unwrap_or_else(|| Box::new(std::iter::empty())),
            None => Box::new(std::iter::empty()),
        }
    }

    fn checkpoint(
        &self,
        _handle: &AgentSessionHandle,
        _run: RunId,
    ) -> Result<CheckpointId, AgentRuntimeError> {
        Err(AgentRuntimeError::unsupported(
            "the Claude adapter does not record checkpoints; the runtime owns that feature",
        ))
    }

    fn restore_checkpoint(
        &self,
        _handle: &AgentSessionHandle,
        _checkpoint: CheckpointId,
    ) -> Result<(), AgentRuntimeError> {
        Err(AgentRuntimeError::unsupported(
            "the Claude adapter does not record checkpoints; the runtime owns that feature",
        ))
    }

    fn stop(&self, handle: AgentSessionHandle) -> Result<(), AgentRuntimeError> {
        let session = self
            .sessions
            .lock()
            .expect("sessions mutex")
            .remove(handle.session_id.as_str())
            .ok_or_else(|| {
                AgentRuntimeError::new(
                    ReceiptCode::UnknownSession,
                    "the session id is not live in this adapter",
                )
            })?;
        session.shutdown();
        Ok(())
    }
}
