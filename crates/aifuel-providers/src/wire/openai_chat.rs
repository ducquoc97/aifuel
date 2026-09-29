//! The `openai_chat` Wire Api: request shaping and stream-payload verdicts
//! for OpenAI-compatible chat completions endpoints.

use crate::ResolvedAuth;
use aifuel_core::{AgentRunError, EndpointConfig, KeyDelivery, TokenUsage};
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};
use serde_json::{Value, json};
use std::str::FromStr;

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
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(concat!("aifuel/", env!("CARGO_PKG_VERSION"))),
    );
    for (name, value) in &endpoint.extra_headers {
        let name = HeaderName::from_str(name).map_err(|_| {
            AgentRunError::InvalidRequest(format!(
                "endpoint header {name:?} is not a valid header name"
            ))
        })?;
        let value = HeaderValue::from_str(value).map_err(|_| {
            AgentRunError::InvalidRequest(format!(
                "endpoint header {name:?} is not a valid header value"
            ))
        })?;
        headers.insert(name, value);
    }
    match auth {
        ResolvedAuth::None => {}
        ResolvedAuth::ApiKey { key, delivery } => match delivery {
            KeyDelivery::Bearer => {
                insert_sensitive(&mut headers, AUTHORIZATION, &format!("Bearer {key}"))?
            }
            KeyDelivery::Header { name } => {
                let name = HeaderName::from_str(name).map_err(|_| {
                    AgentRunError::InvalidRequest(format!(
                        "credential delivery header {name:?} is not a valid header name"
                    ))
                })?;
                insert_sensitive(&mut headers, name, key)?;
            }
        },
        ResolvedAuth::OAuth { access_token, .. } => insert_sensitive(
            &mut headers,
            AUTHORIZATION,
            &format!("Bearer {access_token}"),
        )?,
    }
    Ok(headers)
}

/// Insert credential material as a sensitive header so reqwest strips it on
/// any redirect and never prints it in header `Debug` output.
fn insert_sensitive(
    headers: &mut HeaderMap,
    name: HeaderName,
    value: &str,
) -> Result<(), AgentRunError> {
    let mut value = HeaderValue::from_str(value).map_err(|_| {
        AgentRunError::InvalidRequest("credential material is not a valid header value".to_owned())
    })?;
    value.set_sensitive(true);
    headers.insert(name, value);
    Ok(())
}

/// What one `data:` payload means for the run.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DataVerdict {
    /// Public answer text; `terminal` marks a chunk that also finished the
    /// turn (a `finish_reason` on a chunk that still carried content). A
    /// terminal chunk can still carry inline `usage` accounting.
    Delta {
        text: String,
        terminal: bool,
        model: Option<String>,
        usage: Option<TokenUsage>,
    },
    /// A `finish_reason` chunk with no content: the turn is done, but an
    /// `include_usage` endpoint still sends a usage-only chunk next, so the
    /// stream keeps reading until `[DONE]` or EOF.
    Finish {
        model: Option<String>,
        usage: Option<TokenUsage>,
    },
    /// The `[DONE]` terminator: nothing follows it.
    Complete {
        model: Option<String>,
        usage: Option<TokenUsage>,
    },
    /// The provider reported a failure mid-stream.
    Failed { message: String },
    /// The provider reported token accounting for the run.
    Usage {
        usage: TokenUsage,
        model: Option<String>,
    },
    /// A payload carrying no answer content: role priming or a keepalive
    /// body.
    Ignored { model: Option<String> },
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
