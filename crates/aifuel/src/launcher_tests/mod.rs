use super::*;
use aifuel_core::{
    AccessMode, AgentExecutionAdapter, AgentInteractionKind, AgentInteractionRequest,
    AgentInteractionResponse, AgentRunError, AgentRunOutputHandler, ExecutionMode, ProviderKey,
    RunCancellationToken, RunRequest, RunState, RunStatus,
};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

mod approval;
mod interactions;
mod streaming;

pub(super) struct InputAdapter;

impl AgentExecutionAdapter for InputAdapter {
    fn provider(&self) -> ProviderKey {
        ProviderKey::Codex
    }

    fn execute(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
    ) -> Result<RunResult, AgentRunError> {
        let handler = request
            .interaction_handler
            .as_ref()
            .expect("managed run installs an interaction handler");
        let response = handler.interact(
            AgentInteractionRequest {
                request_id: json!(4),
                method: "item/tool/requestUserInput".to_owned(),
                kind: AgentInteractionKind::OrdinaryInput,
                description: "Which target should I use?".to_owned(),
                questions: vec![aifuel_core::AgentInputQuestion {
                    id: "target".to_owned(),
                    text: "Which target should I use?".to_owned(),
                }],
                parameters: json!({"questions": [{"id": "target"}]}),
                requires_expanded_access: false,
            },
            cancellation,
        )?;
        let AgentInteractionResponse::Answers(answers) = response else {
            return Err(AgentRunError::InvalidRequest(
                "expected a user answer".to_owned(),
            ));
        };
        let answer = answers
            .get("target")
            .and_then(|values| values.first())
            .cloned()
            .unwrap_or_default();
        let mut result = run_result_with_output(request, answer);
        result.session_id = Some("native-session-id".to_owned());
        Ok(result)
    }
}

pub(super) struct StreamingAdapter;

impl AgentExecutionAdapter for StreamingAdapter {
    fn provider(&self) -> ProviderKey {
        ProviderKey::Codex
    }

    fn execute(
        &self,
        request: &RunRequest,
        _cancellation: &RunCancellationToken,
    ) -> Result<RunResult, AgentRunError> {
        Ok(streaming_result(request))
    }

    fn execute_with_output_handler(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
        output_handler: &dyn AgentRunOutputHandler,
    ) -> Result<RunResult, AgentRunError> {
        output_handler.on_output("first ");
        output_handler.on_output("answer");
        self.execute(request, cancellation)
    }
}

pub(super) fn streaming_result(request: &RunRequest) -> RunResult {
    RunResult {
        run_id: "provider-run-id".to_owned(),
        local_session_id: "provider-session-id".to_owned(),
        session_id: None,
        resumed_from: request.resume.clone(),
        provider_id: request.provider,
        requested_model: request.model.clone(),
        requested_effort: request.effort.clone(),
        effective_model: None,
        effective_effort: None,
        requested_account_id: request.account.clone(),
        account_id: None,
        execution_mode: if request.working_directory.is_some() {
            ExecutionMode::Project
        } else {
            ExecutionMode::PromptOnly
        },
        permission_profile: request.access,
        status: RunStatus::Succeeded,
        exit_code: Some(0),
        output: "first answer".to_owned(),
        error: None,
        diagnostics: None,
        timed_out: false,
        working_directory: std::env::temp_dir(),
    }
}

pub(super) fn run_result_with_output(request: &RunRequest, output: String) -> RunResult {
    let mut result = streaming_result(request);
    result.output = output;
    result
}

pub(super) fn request() -> RunRequest {
    RunRequest {
        provider: ProviderKey::Codex,
        model: Some("model-a".to_owned()),
        effort: None,
        external_tools: None,
        account: None,
        prompt: "start".to_owned(),
        output: OutputFormat::Json,
        working_directory: None,
        access: AccessMode::ReadOnly,
        resume: None,
        timeout: Some(Duration::from_secs(2)),
        interaction_handler: None,
    }
}
