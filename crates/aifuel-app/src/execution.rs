use aifuel_core::{
    AgentExecutionAdapter, AgentRunError, RunCancellationToken, RunRequest, RunResult,
};
use std::fs;

/// Shared application entry point for explicit Agent Runs.
///
/// The facade selects only the adapter registered for the requested provider.
/// It is independent of Provider Discovery and the monitoring and Agent MCP
/// Registration capabilities.
pub struct AgentRunFacade<'a> {
    adapters: &'a [&'a dyn AgentExecutionAdapter],
}

impl<'a> AgentRunFacade<'a> {
    pub const fn new(adapters: &'a [&'a dyn AgentExecutionAdapter]) -> Self {
        Self { adapters }
    }

    /// Execute the explicitly selected provider, or return an error when no
    /// adapter is registered for it. This call blocks until the provider run
    /// finishes, times out, or observes cancellation.
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
            .adapters
            .iter()
            .find(|adapter| adapter.provider() == request.provider)
            .ok_or(AgentRunError::UnsupportedProvider(request.provider))?;

        adapter.execute(&request, cancellation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aifuel_core::{AccessMode, ExecutionMode, OutputFormat, ProviderKey, RunStatus};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    struct ProbeAdapter {
        provider: ProviderKey,
        calls: Arc<AtomicUsize>,
        cancelled: Arc<AtomicBool>,
    }

    impl AgentExecutionAdapter for ProbeAdapter {
        fn provider(&self) -> ProviderKey {
            self.provider
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
                provider_id: self.provider,
                requested_model: request.model.clone(),
                effective_model: None,
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
                working_directory: std::env::temp_dir(),
            })
        }
    }

    fn request(provider: ProviderKey) -> RunRequest {
        RunRequest {
            provider,
            model: None,
            account: None,
            prompt: "hello".to_owned(),
            output: OutputFormat::Text,
            working_directory: None,
            access: AccessMode::ReadOnly,
            resume: None,
            timeout: Some(Duration::from_secs(1)),
        }
    }

    #[test]
    fn facade_dispatches_exact_provider_and_passes_cancellation() {
        let claude_calls = Arc::new(AtomicUsize::new(0));
        let codex_calls = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicBool::new(false));
        let claude = ProbeAdapter {
            provider: ProviderKey::Claude,
            calls: Arc::clone(&claude_calls),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let codex = ProbeAdapter {
            provider: ProviderKey::Codex,
            calls: Arc::clone(&codex_calls),
            cancelled: Arc::clone(&cancelled),
        };
        let adapters: [&dyn AgentExecutionAdapter; 2] = [&claude, &codex];
        let facade = AgentRunFacade::new(&adapters);
        let cancellation = RunCancellationToken::new();
        cancellation.cancel();

        let result = facade
            .execute(&request(ProviderKey::Codex), &cancellation)
            .expect("the selected execution adapter should run");

        assert_eq!(claude_calls.load(Ordering::Relaxed), 0);
        assert_eq!(codex_calls.load(Ordering::Relaxed), 1);
        assert!(cancelled.load(Ordering::Relaxed));
        assert_eq!(result.provider_id, ProviderKey::Codex);
        assert!(result.session_id.is_none());
        assert_eq!(result.status, RunStatus::Cancelled);
    }

    #[test]
    fn absent_execution_capability_is_explicit_and_does_not_fallback() {
        let facade = AgentRunFacade::new(&[]);

        let error = facade
            .execute(
                &request(ProviderKey::Antigravity),
                &RunCancellationToken::new(),
            )
            .expect_err("an unregistered provider must remain unsupported");

        assert!(matches!(
            error,
            AgentRunError::UnsupportedProvider(ProviderKey::Antigravity)
        ));
    }
}
