use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalApprovalDecision {
    Accept,
    Decline,
    Cancel,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ApprovalMessage {
    pub(crate) owner_id: String,
    pub(crate) run_id: String,
    pub(crate) input_id: String,
    pub(crate) decision: LocalApprovalDecision,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ApprovalReply {
    pub(crate) owner_found: bool,
    pub(crate) accepted: bool,
    pub(crate) message: Option<String>,
}
