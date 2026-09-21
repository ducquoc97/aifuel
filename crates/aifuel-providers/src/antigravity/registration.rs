use aifuel_core::{
    AIFUEL_GATEWAY_REGISTRATION_NAME, AgentMcpRegistrationAdapter, AgentMcpRegistrationError,
};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub(crate) static ADAPTER: AntigravityMcpRegistration = AntigravityMcpRegistration;

/// Antigravity CLI's global MCP server configuration adapter.
pub struct AntigravityMcpRegistration;

impl AgentMcpRegistrationAdapter for AntigravityMcpRegistration {
    fn host_id(&self) -> &'static str {
        "antigravity"
    }

    fn configuration_home(&self, user_home: &Path) -> Result<PathBuf, AgentMcpRegistrationError> {
        Ok(user_home.join(".gemini").join("config"))
    }

    fn config_file(&self, host_home: &Path) -> PathBuf {
        host_home.join("mcp_config.json")
    }

    fn expected_entry(
        &self,
        gateway_executable: &Path,
    ) -> Result<Value, AgentMcpRegistrationError> {
        let command = gateway_executable
            .to_str()
            .ok_or_else(|| config_error("AI Fuel executable path is not valid Unicode"))?;
        Ok(serde_json::json!({
            "command": command,
            "args": ["mcp", "gateway", "--agent", self.host_id()],
            "disabled": false
        }))
    }

    fn current_entry(
        &self,
        config: Option<&[u8]>,
    ) -> Result<Option<Value>, AgentMcpRegistrationError> {
        let document = parse_config(config)?;
        let Some(servers) = document.get("mcpServers") else {
            return Ok(None);
        };
        let servers = servers.as_object().ok_or_else(|| {
            config_error("Antigravity mcp_config.json mcpServers must be an object")
        })?;
        Ok(servers.get(AIFUEL_GATEWAY_REGISTRATION_NAME).cloned())
    }

    fn write_entry(
        &self,
        config: Option<&[u8]>,
        gateway_executable: &Path,
    ) -> Result<Vec<u8>, AgentMcpRegistrationError> {
        let mut document = parse_config(config)?;
        let servers = document
            .entry("mcpServers".to_owned())
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .ok_or_else(|| {
                config_error("Antigravity mcp_config.json mcpServers must be an object")
            })?;
        servers.insert(
            AIFUEL_GATEWAY_REGISTRATION_NAME.to_owned(),
            self.expected_entry(gateway_executable)?,
        );
        serialize_config(document)
    }

    fn remove_entry(&self, config: &[u8]) -> Result<Vec<u8>, AgentMcpRegistrationError> {
        let mut document = parse_config(Some(config))?;
        let Some(servers) = document.get_mut("mcpServers") else {
            return Ok(config.to_vec());
        };
        let servers = servers.as_object_mut().ok_or_else(|| {
            config_error("Antigravity mcp_config.json mcpServers must be an object")
        })?;
        servers.remove(AIFUEL_GATEWAY_REGISTRATION_NAME);
        serialize_config(document)
    }
}

fn parse_config(config: Option<&[u8]>) -> Result<Map<String, Value>, AgentMcpRegistrationError> {
    let Some(config) = config else {
        return Ok(Map::new());
    };
    let document: Value = serde_json::from_slice(config)
        .map_err(|_| config_error("Antigravity mcp_config.json is malformed JSON"))?;
    document
        .as_object()
        .cloned()
        .ok_or_else(|| config_error("Antigravity mcp_config.json must be a JSON object"))
}

fn serialize_config(document: Map<String, Value>) -> Result<Vec<u8>, AgentMcpRegistrationError> {
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(document))
        .map_err(|_| config_error("Antigravity mcp_config.json could not be serialized"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn config_error(detail: &str) -> AgentMcpRegistrationError {
    AgentMcpRegistrationError::new(detail)
}

#[cfg(test)]
#[path = "registration/tests.rs"]
mod tests;
