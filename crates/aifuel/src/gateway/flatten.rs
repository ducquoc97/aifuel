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
//!   stay readable. Providers cannot call client-side tools back through
//!   this surface today.

use serde_json::Value;

use super::types::ChatMessage;

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
}
