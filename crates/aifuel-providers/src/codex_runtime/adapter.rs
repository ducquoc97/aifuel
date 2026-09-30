//! The [`AgentAdapter`] contract implementation for [`CodexAdapter`].
//!
//! Every method enforces the serving integration identity first, maps
//! request-side failures onto receipt codes, and reports only what the
//! provider's evidence supports. `start` spawns the app-server process
//! and runs the setup handshake on the session's driver thread before
//! announcing the session; a rejected handshake fails `start` and the
//! process is reaped. Unsupported surfaces (`checkpoint`,
//! `restore_checkpoint`) answer with explicit `unsupported` receipts
//! rather than silent no-ops.

use super::{CodexAdapter, session};
use crate::local_adapter::descriptors;
use aifuel_core::{
    AdapterCapabilities, AgentAdapter, AgentEventStream, AgentRuntimeError, AgentSessionHandle,
    ApprovalDecision, CapabilityState, CheckpointId, ExecutionAvailability, Integration,
    ModelDescriptor, ModelSelection, ReceiptCode, RequestId, RunId, SessionId, StartOptions,
    UserInput,
};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc;

impl AgentAdapter for CodexAdapter {
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
                // No catalog evidence advertises this model.
                advertised: false,
                entitled: CapabilityState::Unknown,
                // Nothing verified the provider serves this model, so
                // the integration's readiness does not transfer to it.
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
        // The app-server session carries no per-model entitlement
        // evidence and no monitoring contract, so entitlement stays
        // `unknown` and quota `None`.
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
            // integration could not run, and the session itself reports
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
        // Resolve before spawn so an unselectable effort or an
        // unadvertised model fails before any process exists. An
        // unadvertised model still resolves: the catalog is the only
        // advertisement evidence, and the provider validates the model
        // at `thread/start` itself.
        self.resolve(&options.selection)?;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let session_id = SessionId::new(format!("codex-session-{}-{id}", std::process::id()));
        let session = session::CodexSession::new(
            integration.id.clone(),
            cwd.clone(),
            options.access,
            options.selection.clone(),
        );
        let (setup_tx, setup_rx) = mpsc::channel();
        session.start_driver(
            self.connector.clone(),
            session::SessionSetup {
                cwd,
                access: options.access,
                model: (!options.selection.model.is_empty())
                    .then(|| options.selection.model.clone()),
                resume_cursor: options.resume_cursor,
            },
            setup_tx,
        )?;
        let thread_id = match setup_rx.recv_timeout(session::SESSION_SETUP_TIMEOUT) {
            Ok(Ok(thread_id)) => thread_id,
            Ok(Err(reason)) => {
                session.shutdown();
                return Err(AgentRuntimeError::provider_error(reason));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                session.kill_child();
                session.shutdown();
                return Err(AgentRuntimeError::provider_error(
                    "the app-server setup handshake did not complete",
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                session.shutdown();
                return Err(AgentRuntimeError::provider_error(
                    "the session driver ended before setup completed",
                ));
            }
        };
        {
            let mut state = session.state.lock().expect("session state mutex");
            state.thread_id = Some(thread_id.clone());
        }
        session.announce();
        self.sessions
            .lock()
            .expect("sessions mutex")
            .insert(session_id.clone(), session);
        Ok(AgentSessionHandle {
            session_id,
            provider_session: Some(thread_id),
        })
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
