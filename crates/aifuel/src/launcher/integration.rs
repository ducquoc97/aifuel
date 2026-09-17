use super::{AccessMode, LaunchError, OutputFormat, ProviderKey, RunRequest};
use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub(super) struct ProviderIntegration {
    provider: ProviderKey,
    program: &'static str,
    preflight_args: &'static [&'static str],
    required_flags: &'static [&'static str],
    pub(super) build_args: fn(&RunRequest) -> Result<Vec<String>, LaunchError>,
    supports_resume: bool,
    supports_account_selection: bool,
    supports_workspace_write: bool,
    supports_jsonl: bool,
}

impl ProviderIntegration {
    pub(super) fn validate(&self, request: &RunRequest) -> Result<(), LaunchError> {
        if request.resume.is_some() && !self.supports_resume {
            return Err(LaunchError::InvalidRequest(format!(
                "{} does not support explicit session continuation",
                self.provider
            )));
        }
        if request.account.is_some() && !self.supports_account_selection {
            return Err(LaunchError::InvalidRequest(format!(
                "{} does not expose provider account selection",
                self.provider
            )));
        }
        if request.access == AccessMode::WorkspaceWrite && !self.supports_workspace_write {
            return Err(LaunchError::InvalidRequest(format!(
                "{} cannot enforce workspace-write access",
                self.provider
            )));
        }
        if request.output == OutputFormat::Jsonl && !self.supports_jsonl {
            return Err(LaunchError::InvalidRequest(format!(
                "{} cannot provide verified JSONL output",
                self.provider
            )));
        }
        Ok(())
    }
}

pub(super) fn for_provider(provider: ProviderKey) -> Result<ProviderIntegration, LaunchError> {
    match provider {
        ProviderKey::Gemini => Ok(ProviderIntegration {
            provider,
            program: "gemini",
            preflight_args: &["--help"],
            required_flags: &["--prompt", "--approval-mode", "--output-format"],
            build_args: gemini_args,
            supports_resume: true,
            supports_account_selection: false,
            supports_workspace_write: true,
            supports_jsonl: true,
        }),
        ProviderKey::Claude => Ok(ProviderIntegration {
            provider,
            program: "claude",
            preflight_args: &["--help"],
            required_flags: &["--print", "--permission-mode", "--output-format"],
            build_args: claude_args,
            supports_resume: true,
            supports_account_selection: false,
            supports_workspace_write: true,
            supports_jsonl: true,
        }),
        ProviderKey::Codex => Ok(ProviderIntegration {
            provider,
            program: "codex",
            preflight_args: &["exec", "--help"],
            required_flags: &["exec", "--sandbox"],
            build_args: codex_args,
            supports_resume: true,
            supports_account_selection: false,
            supports_workspace_write: true,
            supports_jsonl: true,
        }),
        ProviderKey::Copilot => Ok(ProviderIntegration {
            provider,
            program: "copilot",
            preflight_args: &["--help"],
            required_flags: &["--prompt", "--plan", "--output-format"],
            build_args: copilot_args,
            supports_resume: true,
            supports_account_selection: false,
            supports_workspace_write: false,
            supports_jsonl: false,
        }),
        ProviderKey::Antigravity => Err(LaunchError::UnsupportedProvider(provider)),
    }
}

pub(super) fn preflight(
    integration: &ProviderIntegration,
    timeout: Option<Duration>,
    started_at: Instant,
) -> Result<String, LaunchError> {
    let mut program = None;
    let mut child = None;
    for candidate in program_candidates(integration.program) {
        match Command::new(&candidate)
            .args(integration.preflight_args)
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
            Err(error) => return Err(LaunchError::Io(error)),
        }
    }
    let program = program.ok_or_else(|| {
        LaunchError::InvalidRequest(format!(
            "provider executable {:?} was not found",
            integration.program
        ))
    })?;
    let mut child = child.expect("a candidate process exists with a resolved program");
    let stdout = child.stdout.take().expect("preflight stdout was requested");
    let reader = thread::spawn(move || read_output(stdout));
    let deadline = timeout.map(|timeout| started_at + timeout);
    let status = loop {
        if let Some(status) = child.try_wait().map_err(LaunchError::Io)? {
            break status;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            child.kill().map_err(LaunchError::Io)?;
            let _ = child.wait().map_err(LaunchError::Io)?;
            let _ = reader.join();
            return Err(LaunchError::Timeout(
                "provider capability preflight timed out".to_owned(),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    let help = reader
        .join()
        .map_err(|_| LaunchError::InvalidRequest("preflight reader panicked".to_owned()))??;
    if status.success()
        && integration
            .required_flags
            .iter()
            .all(|flag| help.contains(flag))
    {
        Ok(program)
    } else {
        Err(LaunchError::InvalidRequest(format!(
            "provider executable {:?} failed capability preflight for {}",
            integration.program, integration.provider
        )))
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

fn gemini_args(request: &RunRequest) -> Result<Vec<String>, LaunchError> {
    let mut args = vec![
        "--prompt".to_owned(),
        request.prompt.clone(),
        "--skip-trust".to_owned(),
    ];
    append_model(&mut args, request);
    args.extend([
        "--approval-mode".to_owned(),
        match request.access {
            AccessMode::ReadOnly => "plan".to_owned(),
            AccessMode::WorkspaceWrite => "auto_edit".to_owned(),
        },
        "--output-format".to_owned(),
        provider_output_format(request).to_owned(),
    ]);
    append_resume(&mut args, request);
    Ok(args)
}

fn claude_args(request: &RunRequest) -> Result<Vec<String>, LaunchError> {
    let mut args = vec!["--print".to_owned(), request.prompt.clone()];
    append_model(&mut args, request);
    args.extend([
        "--permission-mode".to_owned(),
        match request.access {
            AccessMode::ReadOnly => "plan".to_owned(),
            AccessMode::WorkspaceWrite => "acceptEdits".to_owned(),
        },
        "--output-format".to_owned(),
        provider_output_format(request).to_owned(),
    ]);
    append_resume(&mut args, request);
    Ok(args)
}

fn codex_args(request: &RunRequest) -> Result<Vec<String>, LaunchError> {
    let mut args = vec!["exec".to_owned()];
    append_resume(&mut args, request);
    append_model(&mut args, request);
    args.extend([
        "--sandbox".to_owned(),
        match request.access {
            AccessMode::ReadOnly => "read-only".to_owned(),
            AccessMode::WorkspaceWrite => "workspace-write".to_owned(),
        },
    ]);
    if request.output != OutputFormat::Text {
        args.push("--json".to_owned());
    }
    args.push(request.prompt.clone());
    args.insert(1, "--skip-git-repo-check".to_owned());
    Ok(args)
}

fn copilot_args(request: &RunRequest) -> Result<Vec<String>, LaunchError> {
    let mut args = vec![
        "--prompt".to_owned(),
        request.prompt.clone(),
        "--plan".to_owned(),
    ];
    append_model(&mut args, request);
    args.extend([
        "--output-format".to_owned(),
        match request.output {
            OutputFormat::Text => "text",
            OutputFormat::Json => "json",
            OutputFormat::Jsonl => unreachable!("JSONL was rejected during preflight"),
        }
        .to_owned(),
    ]);
    append_resume(&mut args, request);
    Ok(args)
}

fn provider_output_format(request: &RunRequest) -> &'static str {
    match request.output {
        OutputFormat::Text => "text",
        OutputFormat::Json => "json",
        OutputFormat::Jsonl => "stream-json",
    }
}

fn append_model(args: &mut Vec<String>, request: &RunRequest) {
    if let Some(model) = &request.model {
        args.extend(["--model".to_owned(), model.clone()]);
    }
}

fn append_resume(args: &mut Vec<String>, request: &RunRequest) {
    if let Some(session) = &request.resume {
        args.extend(["--resume".to_owned(), session.clone()]);
    }
}
