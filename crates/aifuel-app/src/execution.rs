use aifuel_core::{
    AgentExecutionAdapter, AgentRunError, RunCancellationToken, RunRequest, RunResult,
};
use std::fs;

use crate::run_management::AgentExecutionAdapters;

/// Shared application entry point for explicit Agent Runs.
///
/// The facade selects only the adapter registered for the requested
/// integration. It is independent of Provider Discovery and the monitoring
/// and Agent MCP Registration capabilities.
pub struct AgentRunFacade {
    adapters: AgentExecutionAdapters,
}

impl AgentRunFacade {
    pub fn new(adapters: impl Into<AgentExecutionAdapters>) -> Self {
        Self {
            adapters: adapters.into(),
        }
    }

    fn iter(&self) -> impl Iterator<Item = &dyn AgentExecutionAdapter> {
        self.adapters.0.iter().map(|adapter| adapter.as_ref())
    }

    /// Execute the explicitly selected integration, or return an error when
    /// no adapter is registered for it. A bare provider id resolves only when
    /// it maps to exactly one registered integration. This call blocks until
    /// the provider run finishes, times out, or observes cancellation.
    pub fn execute(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
    ) -> Result<RunResult, AgentRunError> {
        if request.prompt.trim().is_empty() {
            return Err(AgentRunError::InvalidRequest(
                "prompt must not be empty".to_owned(),
            ));
        }

        let mut request = request.clone();
        request.integration = aifuel_core::resolve_integration(
            &request.integration,
            self.iter()
                .map(|adapter| (adapter.integration(), adapter.provider())),
        )?;
        request.working_directory = request
            .working_directory
            .as_ref()
            .map(fs::canonicalize)
            .transpose()
            .map_err(|error| {
                AgentRunError::InvalidRequest(format!("working directory is unavailable: {error}"))
            })?;
        if let Some(directory) = &request.working_directory
            && !directory.is_dir()
        {
            return Err(AgentRunError::InvalidRequest(format!(
                "working directory is not an existing directory: {}",
                directory.display()
            )));
        }

        let adapter = self
            .iter()
            .find(|adapter| adapter.integration() == request.integration)
            .expect("a resolved selection always names a registered adapter");

        adapter.execute(&request, cancellation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aifuel_core::{
        AccessMode, ExecutionMode, IntegrationId, OutputFormat, ProviderId, RunStatus,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    struct ProbeAdapter {
        integration: IntegrationId,
        provider: ProviderId,
        calls: Arc<AtomicUsize>,
        cancelled: Arc<AtomicBool>,
    }

    impl AgentExecutionAdapter for ProbeAdapter {
        fn integration(&self) -> IntegrationId {
            self.integration.clone()
        }

        fn provider(&self) -> ProviderId {
            self.provider.clone()
        }

        fn execute(
            &self,
            request: &RunRequest,
            cancellation: &RunCancellationToken,
        ) -> Result<RunResult, AgentRunError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.cancelled
                .store(cancellation.is_cancelled(), Ordering::Relaxed);
            Ok(RunResult {
                run_id: "local-run".to_owned(),
                local_session_id: "local-session".to_owned(),
                session_id: None,
                resumed_from: request.resume.clone(),
                provider_id: self.provider.clone(),
                integration_id: self.integration.clone(),
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
                status: if cancellation.is_cancelled() {
                    RunStatus::Cancelled
                } else {
                    RunStatus::Succeeded
                },
                exit_code: Some(0),
                output: request.prompt.clone(),
                error: None,
                diagnostics: None,
                timed_out: false,
                usage: None,
                quota_exhausted: false,
                working_directory: std::env::temp_dir(),
            })
        }
    }

    fn request(integration: IntegrationId) -> RunRequest {
        RunRequest {
            integration,
            model: None,
            effort: None,
            external_tools: None,
            account: None,
            prompt: "hello".to_owned(),
            output: OutputFormat::Text,
            working_directory: None,
            access: AccessMode::ReadOnly,
            resume: None,
            timeout: Some(Duration::from_secs(1)),
            env: Default::default(),
            interaction_handler: None,
        }
    }

    #[test]
    fn facade_dispatches_exact_integration_and_passes_cancellation() {
        let claude_calls = Arc::new(AtomicUsize::new(0));
        let codex_calls = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicBool::new(false));
        let claude = ProbeAdapter {
            integration: IntegrationId::new("claude"),
            provider: ProviderId::new("claude"),
            calls: Arc::clone(&claude_calls),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let codex = ProbeAdapter {
            integration: IntegrationId::new("codex"),
            provider: ProviderId::new("codex"),
            calls: Arc::clone(&codex_calls),
            cancelled: Arc::clone(&cancelled),
        };
        let facade = AgentRunFacade::new(vec![
            Arc::new(claude) as Arc<dyn AgentExecutionAdapter>,
            Arc::new(codex) as Arc<dyn AgentExecutionAdapter>,
        ]);
        let cancellation = RunCancellationToken::new();
        cancellation.cancel();

        let result = facade
            .execute(&request(IntegrationId::new("codex")), &cancellation)
            .expect("the selected execution adapter should run");

        assert_eq!(claude_calls.load(Ordering::Relaxed), 0);
        assert_eq!(codex_calls.load(Ordering::Relaxed), 1);
        assert!(cancelled.load(Ordering::Relaxed));
        assert_eq!(result.provider_id, ProviderId::new("codex"));
        assert_eq!(result.integration_id, IntegrationId::new("codex"));
        assert!(result.session_id.is_none());
        assert_eq!(result.status, RunStatus::Cancelled);
    }

    #[test]
    fn integrations_sharing_one_provider_route_independently() {
        // Two configured integrations may execute the same upstream provider,
        // for example a subscription CLI path and a billed API path. A bare
        // provider id must not collapse them into one route, while an exact
        // integration id selects precisely one.
        let cli_calls = Arc::new(AtomicUsize::new(0));
        let api_calls = Arc::new(AtomicUsize::new(0));
        let cli = ProbeAdapter {
            integration: IntegrationId::new("anthropic-cli"),
            provider: ProviderId::new("anthropic"),
            calls: Arc::clone(&cli_calls),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let api = ProbeAdapter {
            integration: IntegrationId::new("anthropic-api"),
            provider: ProviderId::new("anthropic"),
            calls: Arc::clone(&api_calls),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let facade = AgentRunFacade::new(vec![
            Arc::new(cli) as Arc<dyn AgentExecutionAdapter>,
            Arc::new(api) as Arc<dyn AgentExecutionAdapter>,
        ]);

        facade
            .execute(
                &request(IntegrationId::new("anthropic-api")),
                &RunCancellationToken::new(),
            )
            .expect("an explicit integration id routes exactly");
        assert_eq!(cli_calls.load(Ordering::Relaxed), 0);
        assert_eq!(api_calls.load(Ordering::Relaxed), 1);

        let error = facade
            .execute(
                &request(IntegrationId::new("anthropic")),
                &RunCancellationToken::new(),
            )
            .expect_err("a bare provider id cannot pick one of several integrations");
        assert!(
            matches!(
                error,
                AgentRunError::AmbiguousIntegration { ref provider, .. }
                    if provider.as_str() == "anthropic"
            ),
            "{error}"
        );
    }

    #[test]
    fn bare_provider_id_resolves_its_unique_integration() {
        let adapter = ProbeAdapter {
            integration: IntegrationId::new("codex-cli"),
            provider: ProviderId::new("codex"),
            calls: Arc::new(AtomicUsize::new(0)),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let facade = AgentRunFacade::new(vec![Arc::new(adapter) as Arc<dyn AgentExecutionAdapter>]);

        let result = facade
            .execute(
                &request(IntegrationId::new("codex")),
                &RunCancellationToken::new(),
            )
            .expect("a unique provider id resolves to its one integration");

        assert_eq!(result.integration_id, IntegrationId::new("codex-cli"));
        assert_eq!(result.provider_id, ProviderId::new("codex"));
    }

    #[test]
    fn absent_execution_capability_is_explicit_and_does_not_fallback() {
        let facade = AgentRunFacade::new(Vec::<Arc<dyn AgentExecutionAdapter>>::new());

        let error = facade
            .execute(
                &request(IntegrationId::new("antigravity")),
                &RunCancellationToken::new(),
            )
            .expect_err("an unregistered integration must remain unsupported");

        assert!(matches!(
            error,
            AgentRunError::UnsupportedIntegration(ref integration)
                if integration.as_str() == "antigravity"
        ));
    }
}
