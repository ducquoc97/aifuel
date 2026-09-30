//! The `claude` stream-json wire shapes and their contract mapping.
//!
//! Every stdio line is one JSON object. This module isolates each
//! provider spelling behind pure functions: outgoing lines are built
//! here, incoming lines are parsed to one typed [`Frame`], and the
//! session driver only works on typed facts.

use aifuel_core::{
    AccessMode, AgentRuntimeError, ApprovalDecision, ApprovalKind, ApprovalOption, ApprovalRequest,
    Effort, QuotaSummary, ReceiptCode, TokenUsage,
};
use serde_json::{Value, json};

/// The longest summary a tool event or approval title carries.
const SUMMARY_LIMIT: usize = 160;

/// The `claude` arguments for one bidirectional stream-json session.
///
/// `-p` keeps the process non-interactive while `--input-format
/// stream-json` reads user and control messages from stdin;
/// `--include-partial-messages` turns on `stream_event` deltas;
/// `--permission-prompt-tool stdio` routes permission questions to the
/// host as `control_request` frames instead of prompting a terminal.
pub(super) fn spawn_args(
    model: Option<&str>,
    effort: Option<Effort>,
    access: AccessMode,
    resume: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "-p".to_owned(),
        "--input-format".to_owned(),
        "stream-json".to_owned(),
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        "--verbose".to_owned(),
        "--include-partial-messages".to_owned(),
        "--permission-prompt-tool".to_owned(),
        "stdio".to_owned(),
        "--permission-mode".to_owned(),
        super::permission_mode(access).to_owned(),
    ];
    if let Some(model) = model.filter(|model| !model.is_empty()) {
        args.extend(["--model".to_owned(), model.to_owned()]);
    }
    if let Some(effort) = effort {
        args.extend(["--effort".to_owned(), effort.as_str().to_owned()]);
    }
    if let Some(resume) = resume {
        args.extend(["--resume".to_owned(), resume.to_owned()]);
    }
    args
}

/// The control handshake sent once at session start. It registers no
/// hooks or servers; it exists so the control channel is exercised
/// before the first turn needs it.
pub(super) fn initialize_request(request_id: &str) -> String {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": {"subtype": "initialize", "hooks": null},
    })
    .to_string()
}

/// The protocol-level interrupt that cancels the in-flight turn. The
/// CLI answers with a `control_response` and ends the turn with a
/// `result` carrying `terminal_reason` `aborted_*`.
pub(super) fn interrupt_request(request_id: &str) -> String {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": {"subtype": "interrupt"},
    })
    .to_string()
}

/// A live model switch for a `model.select` the facade already
/// resolved. `None` restores the provider default model.
pub(super) fn set_model_request(request_id: &str, model: Option<&str>) -> String {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": {"subtype": "set_model", "model": model},
    })
    .to_string()
}

/// One user turn: a `user` message carrying a single text block. The
/// provider session id echoes back so the CLI binds the turn to the
/// session it opened.
pub(super) fn user_message(session_id: Option<&str>, text: &str) -> String {
    json!({
        "type": "user",
        "session_id": session_id,
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": text}],
        },
        "parent_tool_use_id": null,
    })
    .to_string()
}

/// The host's answer to a `can_use_tool` control request. Allow echoes
/// the tool input unchanged; deny carries a human-readable message.
pub(super) fn permission_response(
    request_id: &str,
    allow: bool,
    input: &Value,
    message: &str,
) -> String {
    let response = if allow {
        json!({"behavior": "allow", "updatedInput": input})
    } else {
        json!({"behavior": "deny", "message": message})
    };
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": response,
        },
    })
    .to_string()
}

/// An error answer for control request subtypes this adapter does not
/// implement, so the CLI never waits a minute on an unsupported ask.
pub(super) fn error_response(request_id: &str, message: &str) -> String {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "error",
            "request_id": request_id,
            "error": message,
        },
    })
    .to_string()
}

/// How a host decision maps onto the wire for one `can_use_tool`
/// request.
pub(super) enum ToolAnswer {
    Allow,
    Deny { cancel: bool, message: String },
}

/// Validate a decision against the options the request offered and
/// map it to the wire answer. `Text` is never accepted on a permission
/// request, and an unoffered option id is rejected, matching the CLI
/// fallback's option policy.
pub(super) fn tool_answer(
    options: &[String],
    decision: &ApprovalDecision,
) -> Result<ToolAnswer, AgentRuntimeError> {
    let ApprovalDecision::OptionId(id) = decision else {
        return Err(invalid_state(
            "permission approvals take a declared option, not free text",
        ));
    };
    if !options.iter().any(|option| option == id) {
        return Err(invalid_state(format!(
            "{id:?} was not offered on this request"
        )));
    }
    Ok(match id.as_str() {
        "accept" => ToolAnswer::Allow,
        "decline" => ToolAnswer::Deny {
            cancel: false,
            message: "declined by the host".to_owned(),
        },
        // "cancel" denies the tool and interrupts the whole turn, the
        // same semantics the CLI fallback's Cancel carries.
        "cancel" => ToolAnswer::Deny {
            cancel: true,
            message: "cancelled by the host".to_owned(),
        },
        _ => unreachable!("options only contain accept, decline, or cancel"),
    })
}

/// The `approval.requested` payload for one `can_use_tool` request.
///
/// `accept` is offered only where the session's Access Mode already
/// permits workspace mutation; on `plan` mode a permission widening is
/// the provider's to decide, so only decline and cancel remain.
pub(super) fn approval_request(request: &Value, access: AccessMode) -> ApprovalRequest {
    let tool_name = request
        .get("tool_name")
        .or_else(|| request.get("display_name"))
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let input = request.get("input").cloned().unwrap_or(Value::Null);
    let kind = if tool_name == "ExitPlanMode" {
        ApprovalKind::PlanApproval
    } else {
        ApprovalKind::ToolPermission
    };
    let title = request
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| tool_summary(tool_name, &input));
    let mut detail = format!("{tool_name}: {}", compact_json(&input));
    if let Some(reason) = request.get("decision_reason").and_then(Value::as_str) {
        detail.push_str(&format!("\n{reason}"));
    }
    let mut options = Vec::new();
    if access != AccessMode::ReadOnly {
        options.push(ApprovalOption {
            id: "accept".to_owned(),
            label: "Accept".to_owned(),
        });
    }
    options.push(ApprovalOption {
        id: "decline".to_owned(),
        label: "Decline".to_owned(),
    });
    options.push(ApprovalOption {
        id: "cancel".to_owned(),
        label: "Cancel run".to_owned(),
    });
    ApprovalRequest {
        kind,
        title: truncate(&title),
        detail,
        options,
        requires_confirm: false,
    }
}

/// One parsed stdout frame, reduced to what the driver handles.
#[derive(Debug)]
pub(super) enum Frame {
    /// `system`/`init`: the provider session id and effective model.
    Init { session_id: Option<String> },
    /// `system`/`status`: a worker status such as `compacting`.
    Status(String),
    /// `assistant`: one completed API message's content blocks.
    Assistant { blocks: Vec<Block> },
    /// `user` frames carrying tool results; plain user text is the
    /// provider's own echo and is skipped.
    ToolResults(Vec<ToolResultBlock>),
    /// `stream_event` `content_block_delta` `text_delta`.
    TextDelta(String),
    /// `stream_event` `content_block_delta` `thinking_delta`.
    ThinkingDelta(String),
    /// `stream_event` `content_block_start` for `tool_use`: an early
    /// tool_use_id to tool-name binding for `tool.completed`.
    StreamToolStart { tool_use_id: String, name: String },
    /// A CLI-initiated control request (`can_use_tool` and friends).
    ControlRequest { request_id: String, request: Value },
    /// The CLI withdrew one of its own requests.
    ControlCancel { request_id: String },
    /// `result`: the terminal fact of the current turn.
    Result(ResultFrame),
    /// `rate_limit_event`: a live Quota Pool observation.
    Quota(QuotaSummary),
    /// A top-level `error` frame.
    Error(String),
    /// Frames the protocol carries that mean nothing to the contract:
    /// `control_response`, `keep_alive`, `tool_progress`, and the rest.
    Ignored,
}

/// One `assistant` content block.
#[derive(Debug)]
pub(super) enum Block {
    Text(String),
    Thinking(String),
    ToolUse {
        tool_use_id: String,
        name: String,
        input: Value,
    },
}

/// One `tool_result` block inside a `user` frame.
#[derive(Debug)]
pub(super) struct ToolResultBlock {
    pub tool_use_id: String,
    pub ok: bool,
    pub output: Option<String>,
}

/// The `result` frame's terminal fact.
#[derive(Debug)]
pub(super) struct ResultFrame {
    pub is_error: bool,
    pub terminal_reason: Option<String>,
    /// `result` text on error frames, or the last answer on success.
    pub message: Option<String>,
    pub usage: Option<TokenUsage>,
    pub session_id: Option<String>,
}

/// Parse one stdout JSON object into a [`Frame`]. Unrecognized objects
/// are `Ignored`, never a failure: the provider controls the stream and
/// adds frame kinds between versions.
pub(super) fn parse_frame(value: &Value) -> Frame {
    match value.get("type").and_then(Value::as_str) {
        Some("system") => match value.get("subtype").and_then(Value::as_str) {
            Some("init") => Frame::Init {
                session_id: non_empty(value.get("session_id")),
            },
            // Worker statuses the contract maps; housekeeping subtypes
            // such as `commands_changed` mean nothing to the run.
            Some(status @ ("compacting" | "requesting")) => Frame::Status(status.to_owned()),
            _ => Frame::Ignored,
        },
        Some("assistant") => Frame::Assistant {
            blocks: value
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(Value::as_array)
                .map(|content| content.iter().filter_map(parse_block).collect())
                .unwrap_or_default(),
        },
        Some("user") => Frame::ToolResults(tool_results(value)),
        Some("stream_event") => stream_event(value),
        Some("control_request") => Frame::ControlRequest {
            request_id: value
                .get("request_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            request: value.get("request").cloned().unwrap_or(Value::Null),
        },
        Some("control_cancel_request") => Frame::ControlCancel {
            request_id: value
                .get("request_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        },
        Some("result") => Frame::Result(result_frame(value)),
        Some("rate_limit_event") => value
            .get("rate_limit_info")
            .and_then(quota_summary)
            .map(Frame::Quota)
            .unwrap_or(Frame::Ignored),
        Some("error") => Frame::Error(
            value
                .get("error")
                .and_then(|error| {
                    error
                        .as_str()
                        .or_else(|| error.get("message").and_then(Value::as_str))
                })
                .or_else(|| value.get("message").and_then(Value::as_str))
                .unwrap_or("the provider reported an error")
                .to_owned(),
        ),
        _ => Frame::Ignored,
    }
}

fn parse_block(block: &Value) -> Option<Block> {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => block
            .get("text")
            .and_then(Value::as_str)
            .map(|text| Block::Text(text.to_owned())),
        Some("thinking") => block
            .get("thinking")
            .and_then(Value::as_str)
            .map(|thinking| Block::Thinking(thinking.to_owned())),
        Some("tool_use") => Some(Block::ToolUse {
            tool_use_id: block
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            name: block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_owned(),
            input: block.get("input").cloned().unwrap_or(Value::Null),
        }),
        _ => None,
    }
}

fn tool_results(value: &Value) -> Vec<ToolResultBlock> {
    let Some(content) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .map(|block| ToolResultBlock {
            tool_use_id: block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            ok: block.get("is_error").and_then(Value::as_bool) != Some(true),
            output: tool_output(block.get("content")),
        })
        .collect()
}

/// The output of one `tool_result`: a string, or the joined text of
/// its content blocks.
fn tool_output(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let text = blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn stream_event(value: &Value) -> Frame {
    let event = value.get("event").cloned().unwrap_or(Value::Null);
    match event.get("type").and_then(Value::as_str) {
        Some("content_block_delta") => match event
            .get("delta")
            .and_then(|delta| delta.get("type"))
            .and_then(Value::as_str)
        {
            Some("text_delta") => event
                .get("delta")
                .and_then(|delta| delta.get("text"))
                .and_then(Value::as_str)
                .map(|text| Frame::TextDelta(text.to_owned()))
                .unwrap_or(Frame::Ignored),
            Some("thinking_delta") => event
                .get("delta")
                .and_then(|delta| delta.get("thinking"))
                .and_then(Value::as_str)
                .map(|thinking| Frame::ThinkingDelta(thinking.to_owned()))
                .unwrap_or(Frame::Ignored),
            _ => Frame::Ignored,
        },
        Some("content_block_start") => {
            let block = event.get("content_block").cloned().unwrap_or(Value::Null);
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                Frame::StreamToolStart {
                    tool_use_id: block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    name: block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_owned(),
                }
            } else {
                Frame::Ignored
            }
        }
        _ => Frame::Ignored,
    }
}

fn result_frame(value: &Value) -> ResultFrame {
    let usage = value.get("usage").map(|usage| {
        let input = [
            "input_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ]
        .iter()
        .filter_map(|key| usage.get(*key).and_then(Value::as_u64))
        .sum();
        TokenUsage {
            input_tokens: Some(input),
            output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
        }
    });
    ResultFrame {
        is_error: value.get("is_error").and_then(Value::as_bool) == Some(true),
        terminal_reason: non_empty(value.get("terminal_reason")),
        message: value
            .get("result")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| non_empty(value.get("error"))),
        usage,
        session_id: non_empty(value.get("session_id")),
    }
}

/// A `result` reports a cancelled turn when the provider aborted it
/// (`aborted_streaming` and friends); a host-requested interrupt shows
/// the same reason.
pub(super) fn is_aborted(frame: &ResultFrame) -> bool {
    frame
        .terminal_reason
        .as_deref()
        .is_some_and(|reason| reason.starts_with("aborted"))
}

/// Map a `rate_limit_event` payload to a Quota Pool observation. The
/// most constrained unified window wins; a payload with no window and
/// no reset time carries no usable observation and reports `None`.
pub(super) fn quota_summary(info: &Value) -> Option<QuotaSummary> {
    let depleted = info.get("status").and_then(Value::as_str) == Some("rejected");
    if let Some(windows) = info.get("unifiedWindows").and_then(Value::as_object)
        && let Some(worst) = windows
            .values()
            .filter_map(|window| {
                let utilization = window.get("utilization").and_then(Value::as_f64)?;
                Some((utilization, window.get("resetsAt").and_then(Value::as_f64)))
            })
            .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
    {
        return Some(QuotaSummary {
            remaining_pct: Some(((1.0 - worst.0).max(0.0)) * 100.0),
            resets_at: worst.1,
            depleted: depleted || worst.0 >= 1.0,
        });
    }
    let resets_at = info.get("resetsAt").and_then(Value::as_f64);
    if resets_at.is_some() || depleted {
        return Some(QuotaSummary {
            remaining_pct: None,
            resets_at,
            depleted,
        });
    }
    None
}

/// A one-line summary of a tool call: the most identifying input field
/// or a bounded JSON rendering, used for `tool.started` summaries and
/// approval titles.
pub(super) fn tool_summary(name: &str, input: &Value) -> String {
    for key in [
        "command",
        "file_path",
        "notebook_path",
        "path",
        "pattern",
        "query",
        "url",
        "description",
        "prompt",
    ] {
        if let Some(value) = input.get(key).and_then(Value::as_str) {
            return truncate(&format!("{name}: {value}"));
        }
    }
    truncate(&format!("{name}: {}", compact_json(input)))
}

fn non_empty(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn compact_json(value: &Value) -> String {
    match serde_json::to_string(value) {
        Ok(text) => text,
        Err(_) => value.to_string(),
    }
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= SUMMARY_LIMIT {
        return text.to_owned();
    }
    let mut summary: String = text.chars().take(SUMMARY_LIMIT - 1).collect();
    summary.push('…');
    summary
}

fn invalid_state(message: impl Into<String>) -> AgentRuntimeError {
    AgentRuntimeError::new(ReceiptCode::InvalidState, message)
}
