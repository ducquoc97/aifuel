//! Same-user local approval delivery for connection-owned execution MCP runs.

mod protocol;
mod records;
mod server;
mod submit;

#[cfg(test)]
#[path = "approval_ipc/tests.rs"]
mod tests;

pub use crate::approval_ipc_protocol::LocalApprovalDecision;
pub(crate) use server::LocalApprovalServer;
pub use submit::submit_local_approval;
