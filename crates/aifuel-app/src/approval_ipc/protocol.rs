use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct OwnerRecord {
    pub(super) owner_id: String,
    pub(super) socket: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct PendingApprovalRecord {
    pub(super) owner_id: String,
    pub(super) socket: PathBuf,
    pub(super) run_id: String,
    pub(super) input_id: String,
}
