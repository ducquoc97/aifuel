//! Flatten an OpenAI `messages[]` conversation into the single prompt the
//! Agent Run contract carries. The convention:
//!
//! - A lone `user` message maps to its text verbatim - the common case for
//!   single-turn completions stays byte-exact.
//! - `system`/`developer` content leads the transcript unlabeled so it
//!   reads as instruction, followed by `User:`/`Assistant:`/`Tool:` blocks
//!   in order.
//! - Multipart `content` arrays contribute their `text` parts; non-text
//!   parts (images, audio, refusal markers) leave an `[<type> part omitted]`
//!   note so the omission is visible to the model rather than silent.
//! - `assistant.tool_calls` render as a `Tool calls:` JSON block, and
//!   `tool` messages carry their `tool_call_id`/`name`, so tool transcripts
//!   stay readable.
//! - Declared `tools` lead the transcript as an `Available tools:` preamble
//!   (`flatten_tools`) that instructs the model to phrase calls as a fenced
//!   `aifuel_tool_calls` JSON block - the prompt-level emulation that lets
//!   a prompt-only run contract answer OpenAI tool calling. `chat` scans
//!   the answer for that block and recovers `tool_calls` from it.

use serde_json::Value;

use super::types::{ChatMessage, ToolFunction};

/// The opening fence of the tool-call block the tools preamble instructs
/// the model to emit and `chat` scans answers for - one constant so both
/// sides of the convention can never drift apart.
pub(crate) const TOOL_CALLS_OPEN: &str = "```aifuel_tool_calls";

pub(crate) fn flatten_messages(messages: &[ChatMessage]) -> String {
    if let [message] = messages
        && message.role == "user"
    {
        return content_text(&message.content);
    }
    let mut blocks: Vec<String> = Vec::new();
    for message in messages {
        let text = content_text(&message.content);
        let block = match message.role.as_str() {
            "system" | "developer" => text,
            "assistant" => {
                let mut block = format!("Assistant: {text}");
                if let Some(tool_calls) = &message.tool_calls {
                    block.push_str(&format!(
                        "\nTool calls: {}",
                        serde_json::to_string(tool_calls)
                            .unwrap_or_else(|_| tool_calls.to_string())
                    ));
                }
                block
            }
            "tool" => format!(
                "Tool ({}): {text}",
                message
                    .tool_call_id
                    .as_deref()
                    .or(message.name.as_deref())
                    .unwrap_or("unknown")
            ),
            role => format!("{}: {text}", capitalize(role)),
        };
        if !block.trim().is_empty() {
            blocks.push(block);
        }
    }
    blocks.join("\n\n")
}

/// Render declared tools as the preamble that leads the transcript. Each
/// tool is listed with its name, description, and parameter JSON schema
/// verbatim, then the call contract: to call tools the model must answer
/// with EXACTLY one fenced `aifuel_tool_calls` block and nothing else.
///
/// `tool_choice` speaks the OpenAI vocabulary: `"none"` suppresses the
/// preamble entirely (the tools are not even declared), `"required"` or a
/// `{"function": {"name": "X"}}` object makes the call mandatory, and
/// `"auto"` or absent leaves it optional.
pub(crate) fn flatten_tools(tools: &[ToolFunction], tool_choice: Option<&Value>) -> String {
    if tools.is_empty() || tool_choice.and_then(Value::as_str) == Some("none") {
        return String::new();
    }
    let mut preamble = String::from("Available tools:\n");
    for tool in tools {
        preamble.push_str("\n- ");
        preamble.push_str(&tool.name);
        if let Some(description) = &tool.description {
            preamble.push_str(": ");
            preamble.push_str(description);
        }
        if let Some(parameters) = &tool.parameters {
            preamble.push_str("\n  Parameters: ");
            preamble.push_str(
                &serde_json::to_string(parameters).unwrap_or_else(|_| parameters.to_string()),
            );
        }
    }
    let required_name = tool_choice
        .and_then(|choice| choice.get("function"))
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str);
    let directive = match required_name {
        Some(name) => format!("Your reply must be a call to the tool `{name}`."),
        None if tool_choice.and_then(Value::as_str) == Some("required") => {
            "Your reply must be a tool call.".to_owned()
        }
        None => "If no tool is needed, reply normally.".to_owned(),
    };
    preamble.push_str(&format!(
        "\n\nTo call tools, reply with EXACTLY one fenced block of this shape and nothing else:\n\n{TOOL_CALLS_OPEN}\n{{\"calls\":[{{\"name\":\"<tool>\",\"arguments\":{{...}}}}]}}\n```\n\n{directive}"
    ));
    preamble
}

/// The text of one message `content` value: the string itself, or the
/// joined `text` parts of a multipart array with an omission note for
/// anything the run contract cannot carry.
fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text") | Some("input_text") => part
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                Some(kind) => format!("[{kind} part omitted]"),
                None => String::new(),
            })
            .collect::<Vec<_>>()
            .join(""),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn capitalize(role: &str) -> String {
    let mut chars = role.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The model's answer once the fenced `aifuel_tool_calls` block the tools
/// preamble demanded is recovered.
pub(crate) struct ParsedAnswer {
    /// Text outside the block; empty when the model followed the
    /// "nothing else" instruction.
    pub(crate) content: String,
    pub(crate) calls: Vec<ParsedCall>,
}

/// One recovered call. `arguments` stays a serialized JSON string - that
/// is the shape the wire puts in `function.arguments`/`function_call`.
pub(crate) struct ParsedCall {
    pub(crate) name: String,
    pub(crate) arguments: String,
}

/// Recover the fenced call block from a finished answer. `None` means no
/// well-formed block - the caller then treats the answer as plain
/// content, because a half-right block is indistinguishable from the
/// model merely quoting the format back. Shared by every surface that
/// emits tool calls (`chat`, `responses`).
pub(crate) fn parse_tool_calls(answer: &str) -> Option<ParsedAnswer> {
    let fence = answer.find(TOOL_CALLS_OPEN)?;
    // The payload starts on the line after the opening fence and ends at
    // the next fence-only line.
    let mut cursor = fence + TOOL_CALLS_OPEN.len();
    cursor += answer[cursor..].find('\n')? + 1;
    let payload_start = cursor;
    let mut close_start = None;
    for line in answer[payload_start..].split_inclusive('\n') {
        let trimmed = line.trim();
        if !trimmed.is_empty() && trimmed.chars().all(|c| c == '`') {
            close_start = Some(cursor);
            break;
        }
        cursor += line.len();
    }
    let close_start = close_start?;
    let payload: Value = serde_json::from_str(answer[payload_start..close_start].trim()).ok()?;
    let calls = payload.get("calls")?.as_array()?;
    let calls: Vec<ParsedCall> = calls
        .iter()
        .filter_map(|call| {
            let name = call.get("name")?.as_str()?.to_owned();
            let arguments = match call.get("arguments") {
                Some(Value::String(text)) => text.clone(),
                Some(value) => serde_json::to_string(value).ok()?,
                None => "{}".to_owned(),
            };
            Some(ParsedCall { name, arguments })
        })
        .collect();
    if calls.is_empty() {
        return None;
    }
    // Strip the whole block - the opening fence's line through the
    // closing fence's line - and keep whatever real text surrounded it.
    let block_start = answer[..fence].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let block_end = answer[close_start..]
        .find('\n')
        .map(|i| close_start + i + 1)
        .unwrap_or(answer.len());
    let content = format!("{}{}", &answer[..block_start], &answer[block_end..]);
    Some(ParsedAnswer {
        content: content.trim().to_owned(),
        calls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(role: &str, content: Value) -> ChatMessage {
        ChatMessage {
            role: role.to_owned(),
            content,
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    #[test]
    fn a_lone_user_message_passes_through_verbatim() {
        // The single-turn path is byte-exact: no transcript scaffolding
        // wraps the one prompt an app actually sent.
        let messages = vec![message("user", json!("Explain ownership"))];
        assert_eq!(flatten_messages(&messages), "Explain ownership");
    }

    #[test]
    fn system_leads_then_role_blocks() {
        let messages = vec![
            message("system", json!("Be terse.")),
            message("user", json!("Hi")),
            message("assistant", json!("Hello")),
            message("user", json!("Now what?")),
        ];
        assert_eq!(
            flatten_messages(&messages),
            "Be terse.\n\nUser: Hi\n\nAssistant: Hello\n\nUser: Now what?"
        );
    }

    #[test]
    fn multipart_content_joins_text_and_marks_omissions() {
        // Text parts concatenate (a multipart text message arrives split);
        // non-text parts leave a visible marker instead of vanishing.
        let messages = vec![
            message("system", json!("x")),
            message(
                "user",
                json!([
                    {"type": "text", "text": "What is "},
                    {"type": "image_url", "image_url": {"url": "data:..."}},
                    {"type": "text", "text": "in this picture?"}
                ]),
            ),
        ];
        assert_eq!(
            flatten_messages(&messages),
            "x\n\nUser: What is [image_url part omitted]in this picture?"
        );
    }

    #[test]
    fn tool_turns_keep_their_handles() {
        let messages = vec![
            message("system", json!("x")),
            ChatMessage {
                role: "assistant".to_owned(),
                content: json!("checking"),
                name: None,
                tool_calls: Some(json!([{"id":"call_1","function":{"name":"lookup"}}])),
                tool_call_id: None,
            },
            ChatMessage {
                role: "tool".to_owned(),
                content: json!("result text"),
                name: None,
                tool_calls: None,
                tool_call_id: Some("call_1".to_owned()),
            },
        ];
        let flat = flatten_messages(&messages);
        assert!(flat.contains("Tool calls:") && flat.contains("lookup"));
        assert!(flat.contains("Tool (call_1): result text"));
    }

    fn tool(name: &str) -> ToolFunction {
        ToolFunction {
            name: name.to_owned(),
            description: Some(format!("the {name} tool")),
            parameters: Some(json!({"type": "object", "properties": {"q": {"type": "string"}}})),
        }
    }

    #[test]
    fn tools_preamble_declares_tools_and_the_call_contract() {
        // The model can only honor tool calling it can see: the preamble
        // must carry each declaration (schema verbatim) plus the fenced
        // block contract `chat` later parses back.
        let preamble = flatten_tools(&[tool("lookup")], None);
        assert!(preamble.contains("- lookup: the lookup tool"));
        // serde_json's canonical key order, not the literal source order.
        assert!(
            preamble
                .contains(r#"Parameters: {"properties":{"q":{"type":"string"}},"type":"object"}"#)
        );
        assert!(preamble.contains(TOOL_CALLS_OPEN));
        assert!(preamble.contains(r#"{"calls":[{"name":"<tool>","arguments":{...}}]}"#));
    }

    #[test]
    fn tool_choice_none_suppresses_the_preamble() {
        // "none" means the caller does not want calls, so the tools must
        // not even be declared - a visible declaration invites the model
        // to use it.
        let preamble = flatten_tools(&[tool("lookup")], Some(&json!("none")));
        assert!(preamble.is_empty());
    }

    #[test]
    fn tool_choice_directives_force_a_call() {
        // "required" and a named function choice must read as mandatory,
        // not advisory - Roo/Cline treat an ignored required choice as a
        // broken gateway.
        let preamble = flatten_tools(&[tool("lookup")], Some(&json!("required")));
        assert!(preamble.contains("must be a tool call"));

        let preamble = flatten_tools(
            &[tool("lookup")],
            Some(&json!({"type": "function", "function": {"name": "lookup"}})),
        );
        assert!(preamble.contains("must be a call to the tool `lookup`"));

        let preamble = flatten_tools(&[tool("lookup")], Some(&json!("auto")));
        assert!(preamble.contains("If no tool is needed"));
    }

    #[test]
    fn no_tools_means_no_preamble() {
        assert!(flatten_tools(&[], None).is_empty());
        assert!(flatten_tools(&[], Some(&json!("required"))).is_empty());
    }
}
