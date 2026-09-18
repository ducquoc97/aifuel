use crate::agent_execution::{CliExecutionAdapter, ExecutionCapabilities};
use aifuel_core::{AgentRunError, OutputFormat, ProviderKey, RunRequest};

pub(crate) static ADAPTER: CliExecutionAdapter = CliExecutionAdapter::new(
    ProviderKey::Codex,
    "codex",
    &["exec", "--help"],
    &["exec", "--sandbox"],
    build_args,
    ExecutionCapabilities::new(true, false, true, true),
);

fn build_args(request: &RunRequest) -> Result<Vec<String>, AgentRunError> {
    let mut args = vec!["exec".to_owned(), "--skip-git-repo-check".to_owned()];
    if let Some(model) = &request.model {
        args.extend(["--model".to_owned(), model.clone()]);
    }
    args.extend([
        "--sandbox".to_owned(),
        match request.access {
            aifuel_core::AccessMode::ReadOnly => "read-only".to_owned(),
            aifuel_core::AccessMode::WorkspaceWrite => "workspace-write".to_owned(),
        },
    ]);
    if request.output != OutputFormat::Text {
        args.push("--json".to_owned());
    }
    if let Some(session) = &request.resume {
        args.extend(["resume".to_owned(), session.clone()]);
    }
    args.push(request.prompt.clone());
    Ok(args)
}
