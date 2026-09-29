//! Provider interaction plumbing for the CLI fallback session.
//!
//! When an execution adapter raises an [`AgentInteractionRequest`], the
//! handler here turns it into an `approval.requested` event on the run,
//! parks the provider call, and wakes it with the host's
//! `approval.answer`. The event payload mirrors the run manager's
//! permission policy: `accept` is never offered where it would widen a
//! read-only run or grant access beyond the run policy, and a
//! permission-profile request only offers `decline` because the profile
//! response cannot be built from a boolean answer.

use super::session::{CliSession, invalid_state, unsupported};
use aifuel_core::{
    AccessMode, AgentEventKind, AgentInteractionHandler, AgentInteractionKind,
    AgentInteractionRequest, AgentInteractionResponse, AgentRunError, AgentRuntimeError,
    ApprovalDecision, ApprovalKind, ApprovalOption, ApprovalRequest, PermissionApprovalDecision,
    RunCancellationToken, RunId, SessionStatus,
};
use serde_json::json;
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
            question_ids: request
                .questions
                .iter()
                .map(|question| question.id.clone())
                .collect(),
            response,
        }
    }

    /// Map the host's decision onto the provider-native response shape.
    /// Options are enforced exactly as offered; free text fills a declared
    /// question only when the request has a single answer slot.
    pub(super) fn response_for(
        &self,
        decision: &ApprovalDecision,
    ) -> Result<AgentInteractionResponse, AgentRuntimeError> {
        match self.kind {
            AgentInteractionKind::CommandApproval | AgentInteractionKind::FileChangeApproval => {
                let ApprovalDecision::OptionId(id) = decision else {
                    return Err(invalid_state(
                        "permission approvals take a declared option, not free text",
                    ));
                };
                if !self.options.iter().any(|option| option == id) {
                    return Err(invalid_state(format!(
                        "{id:?} was not offered on this request"
                    )));
                }
                Ok(AgentInteractionResponse::Permission(match id.as_str() {
                    "accept" => PermissionApprovalDecision::Accept,
                    "decline" => PermissionApprovalDecision::Decline,
                    "cancel" => PermissionApprovalDecision::Cancel,
                    _ => unreachable!("options only contain accept, decline, or cancel"),
                }))
            }
            AgentInteractionKind::PermissionProfileApproval => {
                let ApprovalDecision::OptionId(id) = decision else {
                    return Err(invalid_state(
                        "permission profile approvals take a declared option",
                    ));
                };
                if id != "decline" || !self.options.iter().any(|option| option == id) {
                    return Err(invalid_state(format!(
                        "{id:?} was not offered on this request"
                    )));
                }
                // Matches the run manager: decline carries an empty profile
                // scoped to this turn.
                Ok(AgentInteractionResponse::PermissionProfile {
                    permissions: json!({}),
                    scope: "turn".to_owned(),
                })
            }
            AgentInteractionKind::OrdinaryInput | AgentInteractionKind::McpElicitation => {
                let ApprovalDecision::Text(text) = decision else {
                    return Err(invalid_state(
                        "input requests take free text, not a declared option",
                    ));
                };
                let question = match self.question_ids.as_slice() {
                    [] => "answer".to_owned(),
                    [only] => only.clone(),
                    _ => {
                        return Err(unsupported(
                            "a free-text decision cannot answer a multi-question request",
                        ));
                    }
                };
                Ok(AgentInteractionResponse::Answers(BTreeMap::from([(
                    question,
                    vec![text.clone()],
                )])))
            }
        }
    }
}

/// Routes provider interaction requests through the session event stream
/// and blocks on the host's `approval.answer`.
pub(super) struct CliInteractionHandler {
    session: Arc<CliSession>,
    run_id: RunId,
    pending: Arc<Mutex<BTreeMap<String, PendingInteraction>>>,
}

impl CliInteractionHandler {
    pub(super) fn new(
        session: Arc<CliSession>,
        run_id: RunId,
        pending: Arc<Mutex<BTreeMap<String, PendingInteraction>>>,
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
        let payload = approval_request(&request, self.session.access);
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .expect("pending approvals mutex")
            .insert(
                request_id.as_str().to_owned(),
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
                    .remove(request_id.as_str());
                return Err(AgentRunError::Cancelled);
            }
            match rx.recv_timeout(INTERACTION_POLL) {
                Ok((decision, response)) => {
                    // The resolved fact and the resumption are emitted here,
                    // inside the run's causal order, rather than by the
                    // answering thread.
                    self.session.emit(AgentEventKind::ApprovalResolved {
                        request_id: request_id.clone(),
                        decision,
                        answered_by: "cli".to_owned(),
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
            .remove(request_id.as_str());
        response
    }
}

/// The event payload shape for one provider interaction.
fn approval_request(request: &AgentInteractionRequest, access: AccessMode) -> ApprovalRequest {
    let detail = if request.questions.is_empty() {
        request.description.clone()
    } else {
        let questions = request
            .questions
            .iter()
            .map(|question| format!("- {}", question.text))
            .collect::<Vec<_>>()
            .join("\n");
        format!("{}\n{questions}", request.description)
    };
    match request.kind {
        AgentInteractionKind::OrdinaryInput => ApprovalRequest {
            kind: ApprovalKind::Question,
            title: request.description.clone(),
            detail,
            options: Vec::new(),
            requires_confirm: false,
        },
        AgentInteractionKind::McpElicitation => ApprovalRequest {
            kind: ApprovalKind::McpElicitation,
            title: request.description.clone(),
            detail,
            options: Vec::new(),
            requires_confirm: false,
        },
        AgentInteractionKind::CommandApproval | AgentInteractionKind::FileChangeApproval => {
            let mut options = Vec::new();
            if access == AccessMode::WorkspaceWrite && !request.requires_expanded_access {
                options.push(ApprovalOption {
                    id: "accept".to_owned(),
                    label: "Accept".to_owned(),
                });
            }
            options.push(ApprovalOption {
                id: "decline".to_owned(),
                label: "Decline".to_owned(),
            });
            options.push(ApprovalOption {
                id: "cancel".to_owned(),
                label: "Cancel run".to_owned(),
            });
            ApprovalRequest {
                kind: ApprovalKind::ToolPermission,
                title: request.description.clone(),
                detail,
                options,
                requires_confirm: false,
            }
        }
        AgentInteractionKind::PermissionProfileApproval => ApprovalRequest {
            kind: ApprovalKind::ToolPermission,
            title: request.description.clone(),
            detail,
            options: vec![ApprovalOption {
                id: "decline".to_owned(),
                label: "Decline".to_owned(),
            }],
            requires_confirm: false,
        },
    }
}
