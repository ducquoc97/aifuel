use super::helpers::{event_size, now, ordinary_input_response};
use super::*;
use aifuel_core::{
    AccessMode, ExecutionMode, IntegrationId, OutputFormat, ProviderId, ProviderKey,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

struct ProbeAdapter {
    provider: ProviderKey,
    wait: Arc<AtomicBool>,
}

struct StreamingAdapter {
    wait: Arc<AtomicBool>,
}

struct OversizedOutputAdapter {
    bytes: usize,
}

struct InputAdapter {
    method: &'static str,
}

/// Fails the run immediately with an `InvalidRequest` provider error.
struct FailingAdapter;

impl AgentExecutionAdapter for ProbeAdapter {
    fn integration(&self) -> IntegrationId {
        self.provider.into()
    }

    fn provider(&self) -> ProviderId {
        self.provider.into()
    }

    fn execute(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
    ) -> Result<RunResult, AgentRunError> {
        while self.wait.load(Ordering::Acquire) {
            if cancellation.is_cancelled() {
                return Err(AgentRunError::Cancelled);
            }
            thread::sleep(Duration::from_millis(1));
        }
        Ok(RunResult {
            run_id: "provider-run".to_owned(),
            local_session_id: "provider-session".to_owned(),
            session_id: Some("native-session".to_owned()),
            resumed_from: request.resume.clone(),
            provider_id: self.provider.into(),
            integration_id: request.integration.clone(),
            requested_model: request.model.clone(),
            requested_effort: request.effort.clone(),
            effective_model: Some("effective-model".to_owned()),
            effective_effort: None,
            requested_account_id: request.account.clone(),
            account_id: None,
            execution_mode: ExecutionMode::PromptOnly,
            permission_profile: request.access,
            status: RunStatus::Succeeded,
            exit_code: Some(0),
            output: request.prompt.clone(),
            error: None,
            diagnostics: None,
            timed_out: false,
            usage: Some(aifuel_core::TokenUsage {
                input_tokens: Some(7),
                output_tokens: Some(3),
            }),
            working_directory: std::env::temp_dir(),
        })
    }
}

impl AgentExecutionAdapter for StreamingAdapter {
    fn integration(&self) -> IntegrationId {
        ProviderKey::Claude.into()
    }

    fn provider(&self) -> ProviderId {
        ProviderKey::Claude.into()
    }

    fn execute(
        &self,
        _request: &RunRequest,
        _cancellation: &RunCancellationToken,
    ) -> Result<RunResult, AgentRunError> {
        Err(AgentRunError::InvalidRequest(
            "test adapter requires the output-handler path".to_owned(),
        ))
    }

    fn execute_with_output_handler(
        &self,
        _request: &RunRequest,
        cancellation: &RunCancellationToken,
        output_handler: &dyn AgentRunOutputHandler,
    ) -> Result<RunResult, AgentRunError> {
        output_handler.on_output("first ");
        output_handler.on_output("second");
        while self.wait.load(Ordering::Acquire) {
            if cancellation.is_cancelled() {
                return Err(AgentRunError::Cancelled);
            }
            thread::sleep(Duration::from_millis(1));
        }
        Err(AgentRunError::Cancelled)
    }
}

impl AgentExecutionAdapter for OversizedOutputAdapter {
    fn integration(&self) -> IntegrationId {
        ProviderKey::Claude.into()
    }

    fn provider(&self) -> ProviderId {
        ProviderKey::Claude.into()
    }

    fn execute(
        &self,
        _request: &RunRequest,
        _cancellation: &RunCancellationToken,
    ) -> Result<RunResult, AgentRunError> {
        Err(AgentRunError::InvalidRequest(
            "test adapter requires the output-handler path".to_owned(),
        ))
    }

    fn execute_with_output_handler(
        &self,
        _request: &RunRequest,
        _cancellation: &RunCancellationToken,
        output_handler: &dyn AgentRunOutputHandler,
    ) -> Result<RunResult, AgentRunError> {
        output_handler.on_output(&"x".repeat(self.bytes));
        Err(AgentRunError::Timeout("test deadline".to_owned()))
    }
}

impl AgentExecutionAdapter for FailingAdapter {
    fn integration(&self) -> IntegrationId {
        ProviderKey::Claude.into()
    }

    fn provider(&self) -> ProviderId {
        ProviderKey::Claude.into()
    }

    fn execute(
        &self,
        _request: &RunRequest,
        _cancellation: &RunCancellationToken,
    ) -> Result<RunResult, AgentRunError> {
        Err(AgentRunError::InvalidRequest("probe rejection".to_owned()))
    }
}

impl AgentExecutionAdapter for InputAdapter {
    fn integration(&self) -> IntegrationId {
        ProviderKey::Claude.into()
    }

    fn provider(&self) -> ProviderId {
        ProviderKey::Claude.into()
    }

    fn execute(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
    ) -> Result<RunResult, AgentRunError> {
        let handler = request
            .interaction_handler
            .as_ref()
            .ok_or_else(|| AgentRunError::InvalidRequest("no input handler".to_owned()))?;
        let response = handler.interact(
            AgentInteractionRequest {
                request_id: serde_json::json!("input-request"),
                method: self.method.to_owned(),
                kind: AgentInteractionKind::OrdinaryInput,
                description: "Choose a target".to_owned(),
                questions: vec![aifuel_core::AgentInputQuestion {
                    id: "target".to_owned(),
                    text: "Which target?".to_owned(),
                }],
                parameters: serde_json::json!({"diagnostic":"retained separately"}),
                requires_expanded_access: false,
            },
            cancellation,
        )?;
        let AgentInteractionResponse::Answers(answers) = response else {
            return Err(AgentRunError::InvalidRequest(
                "received an invalid ordinary input response".to_owned(),
            ));
        };
        if !answers
            .get("target")
            .is_some_and(|values| values.len() == 1 && values[0] == "workspace")
        {
            return Err(AgentRunError::InvalidRequest(
                "received an unexpected ordinary input response".to_owned(),
            ));
        }
        Ok(RunResult {
            run_id: "input-provider-run".to_owned(),
            local_session_id: "input-provider-session".to_owned(),
            session_id: None,
            resumed_from: None,
            provider_id: self.provider(),
            integration_id: self.integration(),
            requested_model: request.model.clone(),
            requested_effort: request.effort.clone(),
            effective_model: None,
            effective_effort: None,
            requested_account_id: request.account.clone(),
            account_id: None,
            execution_mode: ExecutionMode::PromptOnly,
            permission_profile: request.access,
            status: RunStatus::Succeeded,
            exit_code: Some(0),
            output: "input received".to_owned(),
            error: None,
            diagnostics: None,
            timed_out: false,
            usage: None,
            working_directory: request
                .working_directory
                .clone()
                .unwrap_or_else(std::env::temp_dir),
        })
    }
}

fn request() -> RunRequest {
    RunRequest {
        integration: ProviderKey::Claude.into(),
        model: Some("model-a".to_owned()),
        effort: None,
        external_tools: None,
        account: None,
        prompt: "hello".to_owned(),
        output: OutputFormat::Text,
        working_directory: None,
        access: aifuel_core::AccessMode::ReadOnly,
        resume: None,
        timeout: None,
        interaction_handler: None,
    }
}

fn wait_for_terminal(manager: &RunManager, id: &str) -> ManagedRun {
    // 500 x 2ms = 1s budget: oversized-output tests push MiB through the
    // bounded retention path in debug builds, which a 200ms budget misses
    // under even light load.
    for _ in 0..500 {
        let run = manager.get_run(id).expect("run remains owner-local");
        if run.state.is_terminal() {
            return run;
        }
        thread::sleep(Duration::from_millis(2));
    }
    panic!("run did not reach a terminal state")
}

mod approvals;

mod input_response;

mod lifecycle;

mod runs;

mod store;
