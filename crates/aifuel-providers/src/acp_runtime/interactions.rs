//! ACP payload construction: prompt content blocks, the permission
//! option mapping onto the shared approval policy, and the session
//! `configOptions` model-selector parsing.

use super::session::{invalid_state, unsupported};
use crate::local_adapter::approvals;
use aifuel_core::{
    AccessMode, AgentRuntimeError, ApprovalDecision, ApprovalKind, ApprovalRequest, AttachmentKind,
    UserInput,
};
use serde_json::{Value, json};
use std::path::Path;

/// The largest attachment the adapter will read into a content block.
const MAX_ATTACHMENT_BYTES: u64 = crate::agent_execution::MAX_CAPTURE_BYTES as u64;

/// The `session/prompt` content blocks for one user message. Text maps
/// to `text` blocks, image attachments to `image` blocks (base64 data
/// plus MIME type, the only image carrier ACP defines), and file
/// attachments to `resource_link` blocks, which every ACP agent must
/// support. When the agent did not advertise the `image` prompt
/// capability, an image degrades to its `resource_link` form rather
/// than being silently dropped.
pub(super) fn content_blocks(
    input: &UserInput,
    image_supported: bool,
) -> Result<Vec<Value>, AgentRuntimeError> {
    let mut blocks = Vec::new();
    if !input.text.trim().is_empty() {
        blocks.push(json!({"type": "text", "text": input.text}));
    }
    for attachment in &input.attachments {
        let path = attachment.path.canonicalize().map_err(|_| {
            invalid_state(format!(
                "the attachment path {} does not exist",
                attachment.path.display()
            ))
        })?;
        match attachment.kind {
            AttachmentKind::Image if image_supported => {
                blocks.push(image_block(&path)?);
            }
            AttachmentKind::Image | AttachmentKind::File => {
                blocks.push(resource_link_block(&path)?);
            }
        }
    }
    if blocks.is_empty() {
        return Err(invalid_state("input text must not be empty"));
    }
    Ok(blocks)
}

/// An `image` content block: the file's bytes base64-encoded with its
/// MIME type, plus a `file://` URI so the agent can correlate the block
/// with the workspace path.
fn image_block(path: &Path) -> Result<Value, AgentRuntimeError> {
    let mime = image_mime(path).ok_or_else(|| {
        unsupported(format!(
            "the image attachment {} has no determinable media type",
            path.display()
        ))
    })?;
    let bytes = std::fs::read(path).map_err(|error| {
        AgentRuntimeError::provider_error(format!(
            "the image attachment {} could not be read: {error}",
            path.display()
        ))
    })?;
    if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(unsupported(
            "the image attachment exceeds the protocol frame limit",
        ));
    }
    Ok(json!({
        "type": "image",
        "data": base64_encode(&bytes),
        "mimeType": mime,
        "uri": file_uri(path),
    }))
}

/// A `resource_link` content block pointing at the local file. ACP
/// requires every agent to accept this block type, so it is the honest
/// carrier for file attachments and the fallback for images when the
/// agent advertises no image prompt capability.
fn resource_link_block(path: &Path) -> Result<Value, AgentRuntimeError> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "attachment".to_owned());
    let size = std::fs::metadata(path).map(|meta| meta.len()).ok();
    Ok(json!({
        "type": "resource_link",
        "uri": file_uri(path),
        "name": name,
        "mimeType": image_mime(path),
        "size": size,
    }))
}

/// The media type an image extension implies, or `None` when the path
/// does not name a known image format.
fn image_mime(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        Some("bmp") => Some("image/bmp"),
        Some("svg") => Some("image/svg+xml"),
        _ => None,
    }
}

fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// Standard-alphabet base64 for the `image` block's `data` field. Kept
/// local rather than pulling a dependency: the encoding is a fixed
/// deterministic transform with no options.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut group = [0u8; 3];
        group[..chunk.len()].copy_from_slice(chunk);
        let value = u32::from(group[0]) << 16 | u32::from(group[1]) << 8 | u32::from(group[2]);
        let indexes = [
            (value >> 18) & 0x3f,
            (value >> 12) & 0x3f,
            (value >> 6) & 0x3f,
            value & 0x3f,
        ];
        for (index, position) in indexes.iter().enumerate() {
            if index < chunk.len() + 1 {
                encoded.push(ALPHABET[*position as usize] as char);
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

/// The model selector a session's `configOptions` advertise: the
/// select-typed option carrying `category: "model"` (or the `model`
/// id, the spelling `cursor-agent acp` uses) and its allowed values.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ModelOption {
    pub id: String,
    pub values: Vec<String>,
    pub current: String,
}

/// Extract the model selector from a `session/new`, `session/load`, or
/// `config_option_update` `configOptions` array.
pub(super) fn model_option(config_options: &Value) -> Option<ModelOption> {
    config_options
        .as_array()?
        .iter()
        .filter(|option| option["type"].as_str() == Some("select"))
        .find(|option| {
            option["category"].as_str() == Some("model") || option["id"].as_str() == Some("model")
        })
        .and_then(|option| {
            let id = option["id"].as_str()?.to_owned();
            let current = option["currentValue"].as_str()?.to_owned();
            let mut values = Vec::new();
            if let Some(list) = option["options"].as_array() {
                collect_option_values(list, &mut values);
            }
            Some(ModelOption {
                id,
                values,
                current,
            })
        })
}

/// `options` may be a flat list of `{value}` entries or a list of
/// groups each carrying an `options` list.
fn collect_option_values(list: &[Value], values: &mut Vec<String>) {
    for option in list {
        if let Some(value) = option["value"].as_str() {
            values.push(value.to_owned());
        } else if let Some(grouped) = option["options"].as_array() {
            collect_option_values(grouped, values);
        }
    }
}

/// One `session/request_permission` frame decomposed into the option
/// vocabulary the contract needs plus the `approval.requested` payload
/// to emit. An ask carrying no usable options still surfaces: the host
/// sees `decline`/`cancel`, and either answers `cancelled` on the wire.
pub(super) fn permission_request(
    message: &Value,
    access: AccessMode,
) -> (PendingOptions, ApprovalRequest) {
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let tool_call = params.get("toolCall").cloned().unwrap_or(Value::Null);
    let options = parse_permission_options(params.get("options").unwrap_or(&Value::Null));
    let request = approval_request(&tool_call, &options, access);
    (options, request)
}

/// The `approval.requested` payload for one `session/request_permission`.
///
/// The shared approval policy decides the offered contract options:
/// `accept` appears only when the run's access is not read-only and the
/// agent offered a concrete `allow_once` option - an `allow_always`-only
/// ask would widen access beyond this run, so `requires_expanded_access`
/// suppresses `accept` for it.
pub(super) fn approval_request(
    tool_call: &Value,
    options: &PendingOptions,
    access: AccessMode,
) -> ApprovalRequest {
    let title = tool_call["title"]
        .as_str()
        .or_else(|| tool_call["name"].as_str())
        .unwrap_or("the agent requests permission")
        .to_owned();
    let mut detail = title.clone();
    if let Some(kind) = tool_call["kind"].as_str() {
        detail.push_str(&format!("\nkind: {kind}"));
    }
    let locations = tool_call["locations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|location| location["path"].as_str())
        .collect::<Vec<_>>();
    if !locations.is_empty() {
        detail.push_str(&format!("\npaths: {}", locations.join(", ")));
    }
    ApprovalRequest {
        kind: ApprovalKind::ToolPermission,
        title,
        detail,
        options: approvals::permission_options(access, options.accept_option.is_none()),
        requires_confirm: false,
    }
}

/// Map a host decision onto the `RequestPermissionOutcome` for one
/// parked request. Contract `accept` selects the agent's `allow_once`
/// option, `decline` selects its reject option (`reject_once` first,
/// else `reject_always`), and `cancel` - or a decision the agent has no
/// concrete option for - returns the `cancelled` outcome.
pub(super) fn permission_outcome(
    options: &PendingOptions,
    decision: &ApprovalDecision,
) -> Result<Value, AgentRuntimeError> {
    let ApprovalDecision::OptionId(id) = decision else {
        return Err(invalid_state(
            "permission approvals take a declared option, not free text",
        ));
    };
    let selected = match id.as_str() {
        "accept" => options.accept_option.as_deref(),
        "decline" => options.reject_option.as_deref(),
        _ => None,
    };
    match selected {
        Some(option_id) => Ok(json!({
            "outcome": {"outcome": "selected", "optionId": option_id}
        })),
        None => Ok(json!({
            "outcome": {"outcome": "cancelled"}
        })),
    }
}

/// The option ids a `session/request_permission` offered, decomposed
/// into the contract's accept/decline vocabulary. `accept` maps only to
/// a genuine `allow_once` option: `allow_always` alone would widen
/// authorization beyond this run, so it is stored for reference but
/// never answers `accept`. `decline` prefers `reject_once` and falls
/// back to `reject_always`.
pub(super) fn parse_permission_options(options: &Value) -> PendingOptions {
    let mut pending = PendingOptions::default();
    for option in options.as_array().into_iter().flatten() {
        let Some(option_id) = option["optionId"].as_str() else {
            continue;
        };
        match option["kind"].as_str() {
            Some("allow_once") => pending
                .accept_option
                .get_or_insert_with(|| option_id.to_owned()),
            Some("reject_once") => pending
                .reject_option
                .get_or_insert_with(|| option_id.to_owned()),
            Some("reject_always") => pending
                .reject_option
                .get_or_insert_with(|| option_id.to_owned()),
            _ => continue,
        };
    }
    pending
}

/// The ACP option ids one parked permission request can answer with.
#[derive(Debug, Default)]
pub(super) struct PendingOptions {
    /// The `allow_once` option id, when the agent offered one.
    pub accept_option: Option<String>,
    /// The `reject_once`/`reject_always` option id, when offered.
    pub reject_option: Option<String>,
}

/// A non-empty model value that the advertised selector does not list
/// is an invalid selection, never a silent default. An empty values
/// list means the selector advertised no closed set.
pub(super) fn selectable_model(option: &ModelOption, model: &str) -> Result<(), AgentRuntimeError> {
    if option.values.is_empty() || option.values.iter().any(|value| value == model) {
        Ok(())
    } else {
        Err(AgentRuntimeError::new(
            aifuel_core::ReceiptCode::InvalidSelection,
            format!("{model:?} is not a selectable option for {}", option.id),
        ))
    }
}
