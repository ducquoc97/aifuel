use crate::agent_execution::{CliExecutionAdapter, ExecutionCapabilities};
use aifuel_core::{AgentRunError, OutputFormat, ProviderKey, RunRequest};

pub(crate) static ADAPTER: CliExecutionAdapter = CliExecutionAdapter::new(
    ProviderKey::Copilot,
    "copilot",
    &["--help"],
    &["--prompt", "--plan", "--output-format"],
    build_args,
    ExecutionCapabilities::new(true, false, false, false),
);

fn build_args(request: &RunRequest) -> Result<Vec<String>, AgentRunError> {
    let mut args = vec![
        "--prompt".to_owned(),
        request.prompt.clone(),
        "--plan".to_owned(),
    ];
    if let Some(model) = &request.model {
        args.extend(["--model".to_owned(), model.clone()]);
    }
    args.extend([
        "--output-format".to_owned(),
        match request.output {
            OutputFormat::Text => "text",
            OutputFormat::Json => "json",
            OutputFormat::Jsonl => unreachable!("JSONL was rejected during preflight"),
        }
        .to_owned(),
    ]);
    if let Some(session) = &request.resume {
        args.extend(["--resume".to_owned(), session.clone()]);
    }
    Ok(args)
}
