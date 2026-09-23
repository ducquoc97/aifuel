//! Codex App Server interaction messages.
//!
//! This module is deliberately transport-agnostic. The existing CLI runner
//! still uses `codex exec`; an App Server adapter can use these values to route
//! server-initiated approval and elicitation requests through the application
//! input/approval boundary without copying prompt content into persistent
//! state.

use aifuel_core::{
    AgentInputQuestion, AgentInteractionKind, AgentInteractionRequest, AgentInteractionResponse,
    PermissionApprovalDecision,
};
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PendingInteraction {
    pub request_id: Value,
    pub kind: AgentInteractionKind,
    pub description: String,
    pub method: String,
    pub questions: Vec<AgentInputQuestion>,
    pub parameters: Value,
    pub requires_expanded_access: bool,
}

/// Parse a server-initiated App Server request into the normalized pending
/// interaction seam. Unknown methods remain invisible until a provider-local
/// adapter explicitly supports them.
pub(crate) fn parse_pending(message: &Value) -> Option<PendingInteraction> {
    let method = message.get("method")?.as_str()?;
    let request_id = message.get("id")?.clone();
    let params = message.get("params").cloned().unwrap_or_default();
    let (kind, description, requires_expanded_access) = match method {
        "item/commandExecution/requestApproval" => (
            AgentInteractionKind::CommandApproval,
            params
                .get("reason")
                .or_else(|| params.get("command"))
                .or_else(|| params.get("description"))
                .and_then(Value::as_str)
                .unwrap_or("provider requested permission")
                .to_owned(),
            has_nonempty_additional_permissions(&params),
        ),
        "item/fileChange/requestApproval" => (
            AgentInteractionKind::FileChangeApproval,
            params
                .get("reason")
                .or_else(|| params.get("description"))
                .and_then(Value::as_str)
                .unwrap_or("provider requested permission")
                .to_owned(),
            requests_additional_file_root(&params),
        ),
        "item/permissions/requestApproval" => (
            AgentInteractionKind::PermissionProfileApproval,
            params
                .get("reason")
                .or_else(|| params.get("description"))
                .and_then(Value::as_str)
                .unwrap_or("provider requested permission")
                .to_owned(),
            has_nonempty_additional_permissions(&params) || requests_additional_file_root(&params),
        ),
        "mcpServer/elicitation/request" => (
            AgentInteractionKind::McpElicitation,
            params
                .get("message")
                .or_else(|| params.get("question"))
                .or_else(|| params.get("description"))
                .or_else(|| {
                    params
                        .get("questions")
                        .and_then(Value::as_array)
                        .and_then(|questions| questions.first())
                        .and_then(|question| question.get("question"))
                })
                .and_then(Value::as_str)
                .unwrap_or("provider requested input")
                .to_owned(),
            false,
        ),
        "item/tool/requestUserInput" => (
            AgentInteractionKind::OrdinaryInput,
            params
                .get("message")
                .or_else(|| params.get("question"))
                .or_else(|| params.get("description"))
                .or_else(|| {
                    params
                        .get("questions")
                        .and_then(Value::as_array)
                        .and_then(|questions| questions.first())
                        .and_then(|question| question.get("question"))
                })
                .and_then(Value::as_str)
                .unwrap_or("provider requested input")
                .to_owned(),
            false,
        ),
        _ => return None,
    };
    let questions = params
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|question| {
            let id = question.get("id")?.as_str()?.to_owned();
            let text = question
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or(&description)
                .to_owned();
            Some(AgentInputQuestion { id, text })
        })
        .collect::<Vec<_>>();
    Some(PendingInteraction {
        request_id,
        kind,
        description,
        method: method.to_owned(),
        questions,
        parameters: params,
        requires_expanded_access,
    })
}

pub(crate) fn application_request(pending: &PendingInteraction) -> AgentInteractionRequest {
    AgentInteractionRequest {
        request_id: pending.request_id.clone(),
        method: pending.method.clone(),
        kind: pending.kind,
        description: pending.description.clone(),
        questions: pending.questions.clone(),
        parameters: pending.parameters.clone(),
        requires_expanded_access: pending.requires_expanded_access,
    }
}

pub(crate) fn response_message(
    pending: &PendingInteraction,
    response: AgentInteractionResponse,
) -> Result<Value, String> {
    let result = match (pending.kind, pending.method.as_str(), response) {
        (
            AgentInteractionKind::OrdinaryInput,
            "item/tool/requestUserInput",
            AgentInteractionResponse::Answers(answers),
        ) => {
            let answers = answers
                .into_iter()
                .map(|(question, answers)| (question, json!({"answers": answers})))
                .collect::<Map<_, _>>();
            json!({"answers": answers})
        }
        (
            AgentInteractionKind::McpElicitation,
            "mcpServer/elicitation/request",
            AgentInteractionResponse::Elicitation(content),
        ) => json!({"action":"accept", "content":content}),
        (
            AgentInteractionKind::McpElicitation,
            "mcpServer/elicitation/request",
            AgentInteractionResponse::Answers(answers),
        ) => {
            let content = answers
                .into_iter()
                .filter_map(|(key, values)| {
                    values
                        .into_iter()
                        .next()
                        .map(|value| (key, Value::String(value)))
                })
                .collect::<Map<_, _>>();
            json!({"action":"accept", "content":content})
        }
        (
            AgentInteractionKind::CommandApproval,
            "item/commandExecution/requestApproval",
            AgentInteractionResponse::Permission(decision),
        ) => json!({"decision": permission_decision(decision)}),
        (
            AgentInteractionKind::FileChangeApproval,
            "item/fileChange/requestApproval",
            AgentInteractionResponse::Permission(decision),
        ) => json!({"decision": permission_decision(decision)}),
        (
            AgentInteractionKind::PermissionProfileApproval,
            "item/permissions/requestApproval",
            AgentInteractionResponse::PermissionProfile { permissions, scope },
        ) => json!({"permissions":permissions,"scope":scope}),
        _ => return Err("interaction response does not match the native request".to_owned()),
    };
    Ok(json!({"id":pending.request_id,"result":result}))
}

fn has_nonempty_additional_permissions(parameters: &Value) -> bool {
    let Some(parameters) = parameters.as_object() else {
        return true;
    };
    match parameters.get("additionalPermissions") {
        None | Some(Value::Null) => false,
        Some(Value::Object(profile)) => !profile.is_empty(),
        Some(_) => true,
    }
}

fn requests_additional_file_root(parameters: &Value) -> bool {
    let Some(parameters) = parameters.as_object() else {
        return true;
    };
    match parameters.get("grantRoot") {
        None | Some(Value::Null) => false,
        Some(Value::String(root)) => !root.trim().is_empty(),
        Some(_) => true,
    }
}

fn permission_decision(decision: PermissionApprovalDecision) -> &'static str {
    match decision {
        PermissionApprovalDecision::Accept => "accept",
        PermissionApprovalDecision::Decline => "decline",
        PermissionApprovalDecision::Cancel => "cancel",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn command_approval_is_normalized_as_permission_input() {
        let pending = parse_pending(&json!({
            "id": 7,
            "method": "item/commandExecution/requestApproval",
            "params": {"command": "touch file", "reason": "write requested"}
        }))
        .expect("approval should be recognized");

        assert_eq!(pending.request_id, json!(7));
        assert_eq!(pending.kind, AgentInteractionKind::CommandApproval);
        assert_eq!(pending.description, "write requested");
    }

    #[test]
    fn elicitation_is_normalized_as_ordinary_input() {
        let pending = parse_pending(&json!({
            "id": "q-1",
            "method": "mcpServer/elicitation/request",
            "params": {"message": "Which target?"}
        }))
        .expect("elicitation should be recognized");

        assert_eq!(pending.kind, AgentInteractionKind::McpElicitation);
        assert_eq!(pending.description, "Which target?");
    }

    #[test]
    fn app_server_user_input_normalizes_question_ids_and_text() {
        let pending = parse_pending(&json!({
            "id": "input-1",
            "method": "item/tool/requestUserInput",
            "params": {"questions": [
                {"id": "target", "question": "Choose target"},
                {"id": "branch", "question": "Choose branch"}
            ]}
        }))
        .expect("tool input should be recognized");
        assert_eq!(pending.kind, AgentInteractionKind::OrdinaryInput);
        assert_eq!(
            pending.questions,
            vec![
                AgentInputQuestion {
                    id: "target".to_owned(),
                    text: "Choose target".to_owned(),
                },
                AgentInputQuestion {
                    id: "branch".to_owned(),
                    text: "Choose branch".to_owned(),
                },
            ]
        );
        assert_eq!(pending.description, "Choose target");
        let response = response_message(
            &pending,
            AgentInteractionResponse::Answers(BTreeMap::from([
                ("target".to_owned(), vec!["workspace".to_owned()]),
                ("branch".to_owned(), vec!["main".to_owned()]),
            ])),
        )
        .expect("answer should serialize");
        assert_eq!(response["id"], "input-1");
        assert_eq!(
            response["result"]["answers"]["target"]["answers"][0],
            "workspace"
        );
        assert_eq!(
            response["result"]["answers"]["branch"]["answers"][0],
            "main"
        );
    }

    #[test]
    fn elicitation_preserves_typed_content() {
        let pending = parse_pending(&json!({
            "id": "form-1",
            "method": "mcpServer/elicitation/request",
            "params": {"message": "Confirm these values"}
        }))
        .expect("elicitation should be recognized");
        let response = response_message(
            &pending,
            AgentInteractionResponse::Elicitation(json!({
                "approved": true,
                "count": 3,
                "labels": ["one", "two"]
            })),
        )
        .expect("typed elicitation should serialize");
        assert_eq!(response["result"]["action"], "accept");
        assert_eq!(response["result"]["content"]["approved"], true);
        assert_eq!(response["result"]["content"]["count"], 3);
        assert_eq!(response["result"]["content"]["labels"][1], "two");
    }

    #[test]
    fn additional_permission_approval_is_distinct_from_command_approval() {
        let pending = parse_pending(&json!({
            "id": 8,
            "method": "item/permissions/requestApproval",
            "params": {"itemId": "item-1", "reason": "request write access"}
        }))
        .expect("permission profile request should be recognized");
        assert_eq!(
            pending.kind,
            AgentInteractionKind::PermissionProfileApproval
        );
        let mapped = application_request(&pending);
        assert_eq!(mapped.kind, AgentInteractionKind::PermissionProfileApproval);
    }

    #[test]
    fn provider_normalizes_permission_expansion_before_the_application_boundary() {
        let command = parse_pending(&json!({
            "id": 4,
            "method": "item/commandExecution/requestApproval",
            "params": {
                "command": "touch file",
                "additionalPermissions": {"fileSystem":{"write":["/etc"]}}
            }
        }))
        .expect("command approval should be recognized");
        let command_request = application_request(&command);
        assert_eq!(command_request.kind, AgentInteractionKind::CommandApproval);
        assert!(command_request.requires_expanded_access);

        let file_change = parse_pending(&json!({
            "id": 5,
            "method": "item/fileChange/requestApproval",
            "params": {"itemId":"item-1","grantRoot":"/etc"}
        }))
        .expect("file approval should be recognized");
        let file_request = application_request(&file_change);
        assert_eq!(file_request.kind, AgentInteractionKind::FileChangeApproval);
        assert!(file_request.requires_expanded_access);
    }

    #[test]
    fn permission_response_keeps_the_server_request_id() {
        let pending = parse_pending(&json!({
            "id": 3,
            "method": "item/commandExecution/requestApproval",
            "params": {"command": "touch file"}
        }))
        .unwrap();
        let response = response_message(
            &pending,
            AgentInteractionResponse::Permission(PermissionApprovalDecision::Decline),
        )
        .unwrap();
        assert_eq!(response["id"], 3);
        assert_eq!(response["result"]["decision"], "decline");
    }
}
