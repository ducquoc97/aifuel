use aifuel_core::{
    AccessMode, AgentExecutionAdapter, AgentRunError, ExecutionMode, OutputFormat,
    RunCancellationToken, RunRequest, RunResult, RunStatus,
};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) struct CliExecutionAdapter {
    provider: aifuel_core::ProviderKey,
    program: &'static str,
    preflight_args: &'static [&'static str],
    required_flags: &'static [&'static str],
    build_args: fn(&RunRequest) -> Result<Vec<String>, AgentRunError>,
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
        capabilities: ExecutionCapabilities,
    ) -> Self {
        Self {
            provider,
            program,
            preflight_args,
            required_flags,
            build_args,
            capabilities,
        }
    }

    fn validate(&self, request: &RunRequest) -> Result<(), AgentRunError> {
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

    fn preflight(
        &self,
        timeout: Option<Duration>,
        started_at: Instant,
        cancellation: &RunCancellationToken,
    ) -> Result<String, AgentRunError> {
        let mut program = None;
        let mut child = None;
        for candidate in program_candidates(self.program) {
            match Command::new(&candidate)
                .args(self.preflight_args)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(process) => {
                    program = Some(candidate);
                    child = Some(process);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(AgentRunError::Io(error)),
            }
        }
        let program = program.ok_or_else(|| {
            AgentRunError::InvalidRequest(format!(
                "provider executable {:?} was not found",
                self.program
            ))
        })?;
        let mut child =
            ManagedChild::new(child.expect("a candidate process exists with a resolved program"));
        let stdout = child
            .child_mut()
            .stdout
            .take()
            .expect("preflight stdout was requested");
        let reader = thread::spawn(move || read_output(stdout));
        let deadline = timeout.map(|timeout| started_at + timeout);
        let status = loop {
            if let Some(status) = child.try_wait().map_err(AgentRunError::Io)? {
                break status;
            }
            if cancellation.is_cancelled() {
                let _ = child.kill_and_wait();
                let _ = reader.join();
                return Err(AgentRunError::Cancelled);
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                child.kill_and_wait().map_err(AgentRunError::Io)?;
                let _ = reader.join();
                return Err(AgentRunError::Timeout(
                    "provider capability preflight timed out".to_owned(),
                ));
            }
            thread::sleep(Duration::from_millis(10));
        };
        let help = reader
            .join()
            .map_err(|_| AgentRunError::InvalidRequest("preflight reader panicked".to_owned()))??;
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

    fn execute(
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

        let started_at = Instant::now();
        self.validate(request)?;
        let args = (self.build_args)(request)?;
        let program = self.preflight(request.timeout, started_at, cancellation)?;
        if cancellation.is_cancelled() {
            return Err(AgentRunError::Cancelled);
        }

        let mut command = Command::new(&program);
        command
            .args(args)
            .current_dir(working_directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = ManagedChild::new(command.spawn().map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => AgentRunError::InvalidRequest(format!(
                "provider executable {program:?} was not found"
            )),
            _ => AgentRunError::Io(error),
        })?);
        let stdout = child
            .child_mut()
            .stdout
            .take()
            .expect("stdout was requested");
        let stderr = child
            .child_mut()
            .stderr
            .take()
            .expect("stderr was requested");
        let stdout_reader = thread::spawn(move || read_stream(stdout));
        let stderr_reader = thread::spawn(move || read_stream(stderr));

        let timeout = request
            .timeout
            .map(|timeout| timeout.saturating_sub(started_at.elapsed()));
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let mut timed_out = false;
        let mut cancelled = false;
        let exit_code = loop {
            if let Some(status) = child.try_wait().map_err(AgentRunError::Io)? {
                break status.code();
            }
            if cancellation.is_cancelled() {
                cancelled = true;
                break child.kill_and_wait().map_err(AgentRunError::Io)?.code();
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                timed_out = true;
                break child.kill_and_wait().map_err(AgentRunError::Io)?.code();
            }
            thread::sleep(Duration::from_millis(10));
        };

        let output = stdout_reader
            .join()
            .map_err(|_| AgentRunError::InvalidRequest("stdout reader panicked".to_owned()))??;
        let error_output = stderr_reader
            .join()
            .map_err(|_| AgentRunError::InvalidRequest("stderr reader panicked".to_owned()))??;
        let status = if cancelled {
            RunStatus::Cancelled
        } else if timed_out {
            RunStatus::Timeout
        } else if exit_code == Some(0) {
            RunStatus::Succeeded
        } else {
            RunStatus::Failed
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
        let diagnostics = (!error_output.trim().is_empty()).then_some(error_output);
        let error = if cancelled {
            Some("agent run was cancelled".to_owned())
        } else if exit_code == Some(0) && !timed_out {
            None
        } else if let Some(diagnostics) = &diagnostics {
            Some(diagnostics.clone())
        } else {
            Some(format!("provider exited with {exit_code:?}"))
        };

        Ok(RunResult {
            run_id,
            local_session_id,
            session_id: None,
            provider_id: request.provider,
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
            status,
            exit_code,
            output,
            error,
            diagnostics,
            timed_out,
            resumed_from: request.resume.clone(),
            working_directory: working_directory.to_path_buf(),
        })
    }
}

struct ManagedChild {
    child: Child,
    reaped: bool,
}

impl ManagedChild {
    fn new(child: Child) -> Self {
        Self {
            child,
            reaped: false,
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        self.reaped = status.is_some();
        Ok(status)
    }

    fn kill_and_wait(&mut self) -> io::Result<ExitStatus> {
        self.child.kill()?;
        self.wait()
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait()?;
        self.reaped = true;
        Ok(status)
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn program_candidates(program: &str) -> Vec<String> {
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

fn read_output<R: Read>(mut reader: R) -> Result<String, io::Error> {
    let mut output = String::new();
    reader.read_to_string(&mut output)?;
    Ok(output)
}

fn read_stream<R: Read>(mut reader: R) -> Result<String, io::Error> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
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
