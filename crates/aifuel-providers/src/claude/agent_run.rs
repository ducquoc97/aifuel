use crate::agent_execution::{CliExecutionAdapter, ExecutionCapabilities};
use aifuel_core::{AgentRunError, OutputFormat, ProviderKey, RunRequest};

pub(crate) static ADAPTER: CliExecutionAdapter = CliExecutionAdapter::new(
    ProviderKey::Claude,
    "claude",
    &["--help"],
    &["--print", "--permission-mode", "--output-format"],
    build_args,
    ExecutionCapabilities::new(true, false, true, true),
);

fn build_args(request: &RunRequest) -> Result<Vec<String>, AgentRunError> {
    let mut args = vec!["--print".to_owned(), request.prompt.clone()];
    append_model(&mut args, request);
    args.extend([
        "--permission-mode".to_owned(),
        match request.access {
            aifuel_core::AccessMode::ReadOnly => "plan".to_owned(),
            aifuel_core::AccessMode::WorkspaceWrite => "acceptEdits".to_owned(),
        },
        "--output-format".to_owned(),
        output_format(request.output).to_owned(),
    ]);
    append_resume(&mut args, request);
    Ok(args)
}

fn output_format(format: OutputFormat) -> &'static str {
    match format {
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
