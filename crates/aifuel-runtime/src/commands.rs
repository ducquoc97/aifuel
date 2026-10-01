//! The per-command implementations behind [`AgentRuntime::dispatch`].
//!
//! Session-scoped commands resolve the live adapter session first; a
//! persisted-but-not-live session fails `invalid_state` because there is
//! nothing to drive, and an unknown id fails `unknown_session`. The facade
//! authors `session.created` and `session.closed` itself so their receipts
//! carry the fact's `seq`; for `approval.answer` it registers the
//! answering consumer before the adapter resolves the request, so the
//! pump can attribute the adapter-emitted `approval.resolved` in the
//! run's causal order.

use crate::adapter::RuntimeAdapter;
use crate::dispatch::{CommandOutcome, CommandPayload, store_error};
use crate::pump;
use crate::runtime::{AgentRuntime, LiveSession};
use crate::{MAX_REPLAY_BYTES, MAX_REPLAY_EVENTS};
use aifuel_core::{
    AccessMode, AgentEventKind, AgentRuntimeError, AgentSessionHandle, ApprovalDecision, CommandId,
    ConsumerId, ExecutionAvailability, IntegrationId, ModelDescriptor, ModelSelection, Receipt,
    ReceiptCode, RequestId, RunId, Seq, SessionId, SessionStatus, StartOptions, UserInput,
};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

impl AgentRuntime {
    /// `session.create`: resolve the selection, start the adapter session,
    /// register the persisted projection, append `session.created`, and
    /// start the event pump. `resume_cursor` continues a provider session
    /// the host already holds; `external_tools` asks the adapter to enforce
    /// an exact AI Fuel Gateway tool set and fails `unsupported` when the
    /// serving adapter cannot - tool enforcement is never silently dropped.
    pub(crate) fn session_create(
        &self,
        command_id: CommandId,
        cwd: PathBuf,
        selection: ModelSelection,
        access: AccessMode,
        resume_cursor: Option<String>,
        external_tools: Vec<String>,
    ) -> CommandOutcome {
        let Some((descriptor, instance)) = self.registry.serving(&selection.integration_id) else {
            return CommandOutcome::rejected(
                command_id,
                ReceiptCode::InvalidSelection,
                format!(
                    "no Provider Integration or Instance is registered as {}",
                    selection.integration_id
                ),
            );
        };
        let Some(adapter) = self.registry.adapter_for(&selection.integration_id) else {
            return CommandOutcome::rejected(
                command_id,
                ReceiptCode::Unsupported,
                format!("no adapter serves integration {}", selection.integration_id),
            );
        };
        if !external_tools.is_empty() && !adapter.capabilities().external_tools {
            return CommandOutcome::rejected(
                command_id,
                ReceiptCode::Unsupported,
                format!(
                    "the adapter serving {} cannot enforce external tools",
                    selection.integration_id
                ),
            );
        }
        // The instance's environment spec resolves against the Credential
        // Store now: a missing or mismatched Managed Credential fails the
        // command before any provider process exists, and the resolved
        // overlay travels inside `StartOptions` only - the persisted
        // session row carries the instance id, never the values.
        let env = match instance {
            Some(instance) => match instance.resolve_env(self.registry.credentials()) {
                Ok(env) => env,
                Err(error) => {
                    return CommandOutcome::rejected(
                        command_id,
                        ReceiptCode::InvalidSelection,
                        format!("instance {}: {error}", instance.id),
                    );
                }
            },
            None => std::collections::BTreeMap::new(),
        };
        // Adapters and descriptors serve base integration identities, so the
        // adapter-facing selection carries the base id. The persisted
        // selection keeps the id the host sent, so `model.select` and
        // reconcile see `claude.work`, not a silently rewritten `claude`.
        let adapter_selection = ModelSelection {
            integration_id: descriptor.integration.id.clone(),
            model: selection.model.clone(),
            effort: selection.effort,
        };
        // Resolve before start, per the contract: a selection that cannot
        // resolve - or resolves to a model that cannot run - fails
        // `invalid_selection` here rather than failing a run later.
        if let Err(error) = resolve_ready(&*adapter, &adapter_selection) {
            return CommandOutcome::rejected(command_id, error.code, error.message);
        }
        let handle = match adapter.start(
            &descriptor.integration,
            StartOptions {
                cwd: cwd.clone(),
                selection: adapter_selection,
                access,
                resume_cursor,
                external_tools: external_tools.clone(),
                env,
            },
        ) {
            Ok(handle) => handle,
            Err(error) => return CommandOutcome::rejected(command_id, error.code, error.message),
        };
        let cleanup = |handle: AgentSessionHandle| {
            let _ = adapter.stop(handle);
        };
        if let Err(error) =
            self.store
                .record_agent_session(&handle.session_id, &selection, &cwd, &external_tools)
        {
            cleanup(handle);
            return CommandOutcome::rejected(
                command_id,
                ReceiptCode::ProviderError,
                store_error(error).message,
            );
        }
        // The facade authors the lifecycle fact so the receipt can carry
        // its seq; the pump skips the adapter's own copy.
        let created = match self.store.append(
            &handle.session_id,
            AgentEventKind::SessionCreated {
                integration_id: selection.integration_id.clone(),
                cwd: cwd.clone(),
            },
        ) {
            Ok(event) => event,
            Err(error) => {
                cleanup(handle);
                return CommandOutcome::rejected(
                    command_id,
                    ReceiptCode::ProviderError,
                    store_error(error).message,
                );
            }
        };
        let join = match pump::spawn(
            self.store.clone(),
            Arc::downgrade(&self.inner),
            handle.session_id.clone(),
            Arc::clone(&adapter),
            handle.clone(),
            false,
        ) {
            Ok(join) => join,
            Err(error) => {
                cleanup(handle);
                return CommandOutcome::rejected(
                    command_id,
                    ReceiptCode::ProviderError,
                    format!("the session event pump could not start: {error}"),
                );
            }
        };
        self.inner.lock().expect("runtime mutex").live.insert(
            handle.session_id.clone(),
            LiveSession {
                adapter,
                handle: handle.clone(),
                subscribers: BTreeSet::new(),
                pump: Some(join),
                closed: false,
                access,
                cwd,
                monitoring: descriptor.integration.monitoring.is_some(),
                serving: selection.integration_id.clone(),
            },
        );
        CommandOutcome::ok(Receipt::ok(
            command_id,
            created.seq,
            Some(handle.session_id),
            None,
        ))
    }

    /// `session.subscribe`: attach the consumer and replay events after
    /// `last_seen_seq`. Replayed events land on the consumer's channel in
    /// order before any live event; when the lag exceeds the replay bounds
    /// the runtime skips replay and the receipt carries a fresh snapshot.
    pub(crate) fn session_subscribe(
        &self,
        command_id: CommandId,
        session_id: SessionId,
        last_seen_seq: Seq,
        consumer_id: &ConsumerId,
    ) -> CommandOutcome {
        match self.store.agent_session(&session_id) {
            Ok(Some(_)) => {}
            Ok(None) => {
                return CommandOutcome::rejected(
                    command_id,
                    ReceiptCode::UnknownSession,
                    "no Agent Session with that id is known",
                );
            }
            Err(error) => {
                return CommandOutcome::rejected(
                    command_id,
                    ReceiptCode::ProviderError,
                    store_error(error).message,
                );
            }
        }
        let mut inner = self.inner.lock().expect("runtime mutex");
        // The subscription registers under the same lock the pump appends
        // under, so replayed events and live events cannot interleave.
        let consumer = Self::consumer(&mut inner, consumer_id);
        let sender = consumer.sender.clone();
        let page = match self.store.replay(
            &session_id,
            last_seen_seq,
            MAX_REPLAY_EVENTS,
            MAX_REPLAY_BYTES,
        ) {
            Ok(page) => page,
            Err(error) => {
                return CommandOutcome::rejected(
                    command_id,
                    ReceiptCode::ProviderError,
                    store_error(error).message,
                );
            }
        };
        if !page.truncated {
            for event in &page.events {
                let _ = sender.send(event.clone());
            }
        }
        if let Some(live) = inner.live.get_mut(&session_id) {
            live.subscribers.insert(consumer_id.clone());
        }
        let snapshot = match self.store.snapshot(&session_id) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return CommandOutcome::rejected(
                    command_id,
                    ReceiptCode::ProviderError,
                    store_error(error).message,
                );
            }
        };
        let Some(snapshot) = snapshot else {
            return CommandOutcome::rejected(
                command_id,
                ReceiptCode::ProviderError,
                "the session row disappeared mid-subscribe",
            );
        };
        CommandOutcome::ok(Receipt::ok(
            command_id,
            snapshot.head_seq,
            Some(session_id),
            Some(snapshot),
        ))
    }

    /// `session.list`: the persisted Agent Session read model.
    pub(crate) fn session_list(&self, command_id: CommandId) -> CommandOutcome {
        match self.store.agent_sessions() {
            Ok(sessions) => CommandOutcome::with_payload(
                Receipt::ok(command_id, 0, None, None),
                CommandPayload::Sessions(sessions),
            ),
            Err(error) => CommandOutcome::rejected(
                command_id,
                ReceiptCode::ProviderError,
                store_error(error).message,
            ),
        }
    }

    /// `session.close`: stop the live adapter session, let the pump drain,
    /// then append `session.closed` as the last fact. A persisted session
    /// that is not live closes by fact alone.
    pub(crate) fn session_close(
        &self,
        command_id: CommandId,
        session_id: SessionId,
    ) -> CommandOutcome {
        let live = {
            let mut inner = self.inner.lock().expect("runtime mutex");
            // The closing session cannot win new answer attributions.
            inner
                .answers
                .retain(|(session, _), _| session != &session_id);
            inner.live.remove(&session_id)
        };
        if let Some(live) = live {
            let already_closed = live.closed;
            if let Err(error) = live.adapter.stop(live.handle) {
                return CommandOutcome::rejected(command_id, error.code, error.message);
            }
            // The adapter's stream closed with the session; the pump drains
            // the queued events before exiting so `session.closed` lands
            // last in causal order.
            if let Some(pump) = live.pump {
                let _ = pump.join();
            }
            // An adapter-initiated close already recorded the fact; answer
            // with the log's head rather than appending a second copy.
            return if already_closed {
                self.head_seq_outcome(command_id, session_id)
            } else {
                self.append_session_closed(command_id, session_id)
            };
        }
        match self.store.agent_session(&session_id) {
            Ok(Some(session)) if session.status == SessionStatus::Closed => {
                CommandOutcome::rejected(
                    command_id,
                    ReceiptCode::InvalidState,
                    "the Agent Session is already closed",
                )
            }
            Ok(Some(_)) => self.append_session_closed(command_id, session_id),
            Ok(None) => CommandOutcome::rejected(
                command_id,
                ReceiptCode::UnknownSession,
                "no Agent Session with that id is known",
            ),
            Err(error) => CommandOutcome::rejected(
                command_id,
                ReceiptCode::ProviderError,
                store_error(error).message,
            ),
        }
    }

    /// Append `session.closed` as the session's last fact and answer with
    /// its seq.
    fn append_session_closed(
        &self,
        command_id: CommandId,
        session_id: SessionId,
    ) -> CommandOutcome {
        match self
            .store
            .append(&session_id, AgentEventKind::SessionClosed { reason: None })
        {
            Ok(event) => {
                CommandOutcome::ok(Receipt::ok(command_id, event.seq, Some(session_id), None))
            }
            Err(error) => CommandOutcome::rejected(
                command_id,
                ReceiptCode::ProviderError,
                store_error(error).message,
            ),
        }
    }

    /// `run.start`: send one input to the live session's adapter. The
    /// adapter's worker emits `run.started` in causal order; the receipt
    /// carries the accepted `run_id` so the host can issue `run.cancel`
    /// without racing the event stream, and its `seq` is the log head
    /// observed at return, which may precede the events the accepted run
    /// is about to produce.
    pub(crate) fn run_start(
        &self,
        command_id: CommandId,
        session_id: SessionId,
        input: UserInput,
    ) -> CommandOutcome {
        let Some((adapter, handle, _)) = self.live_session(&session_id) else {
            return self.not_live(&command_id, &session_id);
        };
        match adapter.send(&handle, input) {
            Ok(run_id) => match self.head_seq(&session_id) {
                Ok(seq) => CommandOutcome::ok(
                    Receipt::ok(command_id, seq, Some(session_id), None).with_run_id(run_id),
                ),
                Err(error) => CommandOutcome::rejected(command_id, error.code, error.message),
            },
            Err(error) => CommandOutcome::rejected(command_id, error.code, error.message),
        }
    }

    /// `run.cancel`: request cancellation of one in-flight Agent Run.
    pub(crate) fn run_cancel(
        &self,
        command_id: CommandId,
        session_id: SessionId,
        run_id: RunId,
    ) -> CommandOutcome {
        let Some((adapter, handle, _)) = self.live_session(&session_id) else {
            return self.not_live(&command_id, &session_id);
        };
        match adapter.cancel(&handle, run_id) {
            Ok(()) => self.head_seq_outcome(command_id, session_id),
            Err(error) => CommandOutcome::rejected(command_id, error.code, error.message),
        }
    }

    /// `approval.answer`: record the answering consumer, then deliver the
    /// decision to the adapter. The first answer wins; the adapter reports
    /// `already_resolved` for the rest. The adapter emits
    /// `approval.resolved` inside the run's causal order; the pump stamps
    /// the registered consumer onto `answered_by`.
    pub(crate) fn approval_answer(
        &self,
        command_id: CommandId,
        session_id: SessionId,
        request_id: RequestId,
        decision: ApprovalDecision,
        consumer_id: &ConsumerId,
    ) -> CommandOutcome {
        let Some((adapter, handle, _)) = self.live_session(&session_id) else {
            return self.not_live(&command_id, &session_id);
        };
        {
            let mut inner = self.inner.lock().expect("runtime mutex");
            inner
                .answers
                .entry((session_id.clone(), request_id.clone()))
                .or_insert_with(|| consumer_id.clone());
        }
        let result = adapter.answer(&handle, request_id.clone(), decision);
        if let Err(error) = result {
            // Keep the winner's attribution: only a registration this call
            // still owns is removed.
            let mut inner = self.inner.lock().expect("runtime mutex");
            if inner
                .answers
                .get(&(session_id.clone(), request_id.clone()))
                .is_some_and(|winner| winner == consumer_id)
            {
                inner
                    .answers
                    .remove(&(session_id.clone(), request_id.clone()));
            }
            return CommandOutcome::rejected(command_id, error.code, error.message);
        }
        self.head_seq_outcome(command_id, session_id)
    }

    /// `model.select`: resolve the selection, apply it to the live session,
    /// and persist it on the session projection. The contract has no
    /// `model.selected` event, so the change is receipt-only.
    pub(crate) fn model_select(
        &self,
        command_id: CommandId,
        session_id: SessionId,
        selection: ModelSelection,
    ) -> CommandOutcome {
        let Some((adapter, handle, serving)) = self.live_session(&session_id) else {
            return self.not_live(&command_id, &session_id);
        };
        // The selection must name the identity the session was created
        // through. `model.select` is not a session re-creation: switching
        // `claude.work` to `claude` (or between two instances of one base)
        // would change the provider's environment mid-session, so a
        // different id - even its own base - is `invalid_selection` rather
        // than a silent swap. An unknown id fails the same way.
        let Some((base, _)) = self.registry.serving(&selection.integration_id) else {
            return CommandOutcome::rejected(
                command_id,
                ReceiptCode::InvalidSelection,
                format!(
                    "no Provider Integration or Instance is registered as {}",
                    selection.integration_id
                ),
            );
        };
        if selection.integration_id != serving {
            return CommandOutcome::rejected(
                command_id,
                ReceiptCode::InvalidSelection,
                format!(
                    "the session was created through {serving}; {} is a different Provider Integration or Instance",
                    selection.integration_id
                ),
            );
        }
        // The adapter-facing selection carries the base integration id,
        // matching what `session.create` started the adapter with.
        let adapter_selection = ModelSelection {
            integration_id: base.integration.id.clone(),
            model: selection.model.clone(),
            effort: selection.effort,
        };
        // The same readiness gate as `session.create`: a selection that
        // resolves to a model that cannot run fails `invalid_selection`.
        if let Err(error) = resolve_ready(&*adapter, &adapter_selection) {
            return CommandOutcome::rejected(command_id, error.code, error.message);
        }
        if let Err(error) = adapter.set_selection(&handle, adapter_selection) {
            return CommandOutcome::rejected(command_id, error.code, error.message);
        }
        if let Err(error) = self
            .store
            .record_agent_session_selection(&session_id, &selection)
        {
            return CommandOutcome::rejected(
                command_id,
                ReceiptCode::ProviderError,
                store_error(error).message,
            );
        }
        self.head_seq_outcome(command_id, session_id)
    }

    /// The live adapter handle for a session the facade created, plus the
    /// Integration Identity - base or instance - the session was created
    /// through.
    fn live_session(
        &self,
        session_id: &SessionId,
    ) -> Option<(Arc<dyn RuntimeAdapter>, AgentSessionHandle, IntegrationId)> {
        self.inner
            .lock()
            .expect("runtime mutex")
            .live
            .get(session_id)
            .map(|live| {
                (
                    Arc::clone(&live.adapter),
                    live.handle.clone(),
                    live.serving.clone(),
                )
            })
    }

    /// The receipt for a session-scoped command that found no live session:
    /// `invalid_state` when the session is known to the log but cannot be
    /// driven, `unknown_session` when it is not known at all.
    fn not_live(&self, command_id: &CommandId, session_id: &SessionId) -> CommandOutcome {
        match self.store.agent_session(session_id) {
            Ok(Some(session)) => CommandOutcome::rejected(
                command_id.clone(),
                ReceiptCode::InvalidState,
                format!(
                    "the Agent Session is {} and not live in this runtime",
                    session.status.as_str()
                ),
            ),
            Ok(None) => CommandOutcome::rejected(
                command_id.clone(),
                ReceiptCode::UnknownSession,
                "no Agent Session with that id is known",
            ),
            Err(error) => CommandOutcome::rejected(
                command_id.clone(),
                ReceiptCode::ProviderError,
                store_error(error).message,
            ),
        }
    }

    /// The `ok` receipt for an accepted session-scoped command: the log's
    /// current head sequence for the session.
    fn head_seq_outcome(&self, command_id: CommandId, session_id: SessionId) -> CommandOutcome {
        match self.head_seq(&session_id) {
            Ok(seq) => CommandOutcome::ok(Receipt::ok(command_id, seq, Some(session_id), None)),
            Err(error) => CommandOutcome::rejected(command_id, error.code, error.message),
        }
    }

    /// The log's current head sequence for a session.
    fn head_seq(&self, session_id: &SessionId) -> Result<Seq, AgentRuntimeError> {
        self.store.head_seq(session_id).map_err(store_error)
    }
}

/// The readiness gate `session.create` and `model.select` share: resolve
/// the selection through the adapter, then reject an availability that
/// cannot run. `ready` proceeds and `unknown` proceeds too - an unprobed
/// provider would otherwise be unusable, and a genuinely broken execution
/// surfaces as `provider_error` at run start.
fn resolve_ready(
    adapter: &dyn RuntimeAdapter,
    selection: &ModelSelection,
) -> Result<ModelDescriptor, AgentRuntimeError> {
    let descriptor = adapter.resolve(selection)?;
    match descriptor.availability {
        ExecutionAvailability::Ready | ExecutionAvailability::Unknown => Ok(descriptor),
        ExecutionAvailability::NeedsAuth => Err(AgentRuntimeError::new(
            ReceiptCode::InvalidSelection,
            "the integration needs authentication before it can run",
        )),
        ExecutionAvailability::Unsupported => Err(AgentRuntimeError::new(
            ReceiptCode::InvalidSelection,
            "the integration cannot execute in this environment",
        )),
    }
}
