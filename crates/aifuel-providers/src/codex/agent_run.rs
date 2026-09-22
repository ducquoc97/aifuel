use crate::agent_execution::{
    CliExecutionAdapter, ExecutionCapabilities, ParsedProviderOutput, parse_public_output,
};
use aifuel_core::{AgentRunError, OutputFormat, ProviderKey, RunRequest};
use serde_json::Value;

pub(crate) static ADAPTER: CliExecutionAdapter = CliExecutionAdapter::new(
    ProviderKey::Codex,
    "codex",
    &["exec", "--help"],
    &["exec", "--sandbox"],
    build_args,
    parse_output,
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

fn parse_output(format: OutputFormat, text: &str) -> ParsedProviderOutput {
    if format == OutputFormat::Text {
        return ParsedProviderOutput {
            output: text.to_owned(),
            terminal: Some(true),
            ..ParsedProviderOutput::default()
        };
    }

    let mut parsed = ParsedProviderOutput::default();
    let mut answers = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        parsed.structured = true;
        let Some(object) = value.as_object() else {
            continue;
        };
        parsed.session_id = string_field(object, &["thread_id", "threadId"]).or(parsed.session_id);
        parsed.effective_model =
            string_field(object, &["model", "model_id", "modelId"]).or(parsed.effective_model);
        match object.get("type").and_then(Value::as_str) {
            Some("item.completed") => {
                let Some(item) = object.get("item").and_then(Value::as_object) else {
                    continue;
                };
                if item.get("type").and_then(Value::as_str) == Some("agent_message")
                    && let Some(message) = item.get("text").and_then(Value::as_str)
                {
                    answers.push(message.to_owned());
                }
                parsed.effective_model = string_field(item, &["model", "model_id", "modelId"])
                    .or(parsed.effective_model);
            }
            Some("item.agent_message.delta") => {
                if let Some(delta) = object
                    .get("delta")
                    .or_else(|| object.get("text"))
                    .and_then(Value::as_str)
                {
                    answers.push(delta.to_owned());
                }
            }
            Some("turn.completed") => parsed.terminal = Some(true),
            Some("turn.failed") | Some("turn.cancelled") => {
                parsed.terminal = Some(false);
                parsed.diagnostics = error_message(object);
            }
            Some("error") => {
                parsed.diagnostics = error_message(object).or(parsed.diagnostics);
            }
            _ => {}
        }
    }
    if !parsed.structured {
        return parse_public_output(format, text);
    }
    parsed.output = answers.join("\n");
    parsed
}

fn string_field(object: &serde_json::Map<String, Value>, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(Value::as_str).map(str::to_owned))
}

fn error_message(object: &serde_json::Map<String, Value>) -> Option<String> {
    object
        .get("error")
        .and_then(|error| {
            error
                .as_str()
                .or_else(|| error.get("message").and_then(Value::as_str))
        })
        .or_else(|| object.get("message").and_then(Value::as_str))
        .map(str::to_owned)
}
