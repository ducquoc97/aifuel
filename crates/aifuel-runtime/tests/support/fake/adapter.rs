//! The [`FakeAdapter`] surface: the canned [`AgentAdapter`] plus the
//! facade's [`RuntimeAdapter`] session-state operations.

use super::{FakeScript, FakeSession, run_script};
use crate::support::{FAKE_INTEGRATION, FAKE_PROVIDER};
use aifuel_core::{
    AdapterCapabilities, AgentAdapter, AgentAuthenticationEvidence, AgentAuthenticationState,
    AgentEventKind, AgentEventStream, AgentIntegrationInfo, AgentPresenceState, AgentRuntimeError,
    AgentSessionHandle, AgentVersionEvidence, ApprovalDecision, CheckpointId, Integration,
    IntegrationId, ModelDescriptor, ModelSelection, ProviderId, ReceiptCode, RequestId, RunId,
    SessionId, SessionStatus, StartOptions, UserInput,
};
use aifuel_runtime::RuntimeAdapter;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

/// A scripted [`RuntimeAdapter`]: one integration, canned model
/// descriptors, and per-`send` run scripts.
pub struct FakeAdapter {
    capabilities: AdapterCapabilities,
    models: Vec<ModelDescriptor>,
    scripts: Mutex<VecDeque<FakeScript>>,
    sessions: Mutex<BTreeMap<String, Arc<FakeSession>>>,
    next_id: AtomicU64,
    /// When set, every `start` fails: the resume reconcile's failure path.
    fail_start: bool,
}

impl FakeAdapter {
    /// An adapter serving `fake-cli` with the given capability flags and
    /// advertised models.
    pub fn new(capabilities: AdapterCapabilities, models: Vec<ModelDescriptor>) -> Self {
        Self {
            capabilities,
            models,
            scripts: Mutex::new(VecDeque::new()),
            sessions: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(0),
            fail_start: false,
        }
    }

    /// Queue the run scripts `send` calls consume in order.
    pub fn with_scripts(mut self, scripts: Vec<FakeScript>) -> Self {
        self.scripts = Mutex::new(scripts.into());
        self
    }

    /// Make every `start` fail: the resume reconcile's failure path.
    pub fn failing_to_start(mut self) -> Self {
        self.fail_start = true;
        self
    }

    /// The honest capability set most tests want: streaming plus resume.
    pub fn default_capabilities() -> AdapterCapabilities {
        AdapterCapabilities {
            streaming: true,
            resume: true,
            approvals: false,
            checkpoints: false,
            effort: true,
            images: false,
            todos: false,
        }
    }

    /// One canned advertised model descriptor.
    pub fn model(model: &str, efforts: &[aifuel_core::Effort]) -> ModelDescriptor {
        ModelDescriptor {
            provider: ProviderId::new(FAKE_PROVIDER),
            model: model.to_owned(),
            label: format!("Fake {model}"),
            efforts: efforts.to_vec(),
            advertised: true,
            entitled: aifuel_core::CapabilityState::Unknown,
            availability: aifuel_core::ExecutionAvailability::Ready,
            quota: None,
        }
    }

    fn serve_check(&self, integration: &IntegrationId) -> Result<(), AgentRuntimeError> {
        if *integration == self.integration() {
            Ok(())
        } else {
            Err(AgentRuntimeError::unsupported(format!(
                "this adapter serves integration {FAKE_INTEGRATION} only"
            )))
        }
    }

    fn session(&self, session_id: &SessionId) -> Result<Arc<FakeSession>, AgentRuntimeError> {
        self.sessions
            .lock()
            .expect("sessions mutex")
            .get(session_id.as_str())
            .cloned()
            .ok_or_else(|| {
                AgentRuntimeError::new(
                    ReceiptCode::UnknownSession,
                    "the session id is not live in this adapter",
                )
            })
    }
}

impl AgentAdapter for FakeAdapter {
    fn capabilities(&self) -> AdapterCapabilities {
        self.capabilities
    }

    fn resolve(&self, selection: &ModelSelection) -> Result<ModelDescriptor, AgentRuntimeError> {
        self.serve_check(&selection.integration_id)?;
        let descriptor = self
            .models
            .iter()
            .find(|model| model.model == selection.model)
            .cloned()
            .unwrap_or_else(|| Self::model(&selection.model, &[]));
        if let Some(effort) = selection.effort
            && !descriptor.efforts.is_empty()
            && !descriptor.efforts.contains(&effort)
        {
            return Err(AgentRuntimeError::new(
                ReceiptCode::InvalidSelection,
                "the requested effort is not selectable for this model",
            ));
        }
        Ok(descriptor)
    }

    fn list_models(
        &self,
        integration: &Integration,
    ) -> Result<Vec<ModelDescriptor>, AgentRuntimeError> {
        self.serve_check(&integration.id)?;
        Ok(self.models.clone())
    }

    fn start(
        &self,
        integration: &Integration,
        options: StartOptions,
    ) -> Result<AgentSessionHandle, AgentRuntimeError> {
        if self.fail_start {
            return Err(AgentRuntimeError::provider_error(
                "the fake adapter refuses to start",
            ));
        }
        self.serve_check(&integration.id)?;
        if options.selection.integration_id != integration.id {
            return Err(AgentRuntimeError::new(
                ReceiptCode::InvalidSelection,
                "the selection names a different integration than the session",
            ));
        }
        if !options.cwd.is_dir() {
            return Err(AgentRuntimeError::new(
                ReceiptCode::InvalidState,
                "the session working directory does not exist",
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let session_id = SessionId::new(format!("fake-session-{}-{id}", std::process::id()));
        // A reconcile resume seeds the persisted cursor the way the real
        // `CliSession` does, so shutdown reads it back unchanged.
        let resume_cursor = options.resume_cursor;
        let session = FakeSession::new(options.selection, resume_cursor.clone());
        session.emit(AgentEventKind::SessionCreated {
            integration_id: integration.id.clone(),
            cwd: options.cwd,
        });
        session.emit(AgentEventKind::SessionStatus {
            status: SessionStatus::Idle,
        });
        self.sessions
            .lock()
            .expect("sessions mutex")
            .insert(session_id.as_str().to_owned(), session);
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
        if input.text.trim().is_empty() {
            return Err(AgentRuntimeError::new(
                ReceiptCode::InvalidState,
                "input text must not be empty",
            ));
        }
        let session = self.session(&handle.session_id)?;
        if session.closed.load(Ordering::Acquire) || session.run_active.swap(true, Ordering::AcqRel)
        {
            return Err(AgentRuntimeError::new(
                ReceiptCode::InvalidState,
                "the session is closed or a run is already in flight",
            ));
        }
        let selection = session.selection.lock().expect("selection mutex").clone();
        let run_id = RunId::new(format!(
            "fake-run-{}-{}",
            std::process::id(),
            self.next_id.fetch_add(1, Ordering::Relaxed)
        ));
        let script = self
            .scripts
            .lock()
            .expect("scripts mutex")
            .pop_front()
            .unwrap_or(FakeScript::Complete {
                deltas: Vec::new(),
                cursor: None,
            });
        // The provider session id is reported synchronously so shutdown
        // cursor reads are deterministic in tests.
        if let Some(cursor) = script.cursor() {
            *session.resume_cursor.lock().expect("resume cursor mutex") = Some(cursor.to_owned());
        }
        let worker = {
            let session = Arc::clone(&session);
            let run_id = run_id.clone();
            thread::spawn(move || run_script(session, run_id, selection, script))
        };
        // The worker is detached; `stop` flips `closed`/`cancelled` which
        // the script honors.
        drop(worker);
        Ok(run_id)
    }

    fn cancel(&self, handle: &AgentSessionHandle, _run: RunId) -> Result<(), AgentRuntimeError> {
        let session = self.session(&handle.session_id)?;
        if !session.run_active.load(Ordering::Acquire) {
            return Err(AgentRuntimeError::new(
                ReceiptCode::InvalidState,
                "no in-flight Agent Run exists for this session",
            ));
        }
        session.cancelled.store(true, Ordering::Release);
        Ok(())
    }

    fn answer(
        &self,
        handle: &AgentSessionHandle,
        request: RequestId,
        decision: ApprovalDecision,
    ) -> Result<(), AgentRuntimeError> {
        let session = self.session(&handle.session_id)?;
        let pending = session
            .pending
            .lock()
            .expect("pending mutex")
            .remove(request.as_str());
        match pending {
            Some(sender) => sender.send(decision).map_err(|_| {
                AgentRuntimeError::new(
                    ReceiptCode::AlreadyResolved,
                    "the Approval Request already left the run",
                )
            }),
            None => Err(AgentRuntimeError::new(
                ReceiptCode::AlreadyResolved,
                "the Approval Request is not pending",
            )),
        }
    }

    fn events(&self, handle: &AgentSessionHandle) -> AgentEventStream {
        let sessions = self.sessions.lock().expect("sessions mutex");
        sessions
            .get(handle.session_id.as_str())
            .and_then(|session| session.events.lock().expect("events mutex").take())
            .map(|events| Box::new(events.into_iter()) as AgentEventStream)
            .unwrap_or_else(|| Box::new(std::iter::empty()))
    }

    fn checkpoint(
        &self,
        _handle: &AgentSessionHandle,
        _run: RunId,
    ) -> Result<CheckpointId, AgentRuntimeError> {
        Err(AgentRuntimeError::unsupported(
            "the fake adapter does not record checkpoints",
        ))
    }

    fn restore_checkpoint(
        &self,
        _handle: &AgentSessionHandle,
        _checkpoint: CheckpointId,
    ) -> Result<(), AgentRuntimeError> {
        Err(AgentRuntimeError::unsupported(
            "the fake adapter does not record checkpoints",
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
        session.closed.store(true, Ordering::Release);
        session.cancelled.store(true, Ordering::Release);
        // Dropping the pending senders wakes a parked interaction with a
        // closed channel, matching the real session's teardown.
        session.pending.lock().expect("pending mutex").clear();
        session.emit(AgentEventKind::SessionClosed { reason: None });
        Ok(())
    }
}

impl Drop for FakeAdapter {
    /// Dropping the adapter cancels in-flight runs so parked workers exit
    /// instead of leaking.
    fn drop(&mut self) {
        for session in self.sessions.lock().expect("sessions mutex").values() {
            session.closed.store(true, Ordering::Release);
            session.cancelled.store(true, Ordering::Release);
            session.pending.lock().expect("pending mutex").clear();
        }
    }
}

impl RuntimeAdapter for FakeAdapter {
    fn integration(&self) -> IntegrationId {
        IntegrationId::new(FAKE_INTEGRATION)
    }

    fn provider(&self) -> ProviderId {
        ProviderId::new(FAKE_PROVIDER)
    }

    fn agent_info(&self) -> AgentIntegrationInfo {
        AgentIntegrationInfo::from_inspection(
            self.provider(),
            self.integration(),
            aifuel_core::AgentPresenceEvidence {
                state: AgentPresenceState::Present,
                reason: "fake".to_owned(),
            },
            AgentVersionEvidence {
                version: Some("1.0".to_owned()),
                reason: "fake".to_owned(),
            },
            AgentAuthenticationEvidence {
                state: AgentAuthenticationState::Authenticated,
                reason: "fake".to_owned(),
            },
            Vec::new(),
        )
    }

    fn resume_cursor(&self, session_id: &SessionId) -> Option<String> {
        self.sessions
            .lock()
            .expect("sessions mutex")
            .get(session_id.as_str())
            .and_then(|session| {
                session
                    .resume_cursor
                    .lock()
                    .expect("resume cursor mutex")
                    .clone()
            })
    }

    fn set_selection(
        &self,
        handle: &AgentSessionHandle,
        selection: ModelSelection,
    ) -> Result<ModelDescriptor, AgentRuntimeError> {
        let descriptor = self.resolve(&selection)?;
        let session = self.session(&handle.session_id)?;
        if session.run_active.load(Ordering::Acquire) {
            return Err(AgentRuntimeError::new(
                ReceiptCode::InvalidState,
                "the selection cannot change while a run is in flight",
            ));
        }
        *session.selection.lock().expect("selection mutex") = selection;
        Ok(descriptor)
    }
}
