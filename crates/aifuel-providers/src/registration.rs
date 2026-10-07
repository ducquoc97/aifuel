#[path = "codex/registration.rs"]
mod codex;

use aifuel_core::AgentMcpRegistrationAdapter;

/// The independent MCP Hosts with a built-in registration adapter, in
/// catalog order. `mcp setup` iterates this set when no `--agent` narrows
/// the operation.
const REGISTRATIONS: &[(&str, &dyn AgentMcpRegistrationAdapter)] = &[
    ("codex", &codex::CODEX_REGISTRATION),
    ("claude", &crate::claude::MCP_REGISTRATION_ADAPTER),
    ("copilot", &crate::copilot::COPILOT_REGISTRATION),
    ("antigravity", &crate::antigravity::MCP_REGISTRATION_ADAPTER),
    ("devin", &crate::devin::MCP_REGISTRATION_ADAPTER),
];

/// Find an adapter by independent MCP Host id.
pub fn agent_mcp_registration_adapter(
    host_id: &str,
) -> Option<&'static dyn AgentMcpRegistrationAdapter> {
    REGISTRATIONS
        .iter()
        .find(|(id, _)| *id == host_id)
        .map(|(_, adapter)| *adapter)
}

/// Every MCP Host id with a registration adapter.
pub fn agent_mcp_registration_host_ids() -> impl Iterator<Item = &'static str> {
    REGISTRATIONS.iter().map(|(id, _)| *id)
}

pub(crate) fn codex_mcp_runtime_entry(
    gateway_executable: &std::path::Path,
    allowed_tools: &[String],
) -> Result<serde_json::Value, String> {
    codex::CODEX_REGISTRATION
        .runtime_entry(gateway_executable, allowed_tools)
        .map_err(|error| error.to_string())
}
