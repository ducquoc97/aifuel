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
        "copilot" => Some(&crate::copilot::COPILOT_REGISTRATION),
        "antigravity" => Some(&crate::antigravity::MCP_REGISTRATION_ADAPTER),
        "gemini" => Some(&crate::gemini::MCP_REGISTRATION_ADAPTER),
        "devin" => Some(&crate::devin::MCP_REGISTRATION_ADAPTER),
        _ => None,
    }
}

pub(crate) fn codex_mcp_runtime_entry(
    gateway_executable: &std::path::Path,
    allowed_tools: &[String],
) -> Result<serde_json::Value, String> {
    codex::CODEX_REGISTRATION
        .runtime_entry(gateway_executable, allowed_tools)
        .map_err(|error| error.to_string())
}
