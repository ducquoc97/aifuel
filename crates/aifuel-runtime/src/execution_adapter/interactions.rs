//! Approval mapping between the runtime's `approval.requested` contract and
//! the legacy [`AgentInteractionRequest`]/[`AgentInteractionResponse`] pair
//! the owner's interaction handler speaks.

use aifuel_core::{
    AgentInteractionKind, AgentInteractionRequest, AgentInteractionResponse, ApprovalDecision,
    ApprovalKind, ApprovalRequest, PermissionApprovalDecision, RequestId,
};

/// Rebuild the legacy [`AgentInteractionRequest`] behind one runtime
/// `approval.requested`. `interaction_kind` is the provider-native kind
/// adapters already carry through; `requires_expanded_access` is inferred
/// from the offered options because the shared approval policy drops
/// `accept` exactly when the run may not grant it.
pub(super) fn interaction_request(
    request_id: &RequestId,
    request: &ApprovalRequest,
) -> AgentInteractionRequest {
    let kind = request.interaction_kind.unwrap_or(match request.kind {
        ApprovalKind::Question => AgentInteractionKind::OrdinaryInput,
        ApprovalKind::McpElicitation => AgentInteractionKind::McpElicitation,
        ApprovalKind::ToolPermission | ApprovalKind::PlanApproval => {
            AgentInteractionKind::CommandApproval
        }
    });
    let requires_expanded_access = matches!(
        kind,
        AgentInteractionKind::CommandApproval | AgentInteractionKind::FileChangeApproval
    ) && !request.options.iter().any(|option| option.id == "accept");
    AgentInteractionRequest {
        request_id: serde_json::Value::String(request_id.as_str().to_owned()),
        method: request.native_method.clone().unwrap_or_default(),
        kind,
        description: request.title.clone(),
        questions: request.questions.clone(),
        parameters: request
            .parameters
            .clone()
            .unwrap_or(serde_json::Value::Null),
        requires_expanded_access,
    }
}

/// Map the owner handler's legacy response back onto the contract decision.
/// Option ids mirror the shared approval policy `local_adapter` applies:
/// `accept`, `decline`, and `cancel` are the only spellings a permission
/// request can offer, and a permission-profile response answers its sole
/// `decline` option.
pub(super) fn approval_decision(response: &AgentInteractionResponse) -> ApprovalDecision {
    match response {
        AgentInteractionResponse::Answers(answers) => ApprovalDecision::Answers(answers.clone()),
        AgentInteractionResponse::Elicitation(content) => {
            ApprovalDecision::Elicitation(content.clone())
        }
        AgentInteractionResponse::Permission(decision) => ApprovalDecision::OptionId(
            match decision {
                PermissionApprovalDecision::Accept => "accept",
                PermissionApprovalDecision::Decline => "decline",
                PermissionApprovalDecision::Cancel => "cancel",
            }
            .to_owned(),
        ),
        AgentInteractionResponse::PermissionProfile { .. } => {
            ApprovalDecision::OptionId("decline".to_owned())
        }
    }
}
