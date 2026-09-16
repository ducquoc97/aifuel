use super::{AccessMode, LaunchError, OutputFormat, ProviderKey, RunRequest};
use std::io;
use std::process::Command;

pub(super) struct ProviderIntegration {
    provider: ProviderKey,
    program: &'static str,
    required_flags: &'static [&'static str],
    build_args: fn(&RunRequest) -> Result<Vec<String>, LaunchError>,
    supports_resume: bool,
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

    pub(super) fn program(&self) -> &'static str {
        self.program
    }
}

pub(super) fn for_provider(provider: ProviderKey) -> Result<ProviderIntegration, LaunchError> {
    match provider {
        ProviderKey::Gemini => Ok(ProviderIntegration {
            provider,
            program: "gemini",
            required_flags: &["--prompt", "--approval-mode", "--output-format"],
            build_args: gemini_args,
            supports_resume: true,
            supports_workspace_write: true,
            supports_jsonl: true,
        }),
        ProviderKey::Claude => Ok(ProviderIntegration {
            provider,
            program: "claude",
            required_flags: &["--print", "--permission-mode", "--output-format"],
            build_args: claude_args,
            supports_resume: true,
            supports_workspace_write: true,
            supports_jsonl: true,
        }),
        ProviderKey::Codex => Ok(ProviderIntegration {
            provider,
            program: "codex",
            required_flags: &["exec", "--sandbox"],
            build_args: codex_args,
            supports_resume: true,
            supports_workspace_write: true,
            supports_jsonl: true,
        }),
        ProviderKey::Copilot => Ok(ProviderIntegration {
            provider,
            program: "copilot",
            required_flags: &["--prompt", "--plan", "--output-format"],
            build_args: copilot_args,
            supports_resume: true,
            supports_workspace_write: false,
            supports_jsonl: false,
        }),
        ProviderKey::Antigravity => Err(LaunchError::UnsupportedProvider(provider)),
    }
}

pub(super) fn preflight(integration: &ProviderIntegration) -> Result<(), LaunchError> {
    let output = Command::new(integration.program)
        .arg("--help")
        .output()
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => LaunchError::InvalidRequest(format!(
                "provider executable {:?} was not found",
                integration.program
            )),
            _ => LaunchError::Io(error),
        })?;
    let help = String::from_utf8_lossy(&output.stdout);
    if output.status.success()
        && integration
            .required_flags
            .iter()
            .all(|flag| help.contains(flag))
    {
        Ok(())
    } else {
        Err(LaunchError::InvalidRequest(format!(
            "provider executable {:?} failed capability preflight for {}",
            integration.program, integration.provider
        )))
    }
}

pub(super) fn command_for(
    request: &RunRequest,
    integration: &ProviderIntegration,
) -> Result<Vec<String>, LaunchError> {
    (integration.build_args)(request)
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
