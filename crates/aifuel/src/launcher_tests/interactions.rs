use super::*;

#[test]
fn managed_cli_run_answers_one_native_question_and_returns_the_managed_result() {
    let manager = aifuel_app::RunManager::new(vec![Arc::new(InputAdapter)]);
    let mut request = request();
    request.output = OutputFormat::Text;
    let mut input = io::Cursor::new(b"test workspace\n".to_vec());
    let mut output = Vec::new();
    let mut diagnostics = Vec::new();

    let result = execute_with_manager(
        &request,
        &manager,
        &mut input,
        &mut output,
        &mut diagnostics,
        true,
    )
    .expect("single-question interactive input should finish");

    assert_eq!(result.state, RunState::Succeeded);
    assert_eq!(result.status, Some(RunStatus::Succeeded));
    assert_eq!(result.output.as_deref(), Some("test workspace"));
    assert_eq!(result.session_id.as_deref(), Some("native-session-id"));
    assert_eq!(output, b"test workspace");
    assert!(
        String::from_utf8(diagnostics)
            .expect("diagnostics are UTF-8")
            .contains("Which target should I use?")
    );
}

#[test]
fn managed_cli_run_answers_each_native_question_by_id() {
    struct MultiQuestionAdapter;
    impl AgentExecutionAdapter for MultiQuestionAdapter {
        fn provider(&self) -> ProviderKey {
            ProviderKey::Codex
        }

        fn execute(
            &self,
            request: &RunRequest,
            cancellation: &RunCancellationToken,
        ) -> Result<RunResult, AgentRunError> {
            let response = request
                .interaction_handler
                .as_ref()
                .expect("managed run installs an interaction handler")
                .interact(
                    AgentInteractionRequest {
                        request_id: json!(5),
                        method: "item/tool/requestUserInput".to_owned(),
                        kind: AgentInteractionKind::OrdinaryInput,
                        description: "Answer both questions".to_owned(),
                        questions: vec![
                            aifuel_core::AgentInputQuestion {
                                id: "one".to_owned(),
                                text: "First question?".to_owned(),
                            },
                            aifuel_core::AgentInputQuestion {
                                id: "two".to_owned(),
                                text: "Second question?".to_owned(),
                            },
                        ],
                        parameters: json!({"diagnostic": "preserved separately"}),
                        requires_expanded_access: false,
                    },
                    cancellation,
                )?;
            let AgentInteractionResponse::Answers(answers) = response else {
                return Err(AgentRunError::InvalidRequest(
                    "expected question-ID answers".to_owned(),
                ));
            };
            let first = answers["one"][0].clone();
            let second = answers["two"][0].clone();
            Ok(run_result_with_output(
                request,
                format!("{first}; {second}"),
            ))
        }
    }

    let manager = aifuel_app::RunManager::new(vec![Arc::new(MultiQuestionAdapter)]);
    let mut request = request();
    request.output = OutputFormat::Text;
    let mut input = io::Cursor::new(b"first answer\nsecond answer\n".to_vec());
    let mut output = Vec::new();
    let mut diagnostics = Vec::new();

    let result = execute_with_manager(
        &request,
        &manager,
        &mut input,
        &mut output,
        &mut diagnostics,
        true,
    )
    .expect("each native question should receive its matching answer");

    assert_eq!(
        result.output.as_deref(),
        Some("first answer; second answer")
    );
    assert_eq!(output, b"first answer; second answer");
    let diagnostics = String::from_utf8(diagnostics).expect("prompts should be UTF-8");
    assert!(diagnostics.contains("First question?"));
    assert!(diagnostics.contains("Answer (one):"));
    assert!(diagnostics.contains("Second question?"));
    assert!(diagnostics.contains("Answer (two):"));
}

#[test]
fn noninteractive_multi_question_request_fails_before_reading_answers() {
    struct MultiQuestionAdapter;
    impl AgentExecutionAdapter for MultiQuestionAdapter {
        fn provider(&self) -> ProviderKey {
            ProviderKey::Codex
        }

        fn execute(
            &self,
            request: &RunRequest,
            cancellation: &RunCancellationToken,
        ) -> Result<RunResult, AgentRunError> {
            request
                .interaction_handler
                .as_ref()
                .expect("managed run installs an interaction handler")
                .interact(
                    AgentInteractionRequest {
                        request_id: json!(6),
                        method: "item/tool/requestUserInput".to_owned(),
                        kind: AgentInteractionKind::OrdinaryInput,
                        description: "Answer both questions".to_owned(),
                        questions: vec![
                            aifuel_core::AgentInputQuestion {
                                id: "one".to_owned(),
                                text: "First question?".to_owned(),
                            },
                            aifuel_core::AgentInputQuestion {
                                id: "two".to_owned(),
                                text: "Second question?".to_owned(),
                            },
                        ],
                        parameters: json!({}),
                        requires_expanded_access: false,
                    },
                    cancellation,
                )?;
            unreachable!("noninteractive requests fail before reading answers")
        }
    }

    let manager = aifuel_app::RunManager::new(vec![Arc::new(MultiQuestionAdapter)]);
    let mut input = io::Cursor::new(b"first answer\nsecond answer\n".to_vec());
    let mut output = Vec::new();
    let mut diagnostics = Vec::new();

    let error = execute_with_manager(
        &request(),
        &manager,
        &mut input,
        &mut output,
        &mut diagnostics,
        false,
    )
    .expect_err("scriptable run cannot answer native interactive questions");

    assert!(error.to_string().contains("no interactive terminal"));
    assert_eq!(input.position(), 0, "no partial answer should be read");
}

#[test]
fn managed_cli_run_preserves_typed_mcp_elicitation_json_values() {
    struct ElicitationAdapter;
    impl AgentExecutionAdapter for ElicitationAdapter {
        fn provider(&self) -> ProviderKey {
            ProviderKey::Codex
        }

        fn execute(
            &self,
            request: &RunRequest,
            cancellation: &RunCancellationToken,
        ) -> Result<RunResult, AgentRunError> {
            let response = request
                .interaction_handler
                .as_ref()
                .expect("managed run installs an interaction handler")
                .interact(
                    AgentInteractionRequest {
                        request_id: json!(7),
                        method: "mcpServer/elicitation/request".to_owned(),
                        kind: AgentInteractionKind::McpElicitation,
                        description: "Provide typed form values".to_owned(),
                        questions: Vec::new(),
                        parameters: json!({
                            "message": "Provide typed form values",
                            "requestedSchema": {"type":"object"}
                        }),
                        requires_expanded_access: false,
                    },
                    cancellation,
                )?;
            let AgentInteractionResponse::Elicitation(value) = response else {
                return Err(AgentRunError::InvalidRequest(
                    "expected a typed elicitation response".to_owned(),
                ));
            };
            Ok(run_result_with_output(request, value.to_string()))
        }
    }

    let manager = aifuel_app::RunManager::new(vec![Arc::new(ElicitationAdapter)]);
    let mut request = request();
    request.output = OutputFormat::Text;
    let mut input = io::Cursor::new(b"{\"age\":42,\"enabled\":true}\n".to_vec());
    let mut output = Vec::new();
    let mut diagnostics = Vec::new();

    let result = execute_with_manager(
        &request,
        &manager,
        &mut input,
        &mut output,
        &mut diagnostics,
        true,
    )
    .expect("typed elicitation should be accepted");

    let response: serde_json::Value = serde_json::from_str(
        result
            .output
            .as_deref()
            .expect("elicitation output is available"),
    )
    .expect("typed response should remain JSON");
    assert_eq!(response["age"], 42);
    assert_eq!(response["enabled"], true);
    assert_eq!(
        String::from_utf8(output).expect("response output should be UTF-8"),
        r#"{"age":42,"enabled":true}"#
    );
    assert!(
        String::from_utf8(diagnostics)
            .expect("elicitation prompt should be UTF-8")
            .contains("JSON object:")
    );
}

#[test]
fn noninteractive_run_cancels_instead_of_waiting_for_an_answer() {
    let manager = aifuel_app::RunManager::new(vec![Arc::new(InputAdapter)]);
    let mut input = io::Cursor::new(Vec::new());
    let mut output = Vec::new();
    let mut diagnostics = Vec::new();

    let error = execute_with_manager(
        &request(),
        &manager,
        &mut input,
        &mut output,
        &mut diagnostics,
        false,
    )
    .expect_err("scripted runs must not block for terminal input");

    assert!(error.to_string().contains("no interactive terminal"));
    assert!(input.get_ref().is_empty(), "no synthetic answer is sent");
}
