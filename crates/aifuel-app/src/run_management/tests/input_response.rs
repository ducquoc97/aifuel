use super::*;

#[test]
fn ordinary_input_response_accepts_answers_for_each_question() {
    let pending = PendingRunInput {
        input_id: "input-1".to_owned(),
        run_id: "run-1".to_owned(),
        kind: RunInputKind::Ordinary,
        interaction_kind: AgentInteractionKind::OrdinaryInput,
        description: "Choose target and branch".to_owned(),
        native_method: Some("item/tool/requestUserInput".to_owned()),
        questions: Vec::new(),
        question_ids: vec!["target".to_owned(), "branch".to_owned()],
        parameters: None,
        requires_expanded_access: false,
    };
    let response = ordinary_input_response(
        &pending,
        serde_json::json!({"target":"workspace","branch":["main","release"]}),
    )
    .expect("multi-question answers should be accepted");
    let AgentInteractionResponse::Answers(answers) = response else {
        panic!("ordinary input should produce question answers");
    };
    assert_eq!(answers["target"], vec!["workspace"]);
    assert_eq!(answers["branch"], vec!["main", "release"]);
}

#[test]
fn ordinary_input_response_rejects_scalar_for_multiple_questions() {
    let pending = PendingRunInput {
        input_id: "input-1".to_owned(),
        run_id: "run-1".to_owned(),
        kind: RunInputKind::Ordinary,
        interaction_kind: AgentInteractionKind::OrdinaryInput,
        description: "Choose two values".to_owned(),
        native_method: Some("item/tool/requestUserInput".to_owned()),
        questions: Vec::new(),
        question_ids: vec!["first".to_owned(), "second".to_owned()],
        parameters: None,
        requires_expanded_access: false,
    };
    let error = ordinary_input_response(&pending, serde_json::json!("ambiguous"))
        .expect_err("one string cannot answer two distinct questions");
    assert!(error.message.contains("multiple questions"));
}

#[test]
fn elicitation_response_preserves_typed_json_fields() {
    let pending = PendingRunInput {
        input_id: "input-1".to_owned(),
        run_id: "run-1".to_owned(),
        kind: RunInputKind::Ordinary,
        interaction_kind: AgentInteractionKind::McpElicitation,
        description: "Complete this form".to_owned(),
        native_method: Some("mcpServer/elicitation/request".to_owned()),
        questions: Vec::new(),
        question_ids: Vec::new(),
        parameters: Some(serde_json::json!({"requestedSchema": {}})),
        requires_expanded_access: false,
    };
    let response =
        ordinary_input_response(&pending, serde_json::json!({"approved":true,"count":3}))
            .expect("typed elicitation should be accepted");
    let AgentInteractionResponse::Elicitation(content) = response else {
        panic!("elicitation should preserve structured JSON content");
    };
    assert_eq!(content["approved"], true);
    assert_eq!(content["count"], 3);
}
