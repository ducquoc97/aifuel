//! The OpenCode serve HTTP surface: routes, request bodies, and the
//! response shapes the driver maps onto contract events.
//!
//! Everything here reflects the documented `opencode serve` API (the
//! same surface the generated `opencode` SDK binds): `/event` SSE with
//! `{type, properties}` envelopes, `message.part.updated` carrying the
//! full `part` object plus an optional `delta`, `permission.updated`
//! asks answered at `/session/{id}/permissions/{permissionID}` with
//! `once`/`always`/`reject`, and prompt responses shaped
//! `{info: AssistantMessage, parts}`.

use aifuel_core::{
    AccessMode, AgentRuntimeError, ApprovalDecision, ApprovalKind, ApprovalOption, ApprovalRequest,
    Attachment, AttachmentKind, ReceiptCode, RunId, TodoItem, TodoItemStatus, TokenUsage,
    UserInput,
};
use serde_json::{Value, json};
use std::path::Path;

/// The event stream route. Every other route lives under `/session`.
pub(super) const EVENTS_PATH: &str = "/event";
/// The readiness probe route: any HTTP response proves the serve is
/// speaking HTTP, even a 400.
pub(super) const SESSIONS_PATH: &str = "/session";

// Bus event type spellings the driver maps.
pub(super) const EV_CONNECTED: &str = "server.connected";
pub(super) const EV_PART_UPDATED: &str = "message.part.updated";
pub(super) const EV_PERMISSION_UPDATED: &str = "permission.updated";
pub(super) const EV_PERMISSION_REPLIED: &str = "permission.replied";
pub(super) const EV_SESSION_IDLE: &str = "session.idle";
pub(super) const EV_SESSION_STATUS: &str = "session.status";
pub(super) const EV_SESSION_ERROR: &str = "session.error";
pub(super) const EV_TODO_UPDATED: &str = "todo.updated";

/// The provider's abort verdict, reported as `AssistantMessage.error`
/// when a run ends through `session.abort` or a provider-side
/// interruption.
pub(super) const ABORTED_ERROR_NAME: &str = "MessageAbortedError";

/// The largest tool output copied into a `tool.completed` event; the
/// event stays a bounded observation, not a file dump.
const MAX_TOOL_OUTPUT_CHARS: usize = 64 * 1024;

/// One route URL scoped to the session's project directory. The serve
/// routes every request through the `directory` query parameter, so a
/// multi-instance server cannot answer for the wrong workspace.
pub(super) fn endpoint(
    base: &reqwest::Url,
    path: &str,
    directory: &Path,
) -> Result<reqwest::Url, AgentRuntimeError> {
    let mut url = base.join(path).map_err(|error| {
        AgentRuntimeError::provider_error(format!("malformed opencode route {path}: {error}"))
    })?;
    url.query_pairs_mut()
        .append_pair("directory", &directory.to_string_lossy());
    Ok(url)
}

/// Parse a model selection into OpenCode's `{providerID, modelID}`.
/// OpenCode spells model references `provider/model`; an empty
/// selection means the provider default, and anything without the
/// separator is rejected rather than guessed.
pub(super) fn model_ref(model: &str) -> Result<Option<(String, String)>, AgentRuntimeError> {
    if model.is_empty() {
        return Ok(None);
    }
    model
        .split_once('/')
        .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
        .map(|(provider, model)| (provider.to_owned(), model.to_owned()))
        .map(Some)
        .ok_or_else(|| {
            AgentRuntimeError::new(
                ReceiptCode::InvalidSelection,
                "opencode models are spelled provider/model",
            )
        })
}

/// Translate host input into prompt parts: the run's text and one
/// `file` part per attachment. The serve resolves `file://` URLs
/// against the same filesystem, so paths pass through unencoded, the
/// same convention the OpenCode TUI uses.
pub(super) fn prompt_parts(input: &UserInput) -> Result<Vec<Value>, AgentRuntimeError> {
    let mut parts = Vec::new();
    if !input.text.trim().is_empty() {
        parts.push(json!({"type": "text", "text": input.text}));
    }
    for attachment in &input.attachments {
        parts.push(file_part(attachment));
    }
    if parts.is_empty() {
        return Err(invalid_state("the input carries no prompt parts"));
    }
    Ok(parts)
}

/// The `POST /session/{id}/message` body. `messageID` doubles as the
/// contract `RunId`, so every provider event for the user message and
/// the prompt response correlate to the run. Read-only sessions route
/// through OpenCode's `plan` agent, the provider's read-only persona.
pub(super) fn prompt_body(
    run_id: &RunId,
    model: Option<(String, String)>,
    access: AccessMode,
    parts: Vec<Value>,
) -> Value {
    let mut body = json!({
        "messageID": run_id.as_str(),
        "parts": parts,
    });
    if let Some((provider, model)) = model {
        body["model"] = json!({"providerID": provider, "modelID": model});
    }
    if access == AccessMode::ReadOnly {
        body["agent"] = json!("plan");
    }
    body
}

fn file_part(attachment: &Attachment) -> Value {
    let path = std::fs::canonicalize(&attachment.path).unwrap_or_else(|_| attachment.path.clone());
    let filename = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    json!({
        "type": "file",
        "mime": mime_for(attachment.kind, &path),
        "url": format!("file://{}", path.display()),
        "filename": filename,
    })
}

/// The MIME spelling a `file` part carries: image attachments get their
/// format from the extension (unknown images stay opaque binary), and
/// file attachments are plain text, matching how OpenCode reads them.
fn mime_for(kind: AttachmentKind, path: &Path) -> &'static str {
    if kind == AttachmentKind::File {
        return "text/plain";
    }
    match path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("bmp") => "image/bmp",
        _ => "application/octet-stream",
    }
}

/// The options a permission request offers, spelled in the provider's
/// own reply vocabulary so a decision maps one-to-one onto the wire.
/// A read-only session offers only `reject`: accepting would widen the
/// run beyond its declared access.
pub(super) fn permission_options(access: AccessMode) -> Vec<ApprovalOption> {
    if access == AccessMode::ReadOnly {
        return vec![ApprovalOption {
            id: "reject".to_owned(),
            label: "Reject".to_owned(),
        }];
    }
    ["once", "always", "reject"]
        .into_iter()
        .map(|id| ApprovalOption {
            id: id.to_owned(),
            label: match id {
                "once" => "Allow once".to_owned(),
                "always" => "Always allow".to_owned(),
                _ => "Reject".to_owned(),
            },
        })
        .collect()
}

/// The `approval.requested` payload for one `permission.updated`
/// ask.
pub(super) fn permission_request(
    properties: &Value,
    access: AccessMode,
) -> Option<(String, ApprovalRequest)> {
    let permission_id = properties["id"].as_str()?.to_owned();
    let title = properties["title"]
        .as_str()
        .or_else(|| properties["type"].as_str())
        .unwrap_or("permission request")
        .to_owned();
    let detail = permission_detail(properties, &title);
    let request = ApprovalRequest {
        kind: ApprovalKind::ToolPermission,
        title,
        detail,
        options: permission_options(access),
        requires_confirm: false,
        interaction_kind: None,
        questions: Vec::new(),
        // The SSE properties are the provider-native ask: keep them plus
        // the event type so a host can rebuild the permission payload.
        parameters: Some(properties.clone()),
        native_method: Some(EV_PERMISSION_UPDATED.to_owned()),
    };
    Some((permission_id, request))
}

/// Human-readable detail: the ask's title plus the patterns the
/// provider carries (`pattern` is one string or a list on some asks).
fn permission_detail(properties: &Value, title: &str) -> String {
    let mut lines = vec![title.to_owned()];
    if let Some(permission_type) = properties["type"].as_str() {
        lines.push(format!("permission: {permission_type}"));
    }
    match &properties["pattern"] {
        Value::String(pattern) => lines.push(format!("pattern: {pattern}")),
        Value::Array(patterns) => lines.extend(
            patterns
                .iter()
                .filter_map(Value::as_str)
                .map(|pattern| format!("pattern: {pattern}")),
        ),
        _ => {}
    }
    lines.join("\n")
}

/// Map a host decision onto the provider's reply spelling. Options are
/// enforced exactly as offered; free text is never a permission answer.
pub(super) fn permission_verdict(
    options: &[String],
    decision: &ApprovalDecision,
) -> Result<String, AgentRuntimeError> {
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
    Ok(id.clone())
}

/// The OpenCode session id inside a `Session` response object.
pub(super) fn session_id_of(body: &Value) -> Option<String> {
    body["id"].as_str().map(str::to_owned)
}

/// The terminal facts a prompt response's `info` reports: provider
/// error name and message, and token accounting.
pub(super) fn assistant_verdict(
    body: &Value,
) -> (Option<String>, Option<String>, Option<TokenUsage>) {
    let info = &body["info"];
    let error = &info["error"];
    let error_name = error["name"].as_str().map(str::to_owned);
    let error_message = error_message(error);
    (error_name, error_message, token_usage(&info["tokens"]))
}

/// A provider error's message: `data.message` where present, the
/// error's name otherwise.
pub(super) fn error_message(error: &Value) -> Option<String> {
    if error.is_null() {
        return None;
    }
    error["data"]["message"]
        .as_str()
        .or_else(|| error["name"].as_str())
        .map(str::to_owned)
}

/// Token accounting on a message or step-finish part, mapped into the
/// contract's two buckets. Reasoning tokens stay folded into the
/// provider's `reasoning` field and are not conflated into output.
pub(super) fn token_usage(tokens: &Value) -> Option<TokenUsage> {
    if tokens.is_null() {
        return None;
    }
    let input = tokens["input"].as_u64();
    let output = tokens["output"].as_u64();
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(TokenUsage {
        input_tokens: input,
        output_tokens: output,
    })
}

/// `todo.updated` payloads mapped into contract items. OpenCode's
/// `cancelled` todo state has no contract spelling; it folds into
/// `completed` because the item is finished from the list's point of
/// view.
pub(super) fn todo_items(properties: &Value) -> Vec<TodoItem> {
    properties["todos"]
        .as_array()
        .map(|todos| {
            todos
                .iter()
                .filter_map(|todo| {
                    Some(TodoItem {
                        content: todo["content"].as_str()?.to_owned(),
                        status: match todo["status"].as_str()? {
                            "in_progress" => TodoItemStatus::InProgress,
                            "completed" | "cancelled" => TodoItemStatus::Completed,
                            _ => TodoItemStatus::Pending,
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Truncate provider tool output to the event bound.
pub(super) fn bounded_output(output: &str) -> String {
    output.chars().take(MAX_TOOL_OUTPUT_CHARS).collect()
}

fn invalid_state(message: impl Into<String>) -> AgentRuntimeError {
    AgentRuntimeError::new(ReceiptCode::InvalidState, message)
}
