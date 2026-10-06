//! The OpenAI wire shapes the gateway accepts and emits. Fields beyond
//! these are tolerated and ignored - hosted apps send broad parameter sets
//! (`user`, `seed`, penalties, `extra_body`) and strict decoding would 422
//! requests a provider could have served.

use serde::Deserialize;
use serde_json::{Value, json};

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
    /// Requested model-specific effort - OpenAI's `reasoning_effort`
    /// spelling. The value applies to every attempt the model selector
    /// resolves to, unless the selector pins its own `@effort` suffix.
    #[serde(default)]
    pub(crate) reasoning_effort: Option<String>,
    /// Declared tools for the prompt-level tool-call emulation; see
    /// `flatten::flatten_tools`.
    #[serde(default)]
    pub(crate) tools: Option<Vec<Tool>>,
    #[serde(default)]
    pub(crate) tool_choice: Option<Value>,
    /// The pre-`tools` spelling of the same declaration and directive;
    /// `tool_functions`/`tool_choice` fold them into the modern shapes.
    #[serde(default)]
    pub(crate) functions: Option<Vec<ToolFunction>>,
    #[serde(default)]
    pub(crate) function_call: Option<Value>,
    /// Per-request upper bound the caller asked for, in seconds is not an
    /// OpenAI field - `timeout`/`max_tokens` and friends are accepted via
    /// serde defaults and ignored by design.
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) user: Option<String>,
}

impl ChatRequest {
    /// The declared callable functions, whichever spelling the client
    /// used: `tools[].function` or legacy `functions[]`. Non-function
    /// tool entries (provider builtins) carry no declaration the prompt
    /// can use and drop out here.
    pub(crate) fn tool_functions(&self) -> Vec<ToolFunction> {
        match (&self.tools, &self.functions) {
            (Some(tools), _) => tools
                .iter()
                .filter_map(|tool| tool.function.clone())
                .collect(),
            (None, Some(functions)) => functions.clone(),
            (None, None) => Vec::new(),
        }
    }

    /// The caller's tool-choice directive normalized to the
    /// `tool_choice` vocabulary: a legacy `function_call` object
    /// `{"name": "X"}` reads as `{"function": {"name": "X"}}`, and
    /// `"none"`/`"auto"` pass through as strings.
    pub(crate) fn tool_choice(&self) -> Option<Value> {
        if let Some(choice) = &self.tool_choice {
            return Some(choice.clone());
        }
        match self.function_call.as_ref()? {
            Value::Object(call) => Some(json!({
                "type": "function",
                "function": {"name": call.get("name").cloned().unwrap_or(Value::Null)},
            })),
            other => Some(other.clone()),
        }
    }
}

/// One `tools[]` entry. `function` stays optional so provider-builtin
/// tool types decode and drop out instead of rejecting the request.
#[derive(Debug, Deserialize)]
pub(crate) struct Tool {
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) r#type: String,
    #[serde(default)]
    pub(crate) function: Option<ToolFunction>,
}

/// A callable function declaration - also the shape of each legacy
/// `functions[]` entry, which is why it stands alone.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ToolFunction {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) parameters: Option<Value>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Roo/Cline-era clients still send the pre-`tools` spellings; both
    /// must land on the same functions and the same choice, or the
    /// emulation would silently ignore them.
    #[test]
    fn legacy_functions_and_function_call_map_to_the_modern_shapes() {
        let chat: ChatRequest = serde_json::from_str(
            r#"{
                "model": "auto",
                "messages": [{"role": "user", "content": "hi"}],
                "functions": [{"name": "lookup", "description": "d", "parameters": {"type": "object"}}],
                "function_call": {"name": "lookup"}
            }"#,
        )
        .expect("a legacy request decodes");
        let functions = chat.tool_functions();
        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].name, "lookup");
        assert_eq!(
            chat.tool_choice(),
            Some(json!({"type": "function", "function": {"name": "lookup"}}))
        );
    }

    #[test]
    fn modern_tools_win_over_legacy_spellings() {
        // When both arrive, the modern fields are authoritative - mixing
        // vocabularies mid-request would be a bug source, not a feature.
        let chat: ChatRequest = serde_json::from_str(
            r#"{
                "model": "auto",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"type": "function", "function": {"name": "modern"}}],
                "functions": [{"name": "legacy"}],
                "tool_choice": "required",
                "function_call": {"name": "legacy"}
            }"#,
        )
        .expect("a mixed request decodes");
        let functions = chat.tool_functions();
        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].name, "modern");
        assert_eq!(chat.tool_choice(), Some(json!("required")));
    }

    #[test]
    fn non_function_tool_entries_are_dropped_not_rejected() {
        // Provider-builtin tool entries (e.g. `{"type": "web_search"}`)
        // carry no `function` key; strict decoding would 400 a request a
        // provider could otherwise serve.
        let chat: ChatRequest = serde_json::from_str(
            r#"{
                "model": "auto",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"type": "web_search"}, {"type": "function", "function": {"name": "f"}}]
            }"#,
        )
        .expect("mixed tool entries decode");
        let functions = chat.tool_functions();
        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].name, "f");
    }
}
