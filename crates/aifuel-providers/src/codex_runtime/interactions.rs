//! Approval payload and decision mapping for app-server requests.
//!
//! This mirrors [`cli_adapter`](crate::cli_adapter)'s interaction
//! policy - the same `accept`-never-widens-access rule, the same
//! permission-profile decline, the same free-text question handling -
//! duplicated here because that module's internals are private to it.
//! The provider-native request parsing and response framing stays in
//! [`crate::codex::interaction`]; only the contract-side shapes live
//! here.

use super::session::{PendingApproval, invalid_state, unsupported};
use aifuel_core::{
    AccessMode, AgentInteractionKind, AgentInteractionRequest, AgentInteractionResponse,
    AgentRuntimeError, ApprovalDecision, ApprovalKind, ApprovalOption, ApprovalRequest,
    AttachmentKind, PermissionApprovalDecision, UserInput,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The `turn/start` input items for one user message. Text maps to the
/// provider's `text` input and image attachments to `localImage`; file
/// attachments have no protocol carrier and fail `unsupported` rather
/// than silently dropping.
pub(super) fn input_items(input: &UserInput) -> Result<Vec<Value>, AgentRuntimeError> {
    let mut items = Vec::new();
    if !input.text.trim().is_empty() {
        items.push(json!({"type": "text", "text": input.text}));
    }
    for attachment in &input.attachments {
        match attachment.kind {
            AttachmentKind::Image => {
                items.push(json!({"type": "localImage", "path": attachment.path}))
            }
            AttachmentKind::File => {
                return Err(unsupported(
                    "file attachments have no app-server input carrier",
                ));
            }
        }
    }
    if items.is_empty() {
        return Err(invalid_state("input text must not be empty"));
    }
    Ok(items)
}

/// The event payload shape for one provider interaction.
///
/// Mirrors the run manager's permission policy: `accept` is never
/// offered where it would widen a read-only run or grant access beyond
/// the run policy, and a permission-profile request only offers
/// `decline` because the profile response cannot be built from a
/// boolean answer.
pub(super) fn approval_request(
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
            let mut options = Vec::new();
            if access != AccessMode::ReadOnly && !request.requires_expanded_access {
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

/// Map the host's decision onto the provider-native response shape.
/// Options are enforced exactly as offered; free text fills a declared
/// question only when the request has a single answer slot.
pub(super) fn decision_response(
    pending: &PendingApproval,
    decision: &ApprovalDecision,
) -> Result<AgentInteractionResponse, AgentRuntimeError> {
    match pending.interaction.kind {
        AgentInteractionKind::CommandApproval | AgentInteractionKind::FileChangeApproval => {
            let ApprovalDecision::OptionId(id) = decision else {
                return Err(invalid_state(
                    "permission approvals take a declared option, not free text",
                ));
            };
            if !pending.options.iter().any(|option| option == id) {
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
            if id != "decline" || !pending.options.iter().any(|option| option == id) {
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
            let question = match pending.question_ids.as_slice() {
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
