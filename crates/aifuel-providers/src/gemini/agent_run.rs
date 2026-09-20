use crate::agent_execution::{CliExecutionAdapter, ExecutionCapabilities};
use aifuel_core::{AccessMode, AgentRunError, OutputFormat, ProviderKey, RunRequest};

pub(crate) static ADAPTER: CliExecutionAdapter = CliExecutionAdapter::new(
    ProviderKey::Gemini,
    "gemini",
    &["--help"],
    &["--prompt", "--approval-mode", "--output-format"],
    build_args,
    ExecutionCapabilities::new(true, false, true, true),
);

fn build_args(request: &RunRequest) -> Result<Vec<String>, AgentRunError> {
    let mut args = vec![
        "--prompt".to_owned(),
        request.prompt.clone(),
        "--skip-trust".to_owned(),
    ];
    if let Some(model) = &request.model {
        args.extend(["--model".to_owned(), model.clone()]);
    }
    args.extend([
        "--approval-mode".to_owned(),
        match request.access {
            AccessMode::ReadOnly => "plan".to_owned(),
            AccessMode::WorkspaceWrite => "auto_edit".to_owned(),
        },
        "--output-format".to_owned(),
        match request.output {
            OutputFormat::Text => "text",
            OutputFormat::Json => "json",
            OutputFormat::Jsonl => "stream-json",
        }
        .to_owned(),
    ]);
    if let Some(session) = &request.resume {
        args.extend(["--resume".to_owned(), session.clone()]);
    }
    Ok(args)
}
