//! Approval payload and decision mapping for app-server requests.
//!
//! The event payload and the answer validation follow the shared
//! [approval policy](crate::local_adapter::approvals); the provider-native
//! request parsing and response framing stays in
//! [`crate::codex::interaction`]. Only the `turn/start` input mapping
//! lives here.

use super::session::{PendingApproval, invalid_state, unsupported};
use crate::local_adapter::approvals;
use aifuel_core::{
    AccessMode, AgentInteractionRequest, AgentInteractionResponse, AgentRuntimeError,
    ApprovalDecision, ApprovalRequest, AttachmentKind, UserInput,
};
use serde_json::{Value, json};

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
                return Err(unsupported("this adapter does not carry file attachments"));
            }
        }
    }
    if items.is_empty() {
        return Err(invalid_state("input text must not be empty"));
    }
    Ok(items)
}

/// The event payload shape for one provider interaction, following the
/// shared approval policy.
pub(super) fn approval_request(
    request: &AgentInteractionRequest,
    access: AccessMode,
) -> ApprovalRequest {
    approvals::approval_request(request, access)
}

/// Map the host's decision onto the provider-native response shape,
/// enforcing the shared approval policy exactly as offered.
pub(super) fn decision_response(
    pending: &PendingApproval,
    decision: &ApprovalDecision,
) -> Result<AgentInteractionResponse, AgentRuntimeError> {
    approvals::decision_response(
        pending.interaction.kind,
        &pending.options,
        &pending.question_ids,
        decision,
    )
}
