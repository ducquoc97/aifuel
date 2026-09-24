use crate::agent_execution::{
    CliExecutionAdapter, ExecutionCapabilities, ParsedProviderOutput, parse_public_output,
};
use aifuel_core::{
    AccessMode, AgentRunError, AgentSetupGuidance, OutputFormat, ProviderKey, RunRequest,
};
use serde_json::Value;

pub(crate) static ADAPTER: CliExecutionAdapter = CliExecutionAdapter::new(
    ProviderKey::Gemini,
    "gemini",
    &["--help"],
    &["--prompt", "--approval-mode", "--output-format"],
    build_args,
    parse_output,
    // Native workspace enforcement is unverified for the current integration.
    ExecutionCapabilities::new(true, false, false, true),
)
// The Gemini CLI reference documents --version as printing and exiting.
.with_version_probe(&["--version"])
.with_setup_guidance(AgentSetupGuidance {
    install: "npm install -g @google/gemini-cli",
    login: "Start `gemini` and choose a documented sign-in method, such as Sign in with Google.",
    check: "Run `gemini --version` to check the install. Start `gemini` and complete the interactive auth selection to verify account access; AI Fuel does not inspect local credentials.",
    documentation_url: "https://geminicli.com/docs/get-started/",
});

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
        parsed.session_id =
            string_field(object, &["session_id", "sessionId"]).or(parsed.session_id);
        parsed.effective_model =
            string_field(object, &["model", "model_id", "modelId"]).or(parsed.effective_model);
        let event_type = object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if event_type.contains("error") || object.get("error").is_some() {
            parsed.terminal = Some(false);
            parsed.diagnostics = object
                .get("error")
                .and_then(|error| {
                    error
                        .as_str()
                        .or_else(|| error.get("message").and_then(Value::as_str))
                })
                .or_else(|| object.get("message").and_then(Value::as_str))
                .map(str::to_owned);
            continue;
        }
        if let Some(response) = object
            .get("response")
            .or_else(|| object.get("text"))
            .or_else(|| object.get("output"))
            .and_then(Value::as_str)
        {
            answers.push(response.to_owned());
            if event_type.is_empty() {
                parsed.terminal = Some(true);
            }
        } else if event_type.contains("message")
            && object.get("role").and_then(Value::as_str) == Some("assistant")
            && let Some(content) = object.get("content").and_then(Value::as_str)
        {
            answers.push(content.to_owned());
        }
        if matches!(
            event_type.as_str(),
            "result" | "response.completed" | "turn.completed" | "message.completed"
        ) || event_type.ends_with(".completed")
        {
            parsed.terminal = Some(true);
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
