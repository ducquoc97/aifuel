use aifuel_core::ProviderKey;
use serde::Serialize;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod integration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum AccessMode {
    #[serde(rename = "read-only")]
    ReadOnly,
    #[serde(rename = "workspace-write")]
    WorkspaceWrite,
}

impl AccessMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "read-only" => Ok(Self::ReadOnly),
            "workspace-write" => Ok(Self::WorkspaceWrite),
            _ => Err(format!(
                "invalid access mode {value:?}; expected read-only or workspace-write"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum OutputFormat {
    Text,
    Json,
    Jsonl,
}

impl OutputFormat {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            "jsonl" => Ok(Self::Jsonl),
            _ => Err(format!(
                "invalid output format {value:?}; expected text, json, or jsonl"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ExecutionMode {
    #[serde(rename = "prompt-only")]
    PromptOnly,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Succeeded,
    Failed,
    Timeout,
}

#[derive(Debug, Clone)]
pub struct RunRequest {
    pub provider: ProviderKey,
    pub model: Option<String>,
    pub account: Option<String>,
    pub prompt: String,
    pub output: OutputFormat,
    pub working_directory: Option<PathBuf>,
    pub access: AccessMode,
    pub resume: Option<String>,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Serialize)]
pub struct RunResult {
    pub run_id: String,
    pub local_session_id: String,
    pub session_id: Option<String>,
    pub resumed_from: Option<String>,
    pub provider_id: ProviderKey,
    pub requested_model: Option<String>,
    pub effective_model: Option<String>,
    pub requested_account_id: Option<String>,
    pub account_id: Option<String>,
    pub execution_mode: ExecutionMode,
    pub permission_profile: AccessMode,
    pub status: RunStatus,
    pub exit_code: Option<i32>,
    pub output: String,
    pub error: Option<String>,
    pub diagnostics: Option<String>,
    pub timed_out: bool,
    pub working_directory: PathBuf,
}

#[derive(Debug)]
pub enum LaunchError {
    InvalidRequest(String),
    UnsupportedProvider(ProviderKey),
    Io(io::Error),
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(message) => f.write_str(message),
            Self::UnsupportedProvider(provider) => {
                write!(f, "provider {provider} has no verified agent integration")
            }
            Self::Io(error) => write!(f, "launcher I/O failed: {error}"),
        }
    }
}

impl std::error::Error for LaunchError {}

impl From<io::Error> for LaunchError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub fn execute(request: &RunRequest) -> Result<RunResult, LaunchError> {
    if request.prompt.trim().is_empty() {
        return Err(LaunchError::InvalidRequest(
            "prompt must not be empty".to_owned(),
        ));
    }
    let working_directory = request
        .working_directory
        .as_ref()
        .map(fs::canonicalize)
        .transpose()
        .map_err(|error| {
            LaunchError::InvalidRequest(format!("working directory is unavailable: {error}"))
        })?;
    if let Some(directory) = &working_directory {
        if !directory.is_dir() {
            return Err(LaunchError::InvalidRequest(format!(
                "working directory is not an existing directory: {}",
                directory.display()
            )));
        }
    }
    if request.account.is_some() {
        return Err(LaunchError::InvalidRequest(
            "explicit account selection is not verified by the selected provider CLI".to_owned(),
        ));
    }

    let temporary_directory = if working_directory.is_none() {
        Some(TemporaryDirectory::new()?)
    } else {
        None
    };
    let effective_working_directory = working_directory
        .as_deref()
        .or_else(|| temporary_directory.as_ref().map(TemporaryDirectory::path));

    let integration = integration::for_provider(request.provider)?;
    integration.validate(request)?;
    let args = integration::command_for(request, &integration)?;
    integration::preflight(&integration)?;
    let mut command = Command::new(integration.program());
    command
        .args(args)
        .current_dir(effective_working_directory.expect("launcher always has a working directory"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => LaunchError::InvalidRequest(format!(
            "provider executable {:?} was not found",
            integration.program()
        )),
        _ => LaunchError::Io(error),
    })?;
    let stdout = child.stdout.take().expect("stdout was requested");
    let stderr = child.stderr.take().expect("stderr was requested");
    let stdout_reader = thread::spawn(move || read_stream(stdout));
    let stderr_reader = thread::spawn(move || read_stream(stderr));

    let deadline = request.timeout.map(|timeout| Instant::now() + timeout);
    let mut timed_out = false;
    let exit_code = loop {
        if let Some(status) = child.try_wait().map_err(LaunchError::Io)? {
            break status.code();
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            timed_out = true;
            child.kill().map_err(LaunchError::Io)?;
            break child.wait().map_err(LaunchError::Io)?.code();
        }
        thread::sleep(Duration::from_millis(10));
    };

    let output = stdout_reader
        .join()
        .map_err(|_| LaunchError::InvalidRequest("stdout reader panicked".to_owned()))??;
    let error_output = stderr_reader
        .join()
        .map_err(|_| LaunchError::InvalidRequest("stderr reader panicked".to_owned()))??;
    let status = if timed_out {
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
    let error = if exit_code == Some(0) && !timed_out {
        None
    } else if let Some(diagnostics) = &diagnostics {
        Some(diagnostics.clone())
    } else {
        Some(format!("provider exited with {:?}", exit_code))
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
        execution_mode: if working_directory.is_some() {
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
        working_directory: effective_working_directory
            .expect("launcher always has a working directory")
            .to_path_buf(),
    })
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

pub fn read_prompt_file(path: &Path) -> Result<String, LaunchError> {
    fs::read_to_string(path).map_err(|error| {
        LaunchError::InvalidRequest(format!(
            "could not read prompt file {}: {error}",
            path.display()
        ))
    })
}

pub fn read_stdin_prompt() -> Result<String, LaunchError> {
    let mut prompt = String::new();
    io::stdin()
        .read_to_string(&mut prompt)
        .map_err(LaunchError::Io)?;
    Ok(prompt)
}
