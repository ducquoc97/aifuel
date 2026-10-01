//! A scripted [`AgentExecutionAdapter`]: the seam under the real
//! [`CliAdapter`], so tests drive the production session worker, output
//! handler, and interaction plumbing instead of only the fake
//! `AgentAdapter`.

use super::{FAKE_INTEGRATION, FAKE_PROVIDER};
use aifuel_core::{
    AgentCapability, AgentCapabilityEvidence, AgentExecutionAdapter, AgentInteractionKind,
    AgentInteractionRequest, AgentRunError, AgentRunOutputHandler, CapabilityState, ExecutionMode,
    IntegrationId, ProviderId, RunCancellationToken, RunRequest, RunResult, RunStatus,
};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

/// An [`AgentExecutionAdapter`] scripted per `execute` call, so tests can
/// drive the real [`CliAdapter`](aifuel_providers::CliAdapter) worker,
/// output handler, and interaction plumbing through the facade.
pub struct ScriptedExecution {
    scripts: Mutex<VecDeque<ExecScript>>,
}

/// The canned behavior of one `execute` call on [`ScriptedExecution`].
pub enum ExecScript {
    /// Report each delta through the run's real output handler, then
    /// succeed. `session_id` lands as the session's provider resume cursor
    /// the way a provider-reported native session id would.
    Succeed {
        deltas: Vec<String>,
        session_id: Option<&'static str>,
    },
    /// Raise one permission request through the run's real interaction
    /// handler, block on the host's answer, then succeed.
    Approve { description: &'static str },
}

impl ScriptedExecution {
    /// A scripted executor serving `fake-cli`.
    pub fn new(scripts: Vec<ExecScript>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into()),
        }
    }

    fn run(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
        output: Option<&dyn AgentRunOutputHandler>,
    ) -> Result<RunResult, AgentRunError> {
        let script = self
            .scripts
            .lock()
            .expect("scripts mutex")
            .pop_front()
            .unwrap_or(ExecScript::Succeed {
                deltas: Vec::new(),
                session_id: None,
            });
        let mut session_id = None;
        let mut text = String::new();
        match script {
            ExecScript::Succeed {
                deltas,
                session_id: reported,
            } => {
                session_id = reported.map(str::to_owned);
                for delta in deltas {
                    if let Some(output) = output {
                        output.on_output(&delta);
                    } else {
                        text.push_str(&delta);
                    }
                }
            }
            ExecScript::Approve { description } => {
                let handler = request
                    .interaction_handler
                    .as_ref()
                    .expect("an approvals-capable adapter wires the handler");
                handler.interact(
                    AgentInteractionRequest {
                        request_id: serde_json::Value::Null,
                        method: "session/request_permission".to_owned(),
                        kind: AgentInteractionKind::CommandApproval,
                        description: description.to_owned(),
                        questions: Vec::new(),
                        parameters: serde_json::Value::Null,
                        requires_expanded_access: false,
                    },
                    cancellation,
                )?;
            }
        }
        Ok(RunResult {
            run_id: format!("native-run-{}", std::process::id()),
            local_session_id: format!("local-{}", std::process::id()),
            session_id,
            resumed_from: request.resume.clone(),
            provider_id: ProviderId::new(FAKE_PROVIDER),
            integration_id: IntegrationId::new(FAKE_INTEGRATION),
            requested_model: request.model.clone(),
            requested_effort: request.effort.clone(),
            effective_model: request.model.clone(),
            effective_effort: request.effort.clone(),
            requested_account_id: None,
            account_id: None,
            execution_mode: ExecutionMode::Project,
            permission_profile: request.access,
            status: RunStatus::Succeeded,
            exit_code: Some(0),
            output: text,
            error: None,
            diagnostics: None,
            usage: None,
            timed_out: false,
            working_directory: request.working_directory.clone().unwrap_or_default(),
        })
    }
}

impl AgentExecutionAdapter for ScriptedExecution {
    fn integration(&self) -> IntegrationId {
        IntegrationId::new(FAKE_INTEGRATION)
    }

    fn provider(&self) -> ProviderId {
        ProviderId::new(FAKE_PROVIDER)
    }

    fn declared_agent_capabilities(&self) -> BTreeMap<AgentCapability, AgentCapabilityEvidence> {
        AgentCapability::ALL
            .into_iter()
            .map(|capability| {
                let supported = matches!(
                    capability,
                    AgentCapability::Streaming
                        | AgentCapability::Resume
                        | AgentCapability::PermissionApproval
                        | AgentCapability::WorkspaceWrite
                        | AgentCapability::ReadOnly
                );
                (
                    capability,
                    AgentCapabilityEvidence {
                        state: if supported {
                            CapabilityState::Supported
                        } else {
                            CapabilityState::Unknown
                        },
                        reason: "scripted execution adapter".to_owned(),
                    },
                )
            })
            .collect()
    }

    fn execute(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
    ) -> Result<RunResult, AgentRunError> {
        self.run(request, cancellation, None)
    }

    fn execute_with_output_handler(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
        output_handler: &dyn AgentRunOutputHandler,
    ) -> Result<RunResult, AgentRunError> {
        self.run(request, cancellation, Some(output_handler))
    }
}
