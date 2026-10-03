//! The `anthropic_messages` Wire Api: request shaping and stream-payload
//! verdicts for Anthropic-compatible messages endpoints.
//!
//! The endpoint base URL is the API origin (`https://api.anthropic.com`);
//! the versioned `/v1/messages` path is appended here. Authentication is
//! the `x-api-key` header (`KeyDelivery::Header`), and every request
//! declares the compiled `anthropic-version` the adapter was built against.

use super::http;
use super::stream::DataVerdict;
use crate::ResolvedAuth;
use aifuel_core::{AgentRunError, EndpointConfig, TokenUsage};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

/// The Messages API version every request declares. It is a transport
/// default, not managed material, so a configured endpoint header may pin
/// a different version for a compatible gateway.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The `max_tokens` every request carries. The Messages API requires the
/// field and `RunRequest` has no token budget, so the adapter declares a
/// bound: 8192 is the output ceiling of the Claude 3.5/3.7 generation and
/// every later model accepts more. A lower bound would silently truncate
/// long answers (`stop_reason: max_tokens`); a model capped below this
/// fails loudly at the endpoint instead.
const MAX_TOKENS: u64 = 8_192;

/// The request URL for an endpoint's base URL. The base URL is already
/// normalized by configuration; only redundant trailing slashes are
/// trimmed.
pub(crate) fn messages_url(base_url: &str) -> String {
    format!("{}/v1/messages", base_url.trim_end_matches('/'))
}

/// The streaming messages body for one run. A wire integration serves
/// prompt completion only: a single user message, no provider-side tools,
/// and an explicit stream request. `system` is present only when the
/// run's optimizer configures a caveman level - the terse-response
/// directive - and is absent otherwise, matching the OpenAI body which
/// sets none then either.
pub(crate) fn request_body(model: &str, prompt: &str, system: Option<&str>) -> Value {
    let mut body = json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "messages": [{"role": "user", "content": prompt}],
        "stream": true,
    });
    if let Some(system) = system {
        body["system"] = json!(system);
    }
    body
}

/// The request headers for one run: transport defaults and the declared
/// `anthropic-version` first, configured endpoint headers next, and the
/// managed Authentication Binding last so endpoint config cannot override
/// managed credential material on the wire.
pub(crate) fn request_headers(
    endpoint: &EndpointConfig,
    auth: &ResolvedAuth,
) -> Result<HeaderMap, AgentRunError> {
    let mut headers = http::transport_headers();
    headers.insert(
        HeaderName::from_static("anthropic-version"),
        HeaderValue::from_static(ANTHROPIC_VERSION),
    );
    http::apply_configured_headers(&mut headers, endpoint)?;
    http::apply_auth(&mut headers, auth)?;
    Ok(headers)
}

/// Classify one event payload. Anthropic messages frames every event with
/// an `event:` name equal to the payload's `type`; classification keys off
/// `type` and tolerates the SSE name being absent.
///
/// The documented stream is `message_start` (the Message skeleton carrying
/// the model id and `usage.input_tokens`), `content_block_*` events per
/// block (`content_block_delta` carries `delta.text` on `text_delta`),
/// one or more `message_delta` events (`delta.stop_reason` plus cumulative
/// `usage.output_tokens`), a `message_stop` terminator, and `ping`
/// keepalives. `event: error` or an `error` payload fails the run.
pub(crate) fn classify_event(event: Option<&str>, data: &str) -> DataVerdict {
    let trimmed = data.trim();
    if event == Some("error") {
        return DataVerdict::Failed {
            message: error_message(trimmed),
        };
    }
    if trimmed.is_empty() {
        return DataVerdict::Ignored { model: None };
    }
    let value: Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        // A `data:` payload that is not JSON is a broken stream; guessing
        // at it would corrupt the answer.
        Err(error) => {
            return DataVerdict::Failed {
                message: format!("unparseable stream payload: {error}"),
            };
        }
    };
    match value.get("type").and_then(Value::as_str) {
        // `message_start` carries the serving model and the input token
        // accounting; the output side arrives at `message_delta`.
        Some("message_start") => {
            let message = value.get("message").unwrap_or(&Value::Null);
            let model = message
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned);
            match usage(message.get("usage")) {
                Some(usage) => DataVerdict::Usage { usage, model },
                None => DataVerdict::Ignored { model },
            }
        }
        Some("content_block_delta") => {
            let delta = value.get("delta").unwrap_or(&Value::Null);
            let text = (delta.get("type").and_then(Value::as_str) == Some("text_delta"))
                .then(|| delta.get("text").and_then(Value::as_str))
                .flatten();
            match text {
                Some(text) if !text.is_empty() => DataVerdict::Delta {
                    text: text.to_owned(),
                    terminal: false,
                    model: None,
                    usage: None,
                },
                // `input_json_delta` for tool-use blocks, `thinking_delta`,
                // and empty text carry no answer; this adapter never
                // requests tools or thinking.
                _ => DataVerdict::Ignored { model: None },
            }
        }
        Some("message_delta") => {
            let usage = usage(value.get("usage"));
            let stopped = value
                .pointer("/delta/stop_reason")
                .is_some_and(|reason| !reason.is_null());
            match (stopped, usage) {
                (true, usage) => DataVerdict::Finish { model: None, usage },
                (false, Some(usage)) => DataVerdict::Usage { usage, model: None },
                (false, None) => DataVerdict::Ignored { model: None },
            }
        }
        Some("message_stop") => DataVerdict::Complete {
            model: None,
            usage: None,
        },
        // A compatible server that ignores `stream` can answer one framed
        // `message` payload; its text blocks are still recoverable.
        Some("message") => {
            let model = value
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let usage = usage(value.get("usage"));
            let mut text = String::new();
            for block in value
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if block.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(part) = block.get("text").and_then(Value::as_str)
                {
                    text.push_str(part);
                }
            }
            if text.is_empty() {
                DataVerdict::Finish { model, usage }
            } else {
                DataVerdict::Delta {
                    text,
                    terminal: true,
                    model,
                    usage,
                }
            }
        }
        _ => {
            if let Some(message) = provider_error(&value) {
                DataVerdict::Failed { message }
            } else {
                // `content_block_start`/`stop`, `ping`, and
                // forward-compatible event types carry no answer content.
                DataVerdict::Ignored { model: None }
            }
        }
    }
}

/// The token accounting of one `usage` object: Anthropic reports
/// `input_tokens`/`output_tokens` where OpenAI reports
/// `prompt_tokens`/`completion_tokens`.
fn usage(value: Option<&Value>) -> Option<TokenUsage> {
    let usage = value?;
    let usage = TokenUsage {
        input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
    };
    (usage.input_tokens.is_some() || usage.output_tokens.is_some()).then_some(usage)
}

/// The message of an Anthropic error payload:
/// `{"type": "error", "error": {"type": ..., "message": "..."}}`, or a
/// compatible endpoint's bare `{"error": ...}` envelope.
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
