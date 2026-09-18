#[path = "codex/registration.rs"]
mod codex;

use aifuel_core::AgentMcpRegistrationAdapter;

/// Find an adapter by independent MCP Host id.
pub fn agent_mcp_registration_adapter(
    host_id: &str,
) -> Option<&'static dyn AgentMcpRegistrationAdapter> {
    match host_id {
        "codex" => Some(&codex::CODEX_REGISTRATION),
        "claude" => Some(&crate::claude::MCP_REGISTRATION_ADAPTER),
        _ => None,
    }
}
