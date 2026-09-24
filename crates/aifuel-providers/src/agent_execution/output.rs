//! Normalized extraction of public answer text from provider output.

use aifuel_core::OutputFormat;

/// Provider-local parsing keeps native wire formats out of the common run
/// contract. Parsers return only public answer text and metadata explicitly
/// reported by the provider. Reasoning and tool output are never copied into
/// the normalized answer.
#[derive(Debug, Default)]
pub(crate) struct ParsedProviderOutput {
    pub output: String,
    pub session_id: Option<String>,
    pub effective_model: Option<String>,
    pub diagnostics: Option<String>,
    pub structured: bool,
    pub terminal: Option<bool>,
}

pub(crate) type OutputParser = fn(OutputFormat, &str) -> ParsedProviderOutput;

/// Conservative parser shared by adapters whose current native output does
/// not expose a provider-specific schema through the compiled interface. It
/// extracts public text fields when present and otherwise preserves plain
/// output. Structured records are only considered terminal when a terminal
/// event is explicitly observed.
pub(crate) fn parse_public_output(format: OutputFormat, text: &str) -> ParsedProviderOutput {
    if format == OutputFormat::Text {
        return ParsedProviderOutput {
            output: text.to_owned(),
            terminal: Some(true),
            ..ParsedProviderOutput::default()
        };
    }

    let mut parsed = ParsedProviderOutput {
        structured: true,
        ..ParsedProviderOutput::default()
    };
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            parsed.output.push_str(line);
            parsed.output.push('\n');
            parsed.structured = false;
            continue;
        };
        if let Some(session) = value
            .get("session_id")
            .or_else(|| value.get("sessionId"))
            .and_then(serde_json::Value::as_str)
        {
            parsed.session_id = Some(session.to_owned());
        }
        if let Some(model) = value
            .get("model")
            .or_else(|| value.get("model_id"))
            .and_then(serde_json::Value::as_str)
        {
            parsed.effective_model = Some(model.to_owned());
        }
        let event_type = value.get("type").and_then(serde_json::Value::as_str);
        let public_text = value
            .get("text")
            .or_else(|| value.get("content"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| value.get("message").and_then(serde_json::Value::as_str));
        if let Some(public_text) = public_text {
            parsed.output.push_str(public_text);
        }
        if matches!(
            event_type,
            Some("completed" | "result" | "turn.completed" | "message_stop")
        ) {
            parsed.terminal = Some(true);
        } else if matches!(event_type, Some("error" | "failed" | "turn.failed")) {
            parsed.terminal = Some(false);
            parsed.diagnostics = public_text.map(str::to_owned);
        }
    }
    if parsed.terminal.is_none() && !parsed.output.is_empty() {
        parsed.terminal = Some(true);
    }
    parsed
}
