use crate::agent_execution::{
    CliExecutionAdapter, ExecutionCapabilities, ParsedProviderOutput, parse_public_output,
};
use aifuel_core::{AgentRunError, AgentSetupGuidance, OutputFormat, ProviderKey, RunRequest};
use serde_json::Value;

pub(crate) static ADAPTER: CliExecutionAdapter = CliExecutionAdapter::new(
    ProviderKey::Antigravity,
    "agy",
    &["--help"],
    &["--print", "--output-format"],
    build_args,
    parse_output,
    // The native sandbox help only promises terminal restrictions, not a
    // bounded workspace write policy.
    ExecutionCapabilities::new(true, false, false, true),
)
.with_setup_guidance(AgentSetupGuidance {
    install: "macOS/Linux: `curl -fsSL https://antigravity.google/cli/install.sh | bash`; Windows PowerShell: `irm https://antigravity.google/cli/install.ps1 | iex`.",
    login: "Run `agy` interactively; follow first-launch sign-in, which may open a browser.",
    check: "No non-interactive version or authentication status command is documented. Start `agy` manually to check setup; AI Fuel does not launch it.",
    documentation_url: "https://antigravity.google/docs/cli/install",
});
// The official Antigravity CLI reference does not document a non-interactive
// version command, so native version inspection deliberately starts no process.

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
        parsed.session_id = string_field(object, &["session_id", "sessionId", "conversation_id"])
            .or(parsed.session_id);
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
        }
        if event_type.contains("completed") || event_type == "result" || event_type == "done" {
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

fn output_format(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Text => "text",
        OutputFormat::Json => "json",
        OutputFormat::Jsonl => "stream-json",
    }
}
