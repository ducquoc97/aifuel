//! Approval Request policy shared by the local provider adapters.
//!
//! Every local adapter enforces the same permission policy no matter
//! which provider protocol carried the ask: `accept` is never offered
//! where it would widen a read-only run or grant access beyond the run
//! policy, a permission-profile request offers only `decline` because a
//! profile cannot be built from a boolean answer, and a host decision is
//! valid only against the exact options the request offered. The rule
//! lives here once; adapters keep their protocol-specific request
//! parsing and answer transport.

use aifuel_core::{
    AccessMode, AgentInteractionKind, AgentInteractionRequest, AgentInteractionResponse,
    AgentRuntimeError, ApprovalDecision, ApprovalKind, ApprovalOption, ApprovalRequest,
    PermissionApprovalDecision, ReceiptCode,
};
use serde_json::json;
use std::collections::BTreeMap;

/// The options a permission approval offers: `accept` only where the
/// run's declared access already permits the action - never on a
/// read-only run and never when the request needs access beyond the run
/// policy. `decline` and `cancel` are always offered.
///
/// `requires_expanded_access` is the request's own widening flag;
/// adapters whose wire shape carries no such flag pass `false`.
pub(crate) fn permission_options(
    access: AccessMode,
    requires_expanded_access: bool,
) -> Vec<ApprovalOption> {
    let mut options = Vec::new();
    if access != AccessMode::ReadOnly && !requires_expanded_access {
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
    options
}

/// A permission-profile request offers only `decline`: the profile
/// response cannot be built from a boolean answer.
pub(crate) fn decline_only_options() -> Vec<ApprovalOption> {
    vec![ApprovalOption {
        id: "decline".to_owned(),
        label: "Decline".to_owned(),
    }]
}

/// The `approval.requested` event payload for one provider interaction,
/// following the permission policy above.
pub(crate) fn approval_request(
    request: &AgentInteractionRequest,
    access: AccessMode,
) -> ApprovalRequest {
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
            ApprovalRequest {
                kind: ApprovalKind::ToolPermission,
                title: request.description.clone(),
                detail,
                options: permission_options(access, request.requires_expanded_access),
                requires_confirm: false,
            }
        }
        AgentInteractionKind::PermissionProfileApproval => ApprovalRequest {
            kind: ApprovalKind::ToolPermission,
            title: request.description.clone(),
            detail,
            options: decline_only_options(),
            requires_confirm: false,
        },
    }
}

/// Validate a host's option decision against the options a request
/// offered and map it to the contract verdict. Free text is never a
/// permission answer, and an option that was never offered is rejected
/// rather than guessed.
pub(crate) fn permission_verdict(
    options: &[String],
    decision: &ApprovalDecision,
) -> Result<PermissionApprovalDecision, AgentRuntimeError> {
    let ApprovalDecision::OptionId(id) = decision else {
        return Err(invalid_state(
            "permission approvals take a declared option, not free text",
        ));
    };
    if !options.iter().any(|option| option == id) {
        return Err(invalid_state(format!(
            "{id:?} was not offered on this request"
        )));
    }
    Ok(match id.as_str() {
        "accept" => PermissionApprovalDecision::Accept,
        "decline" => PermissionApprovalDecision::Decline,
        "cancel" => PermissionApprovalDecision::Cancel,
        _ => unreachable!("options only contain accept, decline, or cancel"),
    })
}

/// Map the host's decision onto the provider-native response shape for
/// one parked interaction. Options are enforced exactly as offered;
/// free text fills a declared question only when the request has a
/// single answer slot.
pub(crate) fn decision_response(
    kind: AgentInteractionKind,
    options: &[String],
    question_ids: &[String],
    decision: &ApprovalDecision,
) -> Result<AgentInteractionResponse, AgentRuntimeError> {
    match kind {
        AgentInteractionKind::CommandApproval | AgentInteractionKind::FileChangeApproval => Ok(
            AgentInteractionResponse::Permission(permission_verdict(options, decision)?),
        ),
        AgentInteractionKind::PermissionProfileApproval => {
            let ApprovalDecision::OptionId(id) = decision else {
                return Err(invalid_state(
                    "permission profile approvals take a declared option",
                ));
            };
            if id != "decline" || !options.iter().any(|option| option == id) {
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
            let question = match question_ids {
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

fn invalid_state(message: impl Into<String>) -> AgentRuntimeError {
    AgentRuntimeError::new(ReceiptCode::InvalidState, message)
}

fn unsupported(message: impl Into<String>) -> AgentRuntimeError {
    AgentRuntimeError::unsupported(message)
}
