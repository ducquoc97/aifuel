//! The [`AgentAdapter`] contract implementation for [`AcpAdapter`].
//!
//! Every method enforces the serving integration identity first, maps
//! request-side failures onto receipt codes, and reports only what the
//! agent's evidence supports. `start` spawns the agent process and runs
//! the setup handshake on the session's driver thread before announcing
//! the session; a rejected handshake fails `start` and the process is
//! reaped. Unsupported surfaces (`checkpoint`, `restore_checkpoint`)
//! answer with explicit `unsupported` receipts rather than silent
//! no-ops.

use super::{AcpAdapter, session};
use aifuel_core::{
    AdapterCapabilities, AgentAdapter, AgentEventStream, AgentRuntimeError, AgentSessionHandle,
    ApprovalDecision, CapabilityState, CheckpointId, ExecutionAvailability, Integration,
    ModelDescriptor, ModelSelection, ReceiptCode, RequestId, RunId, SessionId, StartOptions,
    UserInput,
};
use std::sync::atomic::Ordering;
use std::sync::mpsc;

impl AgentAdapter for AcpAdapter {
    fn capabilities(&self) -> AdapterCapabilities {
        self.capabilities
    }

    fn resolve(&self, selection: &ModelSelection) -> Result<ModelDescriptor, AgentRuntimeError> {
        self.ensure_serves(&selection.integration_id)?;
        // ACP advertises selectable models per session through
        // `configOptions`; before a session exists nothing advertises
        // this model, so it stays unadvertised with unknown
        // availability rather than guessed ready.
        let descriptor = ModelDescriptor {
            provider: self.provider(),
            model: selection.model.clone(),
            label: selection.model.clone(),
            efforts: Vec::new(),
            advertised: false,
            entitled: CapabilityState::Unknown,
            availability: ExecutionAvailability::Unknown,
            quota: None,
        };
        if let Some(effort) = selection.effort {
            return Err(AgentRuntimeError::new(
                ReceiptCode::InvalidSelection,
                format!(
                    "{} is not a selectable effort for {}; ACP has no effort selection",
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
        // ACP carries the agent's model menu inside the session's
        // `configOptions`; there is no pre-session catalog evidence to
        // merge, so the honest list is empty.
        Ok(Vec::new())
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
        // Resolve before spawn so an unselectable effort fails before
        // any process exists. An unadvertised model still resolves: the
        // session's advertised selector validates it at setup time.
        self.resolve(&options.selection)?;
        // The protocol offers no way to restrict the agent's tool set,
        // so a non-empty selection fails before spawn rather than
        // silently running wider than asked.
        if !options.external_tools.is_empty() {
            return Err(AgentRuntimeError::unsupported(
                "the ACP adapter cannot enforce an exact external tool selection",
            ));
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let session_id = SessionId::new(format!("acp-session-{}-{id}", std::process::id()));
        let session = session::AcpSession::new(
            integration.id.clone(),
            cwd.clone(),
            options.access,
            options.selection.clone(),
        );
        let (setup_tx, setup_rx) = mpsc::channel();
        session.start_driver(
            self.connector.clone(),
            session::SessionSetup {
                program: self.program.clone(),
                args: self.args.clone(),
                cwd,
                model: (!options.selection.model.is_empty())
                    .then(|| options.selection.model.clone()),
                resume_cursor: options.resume_cursor.filter(|cursor| !cursor.is_empty()),
                env: options.env,
            },
            setup_tx,
        )?;
        let opened = match setup_rx.recv_timeout(session::SESSION_SETUP_TIMEOUT) {
            Ok(Ok(opened)) => opened,
            Ok(Err(reason)) => {
                session.shutdown();
                return Err(AgentRuntimeError::provider_error(reason));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                session.kill_child();
                session.shutdown();
                return Err(AgentRuntimeError::provider_error(
                    "the agent setup handshake did not complete",
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
            provider_session: Some(opened.session_id),
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
