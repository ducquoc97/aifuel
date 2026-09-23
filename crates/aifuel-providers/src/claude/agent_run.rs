use crate::agent_execution::{
    CliExecutionAdapter, ExecutionCapabilities, ParsedProviderOutput, parse_public_output,
};
use aifuel_core::{AgentRunError, AgentSetupGuidance, OutputFormat, ProviderKey, RunRequest};
use serde_json::Value;

pub(crate) static ADAPTER: CliExecutionAdapter = CliExecutionAdapter::new(
    ProviderKey::Claude,
    "claude",
    &["--help"],
    &["--print", "--permission-mode", "--output-format"],
    build_args,
    parse_output,
    // `plan` and `acceptEdits` do not prove a workspace boundary. Keep
    // workspace-write blocked until native effect tests establish one.
    ExecutionCapabilities::new(true, false, false, true),
)
// The Anthropic CLI reference documents this non-interactive version flag.
.with_version_probe(&["--version"])
.with_authentication_probe(&["auth", "status"])
.with_setup_guidance(AgentSetupGuidance {
    install: "npm install -g @anthropic-ai/claude-code",
    login: "Run `claude` and complete the browser sign-in prompt. If `ANTHROPIC_API_KEY` is configured, approve it when prompted.",
    check: "Run `claude --version` to check the install; `claude doctor` gives read-only install and settings diagnostics. AI Fuel separately runs `claude auth status` with a bounded timeout and discards its output.",
    documentation_url: "https://code.claude.com/docs/en/getting-started",
});

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
    let mut result_answer = None;
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
        match object.get("type").and_then(Value::as_str) {
            Some("system") => {
                if let Some(init) = object.get("init").and_then(Value::as_object) {
                    parsed.session_id =
                        string_field(init, &["session_id", "sessionId"]).or(parsed.session_id);
                    parsed.effective_model =
                        string_field(init, &["model", "model_id"]).or(parsed.effective_model);
                }
            }
            Some("assistant") => {
                if let Some(content) = object
                    .get("message")
                    .and_then(|message| message.get("content"))
                    .and_then(Value::as_array)
                {
                    for block in content {
                        if block.get("type").and_then(Value::as_str) == Some("text")
                            && let Some(message) = block.get("text").and_then(Value::as_str)
                        {
                            answers.push(message.to_owned());
                        }
                    }
                }
            }
            Some("content_block_delta") => {
                if let Some(delta) = object
                    .get("delta")
                    .and_then(|delta| delta.get("text"))
                    .and_then(Value::as_str)
                {
                    answers.push(delta.to_owned());
                }
            }
            Some("result") => {
                parsed.terminal =
                    Some(object.get("is_error").and_then(Value::as_bool).map_or_else(
                        || object.get("subtype").and_then(Value::as_str) == Some("success"),
                        |is_error| !is_error,
                    ));
                result_answer = object
                    .get("result")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if parsed.terminal == Some(false) {
                    parsed.diagnostics = error_message(object);
                }
            }
            Some("error") => {
                parsed.terminal = Some(false);
                parsed.diagnostics = error_message(object);
            }
            _ => {}
        }
    }
    if !parsed.structured {
        return parse_public_output(format, text);
    }
    parsed.output = result_answer.unwrap_or_else(|| answers.join("\n"));
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
