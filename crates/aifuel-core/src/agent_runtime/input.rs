//! Host-supplied inputs: run text and attachments, approval options,
//! decisions, and the Approval Request payload.

use crate::{AgentInputQuestion, AgentInteractionKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// One unit of user input sent to an Agent Session as one Agent Run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInput {
    pub text: String,
    /// Host-resolved local paths. The runtime never fetches URLs on a host's
    /// behalf.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

/// A host-resolved local file attached to a [`UserInput`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub kind: AttachmentKind,
    pub path: PathBuf,
}

/// The attachment kinds the contract can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Image,
    File,
}

impl AttachmentKind {
    /// The stable serialized spelling for this attachment kind.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::File => "file",
        }
    }

    /// Parse the serialized spelling written by [`AttachmentKind::as_str`].
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "image" => Self::Image,
            "file" => Self::File,
            _ => return None,
        })
    }
}

/// One explicit answer option offered by an [`ApprovalRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalOption {
    pub id: String,
    pub label: String,
}

/// A consumer's answer to an Approval Request: the id of one declared option,
/// free text where the request accepts it, one answer per question for
/// multi-question asks, or the raw MCP elicitation JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// The id of one declared option on the request.
    OptionId(String),
    /// Free-form text.
    Text(String),
    /// One answer per question for multi-question input asks: each key is a
    /// `questions` entry's `id`, each value the answer list the provider
    /// contract expects.
    Answers(BTreeMap<String, Vec<String>>),
    /// The MCP elicitation JSON an `interaction_kind` `mcp_elicitation`
    /// request asked the host to return verbatim.
    Elicitation(serde_json::Value),
}

/// The category of a blocking Approval Request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    ToolPermission,
    PlanApproval,
    Question,
    McpElicitation,
}

impl ApprovalKind {
    /// The stable serialized spelling for this request kind.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToolPermission => "tool_permission",
            Self::PlanApproval => "plan_approval",
            Self::Question => "question",
            Self::McpElicitation => "mcp_elicitation",
        }
    }

    /// Parse the serialized spelling written by [`ApprovalKind::as_str`].
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "tool_permission" => Self::ToolPermission,
            "plan_approval" => Self::PlanApproval,
            "question" => Self::Question,
            "mcp_elicitation" => Self::McpElicitation,
            _ => return None,
        })
    }
}

/// A typed, blocking question from a running agent delivered to the Host
/// Application and answered through `approval.answer`.
///
/// `requires_confirm` asks the host to re-authenticate the user (biometric,
/// PIN, or a confirm dialog). The runtime cannot verify device biometrics; it
/// records which consumer answered on the `approval.resolved` event.
/// Answering is never implicit: the runtime never auto-approves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub kind: ApprovalKind,
    pub title: String,
    pub detail: String,
    /// The explicit options a consumer may pick between. Empty when the
    /// request accepts free text only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<ApprovalOption>,
    pub requires_confirm: bool,
    /// The provider-native interaction kind behind the request. Adapters
    /// that translate a typed provider interaction into an Approval Request
    /// carry it through so hosts can answer natively; protocol-level asks
    /// leave it `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_kind: Option<AgentInteractionKind>,
    /// The provider's input questions for `OrdinaryInput` and
    /// `McpElicitation` requests. Empty for asks that only offer contract
    /// options.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<AgentInputQuestion>,
    /// The provider-native request parameters for `McpElicitation` asks so
    /// a host can rebuild the elicitation exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
    /// The provider-native method for `McpElicitation` asks, so a legacy
    /// `PendingRunInput` can be rebuilt from the event alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_method: Option<String>,
}
