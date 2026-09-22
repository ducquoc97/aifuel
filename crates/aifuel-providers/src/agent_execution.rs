use aifuel_core::{
    AccessMode, AgentExecutionAdapter, AgentRunError, ExecutionMode, OutputFormat,
    RunCancellationToken, RunRequest, RunResult, RunStatus,
};
use process_wrap::tokio::{KillOnDrop, TokioChildWrapper, TokioCommandWrap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt};

#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;

const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
const CAPTURE_READ_BUFFER_BYTES: usize = 16 * 1024;

/// Provider-local parsing keeps native wire formats out of the common run
/// contract. Parsers return only public answer text and metadata explicitly
/// reported by the provider. Reasoning and tool output are never copied into
/// the normalized answer.
#[derive(Debug, Default)]
pub(crate) struct ParsedProviderOutput {
    pub output: String,
    pub session_id: Option<String>,
    pub effective_model: Option<String>,
    pub diagnostics: Option<String>,
    pub structured: bool,
    pub terminal: Option<bool>,
}

pub(crate) type OutputParser = fn(OutputFormat, &str) -> ParsedProviderOutput;

/// Conservative parser shared by adapters whose current native output does
/// not expose a provider-specific schema through the compiled interface. It
/// extracts public text fields when present and otherwise preserves plain
/// output. Structured records are only considered terminal when a terminal
/// event is explicitly observed.
pub(crate) fn parse_public_output(format: OutputFormat, text: &str) -> ParsedProviderOutput {
    if format == OutputFormat::Text {
        return ParsedProviderOutput {
            output: text.to_owned(),
            terminal: Some(true),
            ..ParsedProviderOutput::default()
        };
    }

    let mut parsed = ParsedProviderOutput {
        structured: true,
        ..ParsedProviderOutput::default()
    };
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            parsed.output.push_str(line);
            parsed.output.push('\n');
            parsed.structured = false;
            continue;
        };
        if let Some(session) = value
            .get("session_id")
            .or_else(|| value.get("sessionId"))
            .and_then(serde_json::Value::as_str)
        {
            parsed.session_id = Some(session.to_owned());
        }
        if let Some(model) = value
            .get("model")
            .or_else(|| value.get("model_id"))
            .and_then(serde_json::Value::as_str)
        {
            parsed.effective_model = Some(model.to_owned());
        }
        let event_type = value.get("type").and_then(serde_json::Value::as_str);
        let public_text = value
            .get("text")
            .or_else(|| value.get("content"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| value.get("message").and_then(serde_json::Value::as_str));
        if let Some(public_text) = public_text {
            parsed.output.push_str(public_text);
        }
        if matches!(
            event_type,
            Some("completed" | "result" | "turn.completed" | "message_stop")
        ) {
            parsed.terminal = Some(true);
        } else if matches!(event_type, Some("error" | "failed" | "turn.failed")) {
            parsed.terminal = Some(false);
            parsed.diagnostics = public_text.map(str::to_owned);
        }
    }
    if parsed.terminal.is_none() && !parsed.output.is_empty() {
        parsed.terminal = Some(true);
    }
    parsed
}

pub(crate) struct CliExecutionAdapter {
    provider: aifuel_core::ProviderKey,
    program: &'static str,
    preflight_args: &'static [&'static str],
    required_flags: &'static [&'static str],
    build_args: fn(&RunRequest) -> Result<Vec<String>, AgentRunError>,
    parse_output: OutputParser,
    capabilities: ExecutionCapabilities,
}

pub(crate) struct ExecutionCapabilities {
    supports_resume: bool,
    supports_account_selection: bool,
    supports_workspace_write: bool,
    supports_jsonl: bool,
}

impl ExecutionCapabilities {
    pub(crate) const fn new(
        supports_resume: bool,
        supports_account_selection: bool,
        supports_workspace_write: bool,
        supports_jsonl: bool,
    ) -> Self {
        Self {
            supports_resume,
            supports_account_selection,
            supports_workspace_write,
            supports_jsonl,
        }
    }
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
        }
    }

    fn validate(&self, request: &RunRequest) -> Result<(), AgentRunError> {
        if request.external_tools.is_some() {
            return Err(AgentRunError::InvalidRequest(format!(
                "{} cannot enforce an exact external MCP tool selection",
                self.provider
            )));
        }
        if request.effort.is_some() {
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
        let request = request.clone();
        let cancellation = cancellation.clone();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                    .map_err(|error| AgentRunError::Io(io::Error::other(error)))?;
                runtime.block_on(self.execute_async(&request, &cancellation))
            });
            worker.join().map_err(|_| {
                AgentRunError::InvalidRequest("provider execution worker panicked".to_owned())
            })?
        })
    }
}

impl CliExecutionAdapter {
    async fn execute_async(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
    ) -> Result<RunResult, AgentRunError> {
        if request.provider != self.provider {
            return Err(AgentRunError::UnsupportedProvider(request.provider));
        }
        if request.prompt.trim().is_empty() {
            return Err(AgentRunError::InvalidRequest(
                "prompt must not be empty".to_owned(),
            ));
        }

        let started_at = Instant::now();
        self.validate(request)?;
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

fn owned_command(
    program: &str,
    configure: impl FnOnce(&mut tokio::process::Command),
) -> TokioCommandWrap {
    let mut command = TokioCommandWrap::with_new(program, |command| {
        // Managed providers must not recursively start another AI Fuel
        // execution owner. The executable boundary rejects this marker.
        command.env("AIFUEL_MANAGED_RUN", "1");
        configure(command);
    });
    #[cfg(unix)]
    command.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(JobObject);
    command.wrap(KillOnDrop);
    command
}

async fn kill_and_wait(child: &mut Box<dyn TokioChildWrapper>) -> io::Result<ExitStatus> {
    // Process-group/job wrappers terminate descendants as well as the direct
    // child. Ignore a race where the group has already exited, then always
    // wait so owned descendants are reaped before returning to the caller.
    let kill_error = child.start_kill().err();
    match Box::into_pin(child.wait()).await {
        Ok(status) => Ok(status),
        Err(wait_error) => Err(kill_error.unwrap_or(wait_error)),
    }
}

#[derive(Debug)]
struct CapturedOutput {
    text: String,
    #[allow(dead_code)]
    bytes: usize,
    #[allow(dead_code)]
    truncated: bool,
}

async fn read_bounded<R>(mut reader: R, limit: usize) -> io::Result<CapturedOutput>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = 0usize;
    let mut captured = Vec::with_capacity(limit.min(CAPTURE_READ_BUFFER_BYTES));
    let mut buffer = vec![0u8; CAPTURE_READ_BUFFER_BYTES];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        bytes = bytes.saturating_add(count);
        if captured.len() < limit {
            let remaining = limit - captured.len();
            captured.extend_from_slice(&buffer[..count.min(remaining)]);
        }
    }
    Ok(CapturedOutput {
        text: String::from_utf8_lossy(&captured).into_owned(),
        bytes,
        truncated: bytes > captured.len(),
    })
}

pub(crate) fn program_candidates(program: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        let mut candidates = vec![program.to_owned()];
        if !program.ends_with(".cmd") {
            candidates.push(format!("{program}.cmd"));
        }
        if !program.ends_with(".bat") {
            candidates.push(format!("{program}.bat"));
        }
        candidates
    }
    #[cfg(not(windows))]
    {
        vec![program.to_owned()]
    }
}

struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    fn new() -> Result<Self, io::Error> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aifuel-run-{}-{stamp}", std::process::id()));
        fs::create_dir(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

#[cfg(test)]
#[path = "agent_execution_tests.rs"]
mod tests;
