//! The [`AgentAdapter`] contract implementation for [`OpenCodeAdapter`].
//!
//! Every method enforces the serving integration identity first, maps
//! request-side failures onto receipt codes, and reports only what the
//! provider's evidence supports. `start` spawns the serve process and
//! runs the setup handshake on the session's driver thread before
//! announcing the session; a rejected handshake fails `start` and the
//! process is reaped. Unsupported surfaces (`checkpoint`,
//! `restore_checkpoint`) answer with explicit `unsupported` receipts
//! rather than silent no-ops.

use super::{OpenCodeAdapter, protocol, session};
use crate::local_adapter::descriptors;
use aifuel_core::{
    AdapterCapabilities, AgentAdapter, AgentEventStream, AgentRuntimeError, AgentSessionHandle,
    ApprovalDecision, CapabilityState, CheckpointId, ExecutionAvailability, Integration,
    ModelDescriptor, ModelSelection, ReceiptCode, RequestId, RunId, SessionId, StartOptions,
    UserInput,
};
use std::sync::atomic::Ordering;
use std::sync::mpsc;

impl AgentAdapter for OpenCodeAdapter {
    fn capabilities(&self) -> AdapterCapabilities {
        self.capabilities
    }

    fn resolve(&self, selection: &ModelSelection) -> Result<ModelDescriptor, AgentRuntimeError> {
        self.ensure_serves(&selection.integration_id)?;
        // OpenCode spells model references `provider/model`; validate the
        // shape before any descriptor claims the model.
        protocol::model_ref(&selection.model)?;
        let availability = self.availability();
        let descriptor = self
            .catalog_models()
            .iter()
            .find(|model| model.model_id == selection.model)
            .map(|model| {
                descriptors::catalog_descriptor(
                    &self.provider(),
                    model,
                    self.entitlements()
                        .get(&model.model_id)
                        .copied()
                        .unwrap_or(CapabilityState::Unknown),
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
        // `/provider` advertises every known model and reports the
        // connected provider ids; that connection is the only
        // entitlement evidence the serve exposes. Quota stays `None`.
        Ok(descriptors::model_descriptors(
            self.provider(),
            self.catalog_models(),
            &self.entitlements(),
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
        // Resolve before spawn so a malformed `provider/model` spelling
        // or an unselectable effort fails before any process exists.
        self.resolve(&options.selection)?;
        // The session API offers no way to restrict the server's tool
        // set, so a non-empty selection fails before spawn rather than
        // silently running wider than asked.
        if !options.external_tools.is_empty() {
            return Err(AgentRuntimeError::unsupported(
                "the OpenCode session API adapter cannot enforce an exact external tool selection",
            ));
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let session_id = SessionId::new(format!("opencode-session-{}-{id}", std::process::id()));
        let session = session::OpenCodeSession::new(
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
                resume_cursor: options.resume_cursor,
            },
            setup_tx,
        )?;
        let provider_session = match setup_rx.recv_timeout(session::SESSION_SETUP_TIMEOUT) {
            Ok(Ok(provider_session)) => provider_session,
            Ok(Err(reason)) => {
                session.shutdown();
                return Err(AgentRuntimeError::provider_error(reason));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                session.kill_child();
                session.shutdown();
                return Err(AgentRuntimeError::provider_error(
                    "the opencode setup handshake did not complete",
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                session.shutdown();
                return Err(AgentRuntimeError::provider_error(
                    "the session driver ended before setup completed",
                ));
            }
        };
        session.announce();
        self.sessions
            .lock()
            .expect("sessions mutex")
            .insert(session_id.clone(), session);
        Ok(AgentSessionHandle {
            session_id,
            provider_session: Some(provider_session),
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
