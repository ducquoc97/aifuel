//! The OpenAI wire shapes the gateway accepts and emits. Fields beyond
//! these are tolerated and ignored - hosted apps send broad parameter sets
//! (`user`, `seed`, penalties, `extra_body`) and strict decoding would 422
//! requests a provider could have served.

use serde::Deserialize;
use serde_json::Value;

/// `POST /v1/chat/completions` request subset the gateway consumes.
#[derive(Debug, Deserialize)]
pub(crate) struct ChatRequest {
    /// The routing selector; see the module docs on `crate::gateway` for
    /// the addressing convention. Required by the OpenAI contract.
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) messages: Vec<ChatMessage>,
    #[serde(default)]
    pub(crate) stream: bool,
    #[serde(default)]
    pub(crate) stream_options: Option<StreamOptions>,
    /// Per-request upper bound the caller asked for, in seconds is not an
    /// OpenAI field - `timeout`/`max_tokens`/`tools` and friends are
    /// accepted via serde defaults and ignored by design.
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) user: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StreamOptions {
    pub(crate) include_usage: Option<bool>,
}

/// One inbound message. `content` stays a raw `Value`: a string, a
/// multipart array, or null on tool-call-only turns - `flatten` reads the
/// text out of each shape.
#[derive(Debug, Deserialize)]
pub(crate) struct ChatMessage {
    pub(crate) role: String,
    #[serde(default)]
    pub(crate) content: Value,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) tool_calls: Option<Value>,
    #[serde(default)]
    pub(crate) tool_call_id: Option<String>,
}
