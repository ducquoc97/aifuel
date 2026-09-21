use crate::agent_execution::{CliExecutionAdapter, ExecutionCapabilities};
use aifuel_core::{AgentRunError, OutputFormat, ProviderKey, RunRequest};

pub(crate) static ADAPTER: CliExecutionAdapter = CliExecutionAdapter::new(
    ProviderKey::Antigravity,
    "agy",
    &["--help"],
    &["--print", "--output-format"],
    build_args,
    ExecutionCapabilities::new(true, false, true, true),
);

fn build_args(request: &RunRequest) -> Result<Vec<String>, AgentRunError> {
    let mut args = vec![
        "--print".to_owned(),
        request.prompt.clone(),
        "--output-format".to_owned(),
        output_format(request.output).to_owned(),
    ];
    if let Some(model) = &request.model {
        args.extend(["--model".to_owned(), model.clone()]);
    }
    match request.access {
        aifuel_core::AccessMode::ReadOnly => args.push("--sandbox".to_owned()),
        aifuel_core::AccessMode::WorkspaceWrite => {
            args.extend(["--mode".to_owned(), "accept-edits".to_owned()]);
        }
    }
    if let Some(session) = &request.resume {
        args.extend(["--conversation".to_owned(), session.clone()]);
    }
    Ok(args)
}

fn output_format(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Text => "text",
        OutputFormat::Json => "json",
        OutputFormat::Jsonl => "stream-json",
    }
}
