//! Tests for the CLI fallback adapter, run against a scripted execution
//! double so no real provider process or `aifuel-app` code is involved.

mod approvals;
mod evidence;
mod listings;
mod runs;

use super::*;
use aifuel_core::{
    AccessMode, AgentAuthenticationState, AgentCapabilityEvidence, AgentEventKind,
    AgentInputQuestion, AgentInteractionKind, AgentInteractionRequest, AgentInteractionResponse,
    AgentPresenceState, AgentRunError, AgentRunOutputHandler, AgentSetupGuidance, CliAdapterId,
    ExecutionConfig, Integration, RunCancellationToken, RunRequest, RunResult, RunStatus,
    SessionStatus, StartOptions, UserInput,
};
use serde_json::json;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

/// A scripted execution adapter double. It never spawns a process; runs
/// complete from a canned script so session, event, and approval plumbing
/// is tested without a real provider.
pub(super) struct FakeExecution {
    integration: IntegrationId,
    provider: ProviderId,
    declared: BTreeMap<AgentCapability, AgentCapabilityEvidence>,
    pub requests: Mutex<Vec<RunRequest>>,
    script: Mutex<VecDeque<FakeRun>>,
    authentication: AgentAuthenticationState,
}

pub(super) enum FakeRun {
    /// Finish immediately with this result status and optional answer text.
    Complete {
        status: RunStatus,
        output: &'static str,
        session_id: Option<&'static str>,
    },
    /// Emit two output deltas, then finish successfully.
    Streamed,
    /// Raise one interaction request and finish when it is answered.
    Interaction {
        kind: AgentInteractionKind,
        description: &'static str,
        questions: Vec<(&'static str, &'static str)>,
        requires_expanded_access: bool,
        expect: Box<dyn Fn(&AgentInteractionResponse) -> bool + Send>,
    },
    /// Block until the cancellation token fires, then report cancelled.
    Blocking,
}

impl FakeExecution {
    pub fn new(provider: ProviderKey) -> Self {
        Self::declaring(provider, &[])
    }

    pub fn declaring(provider: ProviderKey, capabilities: &[AgentCapability]) -> Self {
        let declared = AgentCapability::ALL
            .into_iter()
            .map(|capability| {
                let state = if capabilities.contains(&capability) {
                    CapabilityState::Supported
                } else {
                    CapabilityState::Unsupported
                };
                (
                    capability,
                    AgentCapabilityEvidence {
                        state,
                        reason: String::new(),
                    },
                )
            })
            .collect();
        Self {
            integration: IntegrationId::from(provider),
            provider: ProviderId::from(provider),
            declared,
            requests: Mutex::new(Vec::new()),
            script: Mutex::new(VecDeque::new()),
            authentication: AgentAuthenticationState::Unknown,
        }
    }

    pub fn with_authentication(mut self, state: AgentAuthenticationState) -> Self {
        self.authentication = state;
        self
    }

    pub fn with_runs(mut self, runs: Vec<FakeRun>) -> Self {
        self.script = Mutex::new(runs.into());
        self
    }

    fn result(
        &self,
        request: &RunRequest,
        status: RunStatus,
        output: &str,
        session_id: Option<&str>,
    ) -> RunResult {
        RunResult {
            run_id: "fake-run".to_owned(),
            local_session_id: "fake-local".to_owned(),
            session_id: session_id.map(str::to_owned),
            resumed_from: request.resume.clone(),
            provider_id: self.provider.clone(),
            integration_id: request.integration.clone(),
            requested_model: request.model.clone(),
            requested_effort: request.effort.clone(),
            effective_model: None,
            effective_effort: None,
            requested_account_id: None,
            account_id: None,
            execution_mode: aifuel_core::ExecutionMode::Project,
            permission_profile: request.access,
            status,
            exit_code: Some(0),
            output: output.to_owned(),
            error: None,
            diagnostics: None,
            usage: None,
            timed_out: false,
            quota_exhausted: false,
            working_directory: request
                .working_directory
                .clone()
                .unwrap_or_else(|| PathBuf::from("/tmp")),
        }
    }
}

impl std::fmt::Debug for FakeExecution {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("FakeExecution")
    }
}

/// A no-op sink so `execute` shares the scripted path without a listener.
#[derive(Debug)]
struct DiscardOutput;

impl AgentRunOutputHandler for DiscardOutput {
    fn on_output(&self, _delta: &str) {}
}

impl AgentExecutionAdapter for FakeExecution {
    fn integration(&self) -> IntegrationId {
        self.integration.clone()
    }

    fn provider(&self) -> ProviderId {
        self.provider.clone()
    }

    fn declared_agent_capabilities(&self) -> BTreeMap<AgentCapability, AgentCapabilityEvidence> {
        self.declared.clone()
    }

    fn validate(&self, request: &RunRequest) -> Result<(), AgentRunError> {
        if request.integration != self.integration() {
            return Err(AgentRunError::UnsupportedIntegration(
                request.integration.clone(),
            ));
        }
        if request.prompt.trim().is_empty() {
            return Err(AgentRunError::InvalidRequest(
                "prompt must not be empty".to_owned(),
            ));
        }
        // Mirror the production rule: an exact external tool selection
        // rides on the request, and only a declared `ExternalMcpTools`
        // executor can enforce it.
        if request.external_tools.is_some()
            && self
                .declared
                .get(&AgentCapability::ExternalMcpTools)
                .map(|evidence| evidence.state)
                != Some(CapabilityState::Supported)
        {
            return Err(AgentRunError::InvalidRequest(
                "the fake provider cannot enforce an exact external tool selection".to_owned(),
            ));
        }
        Ok(())
    }

    fn agent_info(&self) -> AgentIntegrationInfo {
        let declared: Vec<_> = self
            .declared
            .iter()
            .map(|(capability, evidence)| (*capability, evidence.clone()))
            .collect();
        AgentIntegrationInfo::from_inspection(
            self.provider.clone(),
            self.integration.clone(),
            aifuel_core::AgentPresenceEvidence {
                state: AgentPresenceState::Present,
                reason: "fake".to_owned(),
            },
            aifuel_core::AgentVersionEvidence {
                version: Some("1.0".to_owned()),
                reason: "fake".to_owned(),
            },
            aifuel_core::AgentAuthenticationEvidence {
                state: self.authentication,
                reason: "fake".to_owned(),
            },
            declared,
        )
        .with_setup_guidance(Some(AgentSetupGuidance {
            install: "fake",
            login: "fake",
            check: "fake",
            documentation_url: "fake",
        }))
    }

    fn execute(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
    ) -> Result<RunResult, AgentRunError> {
        self.execute_with_output_handler(request, cancellation, &DiscardOutput)
    }

    fn execute_with_output_handler(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
        output_handler: &dyn AgentRunOutputHandler,
    ) -> Result<RunResult, AgentRunError> {
        self.requests
            .lock()
            .expect("requests mutex")
            .push(request.clone());
        let run = self
            .script
            .lock()
            .expect("script mutex")
            .pop_front()
            .unwrap_or(FakeRun::Complete {
                status: RunStatus::Succeeded,
                output: "",
                session_id: None,
            });
        match run {
            FakeRun::Complete {
                status,
                output,
                session_id,
            } => Ok(self.result(request, status, output, session_id)),
            FakeRun::Streamed => {
                output_handler.on_output("streamed ");
                output_handler.on_output("answer");
                Ok(self.result(request, RunStatus::Succeeded, "streamed answer", None))
            }
            FakeRun::Interaction {
                kind,
                description,
                questions,
                requires_expanded_access,
                expect,
            } => {
                let handler = request
                    .interaction_handler
                    .as_ref()
                    .expect("an interaction run requires the session interaction handler");
                let response = handler.interact(
                    AgentInteractionRequest {
                        request_id: json!("native-1"),
                        method: "fake/request".to_owned(),
                        kind,
                        description: description.to_owned(),
                        questions: questions
                            .into_iter()
                            .map(|(id, text)| AgentInputQuestion {
                                id: id.to_owned(),
                                text: text.to_owned(),
                            })
                            .collect(),
                        parameters: json!({}),
                        requires_expanded_access,
                    },
                    cancellation,
                )?;
                assert!(
                    expect(&response),
                    "the mapped provider response shape is wrong"
                );
                Ok(self.result(request, RunStatus::Succeeded, "answered", None))
            }
            FakeRun::Blocking => {
                while !cancellation.is_cancelled() {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(AgentRunError::Cancelled)
            }
        }
    }
}

pub(super) fn adapter(script: Vec<FakeRun>) -> CliAdapter {
    CliAdapter::new(Arc::new(
        FakeExecution::new(ProviderKey::Claude).with_runs(script),
    ))
}

pub(super) fn integration(provider: ProviderKey) -> Integration {
    let id = provider.as_str();
    Integration {
        id: IntegrationId::new(id),
        provider: ProviderId::new(id),
        name: format!("Test {id}"),
        execution: ExecutionConfig::Cli {
            adapter: CliAdapterId::new(id),
        },
        monitoring: None,
    }
}

pub(super) fn options(provider: ProviderKey, access: AccessMode) -> StartOptions {
    let id = provider.as_str();
    StartOptions {
        cwd: PathBuf::from("/tmp"),
        selection: ModelSelection {
            integration_id: IntegrationId::new(id),
            model: format!("{id}-model"),
            effort: None,
        },
        access,
        resume_cursor: None,
        external_tools: Vec::new(),
        env: Default::default(),
        optimize: Default::default(),
    }
}

pub(super) fn input(text: &str) -> UserInput {
    UserInput {
        text: text.to_owned(),
        attachments: Vec::new(),
    }
}

/// Bounded read so a missing event fails the test instead of hanging it.
pub(super) fn recv(events: &mpsc::Receiver<AgentEventKind>) -> AgentEventKind {
    events
        .recv_timeout(Duration::from_secs(10))
        .expect("the next event arrives")
}

/// Read until `run.completed` (inclusive) and return every event seen.
pub(super) fn through_run_completed(
    events: &mpsc::Receiver<AgentEventKind>,
) -> Vec<AgentEventKind> {
    let mut kinds = Vec::new();
    for _ in 0..32 {
        let kind = recv(events);
        let terminal = matches!(kind, AgentEventKind::RunCompleted { .. });
        kinds.push(kind);
        if terminal {
            return kinds;
        }
    }
    panic!("run.completed never arrived: {kinds:?}")
}

/// Read until the session returns to `idle` after a completed run.
pub(super) fn through_idle(events: &mpsc::Receiver<AgentEventKind>) -> Vec<AgentEventKind> {
    let mut kinds = through_run_completed(events);
    for _ in 0..8 {
        let kind = recv(events);
        let idle = matches!(
            kind,
            AgentEventKind::SessionStatus {
                status: SessionStatus::Idle
            }
        );
        kinds.push(kind);
        if idle {
            return kinds;
        }
    }
    panic!("the session never returned to idle: {kinds:?}")
}

pub(super) fn test_events(
    adapter: &CliAdapter,
    handle: &AgentSessionHandle,
) -> mpsc::Receiver<AgentEventKind> {
    adapter
        .test_events(handle)
        .expect("the session's event receiver is attached")
}
