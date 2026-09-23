use super::capabilities::ExecutionCapabilities;
use super::output::OutputParser;
use super::process::{
    MAX_CAPTURE_BYTES, TemporaryDirectory, kill_and_wait, owned_command, program_candidates,
    read_bounded,
};
use aifuel_core::{
    AccessMode, AgentCapability, AgentCapabilityEvidence, AgentExecutionAdapter,
    AgentIntegrationInfo, AgentRunError, AgentRunOutputHandler, AgentSetupGuidance, ExecutionMode,
    OutputFormat, RunCancellationToken, RunRequest, RunResult, RunStatus,
};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) type AsyncRunExecutor =
    for<'a> fn(
        &'a RunRequest,
        &'a RunCancellationToken,
        Option<&'a dyn AgentRunOutputHandler>,
    ) -> Pin<Box<dyn Future<Output = Result<RunResult, AgentRunError>> + Send + 'a>>;

pub(crate) struct CliExecutionAdapter {
    provider: aifuel_core::ProviderKey,
    program: &'static str,
    preflight_args: &'static [&'static str],
    required_flags: &'static [&'static str],
    build_args: fn(&RunRequest) -> Result<Vec<String>, AgentRunError>,
    parse_output: OutputParser,
    capabilities: ExecutionCapabilities,
    executor: Option<AsyncRunExecutor>,
    version_probe_args: Option<&'static [&'static str]>,
    authentication_probe_args: Option<&'static [&'static str]>,
    setup_guidance: Option<AgentSetupGuidance>,
}

impl CliExecutionAdapter {
    pub(crate) const fn new(
        provider: aifuel_core::ProviderKey,
        program: &'static str,
        preflight_args: &'static [&'static str],
        required_flags: &'static [&'static str],
        build_args: fn(&RunRequest) -> Result<Vec<String>, AgentRunError>,
        parse_output: OutputParser,
        capabilities: ExecutionCapabilities,
    ) -> Self {
        Self {
            provider,
            program,
            preflight_args,
            required_flags,
            build_args,
            parse_output,
            capabilities,
            executor: None,
            version_probe_args: None,
            authentication_probe_args: None,
            setup_guidance: None,
        }
    }

    pub(crate) const fn with_executor(mut self, executor: AsyncRunExecutor) -> Self {
        self.executor = Some(executor);
        self
    }

    pub(crate) const fn with_version_probe(mut self, args: &'static [&'static str]) -> Self {
        self.version_probe_args = Some(args);
        self
    }

    pub(crate) const fn with_authentication_probe(mut self, args: &'static [&'static str]) -> Self {
        self.authentication_probe_args = Some(args);
        self
    }

    pub(crate) const fn with_setup_guidance(mut self, guidance: AgentSetupGuidance) -> Self {
        self.setup_guidance = Some(guidance);
        self
    }

    fn declared_capabilities(
        &self,
    ) -> std::collections::BTreeMap<AgentCapability, AgentCapabilityEvidence> {
        self.capabilities.evidence()
    }

    fn integration_info(&self) -> AgentIntegrationInfo {
        super::inspection::inspect_agent(
            self.provider,
            self.program,
            self.version_probe_args,
            self.authentication_probe_args,
            self.declared_capabilities(),
        )
        .with_setup_guidance(self.setup_guidance)
    }

    fn validate(&self, request: &RunRequest) -> Result<(), AgentRunError> {
        if request.external_tools.is_some() && !self.capabilities.supports_external_tools {
            return Err(AgentRunError::InvalidRequest(format!(
                "{} cannot enforce an exact external MCP tool selection",
                self.provider
            )));
        }
        if request.effort.is_some() && !self.capabilities.supports_effort {
            return Err(AgentRunError::InvalidRequest(format!(
                "{} cannot report a verified effort setting",
                self.provider
            )));
        }
        if request.resume.is_some() && !self.capabilities.supports_resume {
            return Err(AgentRunError::InvalidRequest(format!(
                "{} does not support explicit session continuation",
                self.provider
            )));
        }
        if request.account.is_some() && !self.capabilities.supports_account_selection {
            return Err(AgentRunError::InvalidRequest(format!(
                "{} does not expose provider account selection",
                self.provider
            )));
        }
        if request.access == AccessMode::WorkspaceWrite
            && !self.capabilities.supports_workspace_write
        {
            return Err(AgentRunError::InvalidRequest(format!(
                "{} cannot enforce workspace-write access",
                self.provider
            )));
        }
        if request.output == OutputFormat::Jsonl && !self.capabilities.supports_jsonl {
            return Err(AgentRunError::InvalidRequest(format!(
                "{} cannot provide verified JSONL output",
                self.provider
            )));
        }
        Ok(())
    }

    async fn preflight(
        &self,
        started_at: Instant,
        timeout: Option<Duration>,
        cancellation: &RunCancellationToken,
    ) -> Result<String, AgentRunError> {
        // Capability setup is always bounded, even when a caller explicitly
        // requests an unbounded task. An explicit task deadline may make this
        // window shorter, but never longer.
        let setup_deadline = Instant::now() + PREFLIGHT_TIMEOUT;
        let deadline = timeout
            .map(|timeout| started_at + timeout)
            .map_or(setup_deadline, |deadline| deadline.min(setup_deadline));
        self.preflight_async(deadline, cancellation).await
    }

    async fn preflight_async(
        &self,
        deadline: Instant,
        cancellation: &RunCancellationToken,
    ) -> Result<String, AgentRunError> {
        let mut resolved = None;
        for candidate in program_candidates(self.program) {
            let mut command = owned_command(&candidate, |command| {
                command
                    .args(self.preflight_args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
            });
            match command.spawn() {
                Ok(process) => {
                    resolved = Some((candidate, process));
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(AgentRunError::Io(error)),
            }
        }
        let (program, mut child) = resolved.ok_or_else(|| {
            AgentRunError::InvalidRequest(format!(
                "provider executable {:?} was not found",
                self.program
            ))
        })?;
        let stdout = child
            .stdout()
            .take()
            .expect("preflight stdout was requested");
        let stderr = child
            .stderr()
            .take()
            .expect("preflight stderr was requested");
        let stdout_reader = tokio::spawn(read_bounded(stdout, MAX_CAPTURE_BYTES));
        let stderr_reader = tokio::spawn(read_bounded(stderr, MAX_CAPTURE_BYTES));
        let status = loop {
            if let Some(status) = child.try_wait().map_err(AgentRunError::Io)? {
                break status;
            }
            if cancellation.is_cancelled() {
                let _ = kill_and_wait(&mut child).await;
                let _ = stdout_reader.await;
                let _ = stderr_reader.await;
                return Err(AgentRunError::Cancelled);
            }
            if Instant::now() >= deadline {
                kill_and_wait(&mut child).await.map_err(AgentRunError::Io)?;
                let _ = stdout_reader.await;
                let _ = stderr_reader.await;
                return Err(AgentRunError::Timeout(
                    "provider capability preflight timed out".to_owned(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let stdout = stdout_reader
            .await
            .map_err(|_| {
                AgentRunError::InvalidRequest("preflight stdout reader panicked".to_owned())
            })?
            .map_err(AgentRunError::Io)?;
        let stderr = stderr_reader
            .await
            .map_err(|_| {
                AgentRunError::InvalidRequest("preflight stderr reader panicked".to_owned())
            })?
            .map_err(AgentRunError::Io)?;
        let help = format!("{}{}", stdout.text, stderr.text);
        if status.success() && self.required_flags.iter().all(|flag| help.contains(flag)) {
            Ok(program)
        } else {
            Err(AgentRunError::InvalidRequest(format!(
                "provider executable {:?} failed capability preflight for {}",
                self.program, self.provider
            )))
        }
    }
}

impl AgentExecutionAdapter for CliExecutionAdapter {
    fn provider(&self) -> aifuel_core::ProviderKey {
        self.provider
    }

    fn setup_guidance(&self) -> Option<AgentSetupGuidance> {
        self.setup_guidance
    }

    fn declared_agent_capabilities(
        &self,
    ) -> std::collections::BTreeMap<AgentCapability, AgentCapabilityEvidence> {
        self.declared_capabilities()
    }

    fn agent_info(&self) -> AgentIntegrationInfo {
        self.integration_info()
    }

    fn validate(&self, request: &RunRequest) -> Result<(), AgentRunError> {
        // Call the inherent metadata validator explicitly. Calling
        // `self.validate` here would recurse into this trait method.
        CliExecutionAdapter::validate(self, request)
    }

    fn execute(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
    ) -> Result<RunResult, AgentRunError> {
        self.execute_sync(request, cancellation, None)
    }

    fn execute_with_output_handler(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
        output_handler: &dyn AgentRunOutputHandler,
    ) -> Result<RunResult, AgentRunError> {
        self.execute_sync(request, cancellation, Some(output_handler))
    }
}

impl CliExecutionAdapter {
    fn execute_sync(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
        output_handler: Option<&dyn AgentRunOutputHandler>,
    ) -> Result<RunResult, AgentRunError> {
        let request = request.clone();
        let cancellation = cancellation.clone();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                    .map_err(|error| AgentRunError::Io(io::Error::other(error)))?;
                runtime.block_on(self.execute_async(&request, &cancellation, output_handler))
            });
            worker.join().map_err(|_| {
                AgentRunError::InvalidRequest("provider execution worker panicked".to_owned())
            })?
        })
    }

    async fn execute_async(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
        output_handler: Option<&dyn AgentRunOutputHandler>,
    ) -> Result<RunResult, AgentRunError> {
        if request.provider != self.provider {
            return Err(AgentRunError::UnsupportedProvider(request.provider));
        }
        if request.prompt.trim().is_empty() {
            return Err(AgentRunError::InvalidRequest(
                "prompt must not be empty".to_owned(),
            ));
        }
        self.validate(request)?;
        if let Some(executor) = self.executor {
            return executor(request, cancellation, output_handler).await;
        }

        let started_at = Instant::now();
        let args = (self.build_args)(request)?;
        let program = self
            .preflight(started_at, request.timeout, cancellation)
            .await?;
        if cancellation.is_cancelled() {
            return Err(AgentRunError::Cancelled);
        }

        let temporary_directory = if request.working_directory.is_none() {
            Some(TemporaryDirectory::new()?)
        } else {
            None
        };
        let working_directory = request
            .working_directory
            .as_deref()
            .or_else(|| temporary_directory.as_ref().map(TemporaryDirectory::path))
            .expect("Agent Run always has a working directory");

        let mut command = owned_command(&program, |command| {
            command
                .args(&args)
                .current_dir(working_directory)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        });
        let mut child = command.spawn().map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => AgentRunError::InvalidRequest(format!(
                "provider executable {program:?} was not found"
            )),
            _ => AgentRunError::Io(error),
        })?;
        let stdout = child.stdout().take().expect("stdout was requested");
        let stderr = child.stderr().take().expect("stderr was requested");
        let stdout_reader = tokio::spawn(read_bounded(stdout, MAX_CAPTURE_BYTES));
        let stderr_reader = tokio::spawn(read_bounded(stderr, MAX_CAPTURE_BYTES));

        let deadline = request.timeout.map(|timeout| started_at + timeout);
        let mut timed_out = false;
        let mut cancelled = false;
        let exit_code = loop {
            if let Some(status) = child.try_wait().map_err(AgentRunError::Io)? {
                break status.code();
            }
            if cancellation.is_cancelled() {
                cancelled = true;
                break kill_and_wait(&mut child)
                    .await
                    .map_err(AgentRunError::Io)?
                    .code();
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                timed_out = true;
                break kill_and_wait(&mut child)
                    .await
                    .map_err(AgentRunError::Io)?
                    .code();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        let output = stdout_reader
            .await
            .map_err(|_| AgentRunError::InvalidRequest("stdout reader panicked".to_owned()))?
            .map_err(AgentRunError::Io)?;
        let error_output = stderr_reader
            .await
            .map_err(|_| AgentRunError::InvalidRequest("stderr reader panicked".to_owned()))?
            .map_err(AgentRunError::Io)?;
        let parsed = (self.parse_output)(request.output, &output.text);
        let status = if cancelled {
            RunStatus::Cancelled
        } else if timed_out {
            RunStatus::Timeout
        } else if exit_code != Some(0) {
            RunStatus::Failed
        } else if parsed.terminal == Some(false) || (parsed.structured && parsed.terminal.is_none())
        {
            // A zero exit status is not enough for a structured provider
            // protocol. A failed or incomplete turn remains a failed run.
            RunStatus::Failed
        } else {
            RunStatus::Succeeded
        };

        let run_id = format!(
            "run-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let local_session_id = format!("session-{run_id}");
        let diagnostics = if error_output.text.trim().is_empty() {
            parsed.diagnostics.clone()
        } else if let Some(provider_diagnostics) = parsed.diagnostics.as_deref() {
            Some(format!("{}\n{}", error_output.text, provider_diagnostics))
        } else {
            Some(error_output.text.clone())
        };
        let error = if cancelled {
            Some("agent run was cancelled".to_owned())
        } else if timed_out {
            Some("agent run timed out".to_owned())
        } else if status == RunStatus::Succeeded {
            None
        } else if let Some(provider_error) = parsed.diagnostics {
            Some(provider_error)
        } else if let Some(diagnostics) = &diagnostics {
            Some(diagnostics.clone())
        } else {
            Some(format!("provider exited with {exit_code:?}"))
        };

        Ok(RunResult {
            run_id,
            local_session_id,
            session_id: parsed.session_id,
            provider_id: request.provider,
            requested_model: request.model.clone(),
            requested_effort: request.effort.clone(),
            effective_model: parsed.effective_model,
            effective_effort: None,
            requested_account_id: request.account.clone(),
            account_id: None,
            execution_mode: if request.working_directory.is_some() {
                ExecutionMode::Project
            } else {
                ExecutionMode::PromptOnly
            },
            permission_profile: request.access,
            status,
            exit_code,
            output: parsed.output,
            error,
            diagnostics,
            timed_out,
            resumed_from: request.resume.clone(),
            working_directory: working_directory.to_path_buf(),
        })
    }
}
