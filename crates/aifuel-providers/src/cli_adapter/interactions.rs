//! Provider interaction plumbing for the CLI fallback session.
//!
//! When an execution adapter raises an [`AgentInteractionRequest`], the
//! handler here turns it into an `approval.requested` event on the run,
//! parks the provider call, and wakes it with the host's
//! `approval.answer`. The event payload and the answer validation follow
//! the shared [approval policy](crate::local_adapter::approvals); what
//! stays here is the run-scoped transport: parking the provider call on
//! a channel inside the run's causal order.

use super::session::CliSession;
use crate::local_adapter::approvals;
use aifuel_core::{
    AgentEventKind, AgentInteractionHandler, AgentInteractionKind, AgentInteractionRequest,
    AgentInteractionResponse, AgentRunError, AgentRuntimeError, ApprovalDecision, RequestId,
    RunCancellationToken, RunId, SessionStatus,
};
use std::collections::BTreeMap;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long the interaction wait loop sleeps between cancellation checks.
const INTERACTION_POLL: Duration = Duration::from_millis(50);

/// One provider interaction waiting on a host answer.
pub(super) struct PendingInteraction {
    kind: AgentInteractionKind,
    /// Option ids the `approval.requested` payload offered.
    options: Vec<String>,
    /// Question ids a text decision fills, in order.
    question_ids: Vec<String>,
    /// The channel the worker waits on; carrying the host's decision keeps
    /// `approval.resolved` emission inside the run's causal order.
    pub response: mpsc::Sender<(ApprovalDecision, AgentInteractionResponse)>,
}

impl PendingInteraction {
    pub(super) fn new(
        request: &AgentInteractionRequest,
        options: Vec<String>,
        response: mpsc::Sender<(ApprovalDecision, AgentInteractionResponse)>,
    ) -> Self {
        Self {
            kind: request.kind,
            options,
            question_ids: request.question_ids(),
            response,
        }
    }

    /// Map the host's decision onto the provider-native response shape,
    /// enforcing the shared approval policy exactly as offered.
    pub(super) fn response_for(
        &self,
        decision: &ApprovalDecision,
    ) -> Result<AgentInteractionResponse, AgentRuntimeError> {
        approvals::decision_response(self.kind, &self.options, &self.question_ids, decision)
    }
}

/// Routes provider interaction requests through the session event stream
/// and blocks on the host's `approval.answer`.
pub(super) struct CliInteractionHandler {
    session: Arc<CliSession>,
    run_id: RunId,
    pending: Arc<Mutex<BTreeMap<RequestId, PendingInteraction>>>,
}

impl CliInteractionHandler {
    pub(super) fn new(
        session: Arc<CliSession>,
        run_id: RunId,
        pending: Arc<Mutex<BTreeMap<RequestId, PendingInteraction>>>,
    ) -> Self {
        Self {
            session,
            run_id,
            pending,
        }
    }
}

impl std::fmt::Debug for CliInteractionHandler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CliInteractionHandler")
    }
}

impl AgentInteractionHandler for CliInteractionHandler {
    fn interact(
        &self,
        request: AgentInteractionRequest,
        cancellation: &RunCancellationToken,
    ) -> Result<AgentInteractionResponse, AgentRunError> {
        let request_id = self.session.next_request_id();
        let payload = approvals::approval_request(&request, self.session.access);
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .expect("pending approvals mutex")
            .insert(
                request_id.clone(),
                PendingInteraction::new(
                    &request,
                    payload
                        .options
                        .iter()
                        .map(|option| option.id.clone())
                        .collect(),
                    tx,
                ),
            );
        self.session.emit(AgentEventKind::SessionStatus {
            status: SessionStatus::WaitingApproval,
        });
        self.session.emit(AgentEventKind::ApprovalRequested {
            run_id: self.run_id.clone(),
            request_id: request_id.clone(),
            request: payload,
        });
        let response = loop {
            if cancellation.is_cancelled() {
                self.pending
                    .lock()
                    .expect("pending approvals mutex")
                    .remove(&request_id);
                return Err(AgentRunError::Cancelled);
            }
            match rx.recv_timeout(INTERACTION_POLL) {
                Ok((decision, response)) => {
                    // The resolved fact and the resumption are emitted here,
                    // inside the run's causal order, rather than by the
                    // answering thread. `answered_by` is a placeholder the
                    // runtime pump rewrites with the answering consumer id;
                    // the adapter never invents a consumer identity.
                    self.session.emit(AgentEventKind::ApprovalResolved {
                        request_id: request_id.clone(),
                        decision,
                        answered_by: "adapter".to_owned(),
                    });
                    self.session.emit(AgentEventKind::SessionStatus {
                        status: SessionStatus::Working,
                    });
                    break Ok(response);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break Err(AgentRunError::Cancelled),
            }
        };
        self.pending
            .lock()
            .expect("pending approvals mutex")
            .remove(&request_id);
        response
    }
}
