//! The `openai_chat` Wire Api: request shaping and stream-payload verdicts
//! for OpenAI-compatible chat completions endpoints.

use super::http;
use super::stream::DataVerdict;
use crate::ResolvedAuth;
use aifuel_core::{AgentRunError, EndpointConfig, TokenUsage};
use reqwest::header::HeaderMap;
use serde_json::{Value, json};

/// The data payload that terminates a chat-completions stream.
const DONE_PAYLOAD: &str = "[DONE]";

/// The request URL for an endpoint's base URL. The base URL is already
/// normalized by configuration; only redundant trailing slashes are
/// trimmed.
pub(crate) fn completions_url(base_url: &str) -> String {
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}

/// The streaming chat-completions body for one run. A wire integration
/// serves prompt completion only: a single user message, no provider-side
/// tools, and an explicit stream request.
pub(crate) fn request_body(model: &str, prompt: &str) -> Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": true,
        // Ask for the usage chunk so token accounting lands on RunResult;
        // compatible endpoints that ignore it simply send no usage chunk.
        "stream_options": {"include_usage": true},
    })
}

/// The request headers for one run: transport defaults first, configured
/// endpoint headers next, and the managed Authentication Binding last so
/// endpoint config cannot override managed credential material on the wire.
pub(crate) fn request_headers(
    endpoint: &EndpointConfig,
    auth: &ResolvedAuth,
) -> Result<HeaderMap, AgentRunError> {
    let mut headers = http::transport_headers();
    http::apply_configured_headers(&mut headers, endpoint)?;
    http::apply_auth(&mut headers, auth)?;
    Ok(headers)
}

/// Classify one event payload. `event` is the SSE `event:` field when the
/// peer set one: some compatible servers mark failures with `event: error`.
/// Payloads are otherwise JSON completion chunks or the `[DONE]`
/// terminator.
pub(crate) fn classify_event(event: Option<&str>, data: &str) -> DataVerdict {
    let trimmed = data.trim();
    if event == Some("error") {
        return DataVerdict::Failed {
            message: error_message(trimmed),
        };
    }
    if trimmed == DONE_PAYLOAD {
        return DataVerdict::Complete {
            model: None,
            usage: None,
        };
    }
    if trimmed.is_empty() {
        return DataVerdict::Ignored { model: None };
    }
    let value: Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        // A `data:` payload that is neither the terminator nor JSON is a
        // broken stream; guessing at it would corrupt the answer.
        Err(error) => {
            return DataVerdict::Failed {
                message: format!("unparseable stream payload: {error}"),
            };
        }
    };
    if let Some(message) = provider_error(&value) {
        return DataVerdict::Failed { message };
    }
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let usage = value.get("usage").and_then(|usage| {
        let usage = TokenUsage {
            input_tokens: usage.get("prompt_tokens").and_then(Value::as_u64),
            output_tokens: usage.get("completion_tokens").and_then(Value::as_u64),
        };
        (usage.input_tokens.is_some() || usage.output_tokens.is_some()).then_some(usage)
    });
    let mut text = String::new();
    let mut terminal = false;
    for choice in value
        .get("choices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if choice
            .get("finish_reason")
            .is_some_and(|reason| !reason.is_null())
        {
            terminal = true;
        }
        // `delta` is the streaming shape; `message` is tolerated so a
        // server that answered non-streamed JSON still yields its text.
        if let Some(content) = choice
            .get("delta")
            .or_else(|| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
        {
            text.push_str(content);
        }
    }
    if !text.is_empty() {
        DataVerdict::Delta {
            text,
            terminal,
            model,
            usage,
        }
    } else if terminal {
        DataVerdict::Finish { model, usage }
    } else if let Some(usage) = usage {
        DataVerdict::Usage { usage, model }
    } else {
        DataVerdict::Ignored { model }
    }
}

/// The message of a provider error object: `{"error": "..."}` or
/// `{"error": {"message": "...", ...}}`.
fn provider_error(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    match error {
        Value::String(message) => Some(message.clone()),
        Value::Object(_) => Some(
            error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| error.to_string()),
        ),
        _ => None,
    }
}

/// The failure detail carried by an `event: error` payload: the JSON error
/// message when the payload is one, else the raw payload text.
fn error_message(data: &str) -> String {
    serde_json::from_str::<Value>(data)
        .ok()
        .and_then(|value| provider_error(&value))
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| {
            if data.is_empty() {
                "the endpoint reported an error event".to_owned()
            } else {
                data.to_owned()
            }
        })
}
