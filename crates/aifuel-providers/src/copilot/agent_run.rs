use crate::agent_execution::{
    CliExecutionAdapter, ExecutionCapabilities, ParsedProviderOutput, parse_public_output,
};
use aifuel_core::{AgentRunError, AgentSetupGuidance, OutputFormat, ProviderKey, RunRequest};
use serde_json::Value;

pub(crate) static ADAPTER: CliExecutionAdapter = CliExecutionAdapter::new(
    ProviderKey::Copilot,
    "copilot",
    &["--help"],
    &["--prompt", "--plan", "--output-format"],
    build_args,
    parse_output,
    ExecutionCapabilities::new(true, false, false, false),
)
// Use the documented flag; the `copilot version` command checks for updates.
.with_version_probe(&["--version"])
.with_setup_guidance(AgentSetupGuidance {
    install: "npm install -g @github/copilot",
    login: "Start `copilot`, then enter `/login` in its interactive UI.",
    check: "Run `copilot --version` to check the installed version; AI Fuel does not inspect Copilot sign-in state.",
    documentation_url: "https://docs.github.com/en/copilot/get-started/cli-quickstart",
});

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
        if event_type.contains("error")
            || event_type.contains("failed")
            || event_type.contains("aborted")
        {
            parsed.terminal = Some(false);
            parsed.diagnostics =
                string_field(object, &["message", "error", "reason"]).or(parsed.diagnostics);
            continue;
        }
        if (event_type.contains("assistant")
            || event_type.contains("response")
            || event_type.contains("final"))
            && let Some(message) = public_text(object)
        {
            answers.push(message);
        }
        if matches!(
            event_type.as_str(),
            "session.idle"
                | "session.completed"
                | "session.complete"
                | "turn.completed"
                | "result"
                | "done"
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

fn public_text(object: &serde_json::Map<String, Value>) -> Option<String> {
    for value in [
        object.get("text"),
        object.get("content"),
        object.get("output"),
        object.get("response"),
        object.get("message"),
        object.get("data").and_then(|data| data.get("content")),
    ] {
        if let Some(text) = value.and_then(Value::as_str) {
            return Some(text.to_owned());
        }
    }
    None
}
