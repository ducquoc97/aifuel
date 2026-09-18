use aifuel_core::{
    AIFUEL_GATEWAY_REGISTRATION_NAME, AgentMcpRegistrationAdapter, AgentMcpRegistrationError,
};
use serde_json::{Map, Value as JsonValue};
use std::path::{Path, PathBuf};

pub(crate) static COPILOT_REGISTRATION: CopilotMcpRegistration = CopilotMcpRegistration;

/// GitHub Copilot CLI's documented user-level MCP configuration adapter.
pub struct CopilotMcpRegistration;

impl AgentMcpRegistrationAdapter for CopilotMcpRegistration {
    fn host_id(&self) -> &'static str {
        "copilot"
    }

    fn config_file(&self, host_home: &Path) -> PathBuf {
        host_home.join("mcp-config.json")
    }

    fn expected_entry(
        &self,
        gateway_executable: &Path,
    ) -> Result<JsonValue, AgentMcpRegistrationError> {
        let command = gateway_executable
            .to_str()
            .ok_or_else(|| config_error("AI Fuel executable path is not valid Unicode"))?;
        Ok(serde_json::json!({
            "type": "local",
            "command": command,
            "args": ["mcp", "gateway", "--agent", self.host_id()],
            "env": {"XDG_CONFIG_HOME": "${XDG_CONFIG_HOME}"},
            "tools": ["*"]
        }))
    }

    fn current_entry(
        &self,
        config: Option<&[u8]>,
    ) -> Result<Option<JsonValue>, AgentMcpRegistrationError> {
        let document = self.parse_config(config)?;
        let Some(servers) = document.get("mcpServers") else {
            return Ok(None);
        };
        let servers = servers
            .as_object()
            .ok_or_else(|| config_error("Copilot mcp-config.json mcpServers must be an object"))?;
        Ok(servers.get(AIFUEL_GATEWAY_REGISTRATION_NAME).cloned())
    }

    fn write_entry(
        &self,
        config: Option<&[u8]>,
        gateway_executable: &Path,
    ) -> Result<Vec<u8>, AgentMcpRegistrationError> {
        let mut document = self.parse_config(config)?;
        let root = document
            .as_object_mut()
            .ok_or_else(|| config_error("Copilot mcp-config.json must contain a JSON object"))?;
        if !root.contains_key("mcpServers") {
            root.insert("mcpServers".to_owned(), JsonValue::Object(Map::new()));
        }
        let servers = root
            .get_mut("mcpServers")
            .and_then(JsonValue::as_object_mut)
            .ok_or_else(|| config_error("Copilot mcp-config.json mcpServers must be an object"))?;
        servers.insert(
            AIFUEL_GATEWAY_REGISTRATION_NAME.to_owned(),
            self.expected_entry(gateway_executable)?,
        );
        serialize_config(&document)
    }

    fn remove_entry(&self, config: &[u8]) -> Result<Vec<u8>, AgentMcpRegistrationError> {
        let mut document = self.parse_config(Some(config))?;
        let root = document
            .as_object_mut()
            .ok_or_else(|| config_error("Copilot mcp-config.json must contain a JSON object"))?;
        if let Some(servers) = root.get_mut("mcpServers") {
            let servers = servers.as_object_mut().ok_or_else(|| {
                config_error("Copilot mcp-config.json mcpServers must be an object")
            })?;
            servers.remove(AIFUEL_GATEWAY_REGISTRATION_NAME);
        }
        serialize_config(&document)
    }
}

impl CopilotMcpRegistration {
    fn parse_config(&self, config: Option<&[u8]>) -> Result<JsonValue, AgentMcpRegistrationError> {
        let Some(config) = config else {
            return Ok(JsonValue::Object(Map::new()));
        };
        let document: JsonValue = serde_json::from_slice(config)
            .map_err(|_| config_error("Copilot mcp-config.json is malformed"))?;
        if !document.is_object() {
            return Err(config_error(
                "Copilot mcp-config.json must contain a JSON object",
            ));
        }
        Ok(document)
    }
}

fn serialize_config(document: &JsonValue) -> Result<Vec<u8>, AgentMcpRegistrationError> {
    let mut bytes = serde_json::to_vec_pretty(document)
        .map_err(|_| config_error("Copilot mcp-config.json could not be encoded"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn config_error(detail: &str) -> AgentMcpRegistrationError {
    AgentMcpRegistrationError::new(detail)
}

#[cfg(test)]
#[path = "registration/tests.rs"]
mod tests;
