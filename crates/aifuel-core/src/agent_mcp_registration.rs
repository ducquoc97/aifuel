use serde_json::Value;
use std::path::{Path, PathBuf};
use std::{error, fmt};

/// The stable name AI Fuel registers for its MCP Gateway in an MCP Host.
pub const AIFUEL_GATEWAY_REGISTRATION_NAME: &str = "aifuel-gateway";

/// A host-specific adapter for configuring the AI Fuel Gateway as an MCP server.
///
/// MCP Host identity and Agent Integration are separate capabilities. An
/// adapter can register a host without providing an executable agent run.
pub trait AgentMcpRegistrationAdapter: Send + Sync {
    /// The independent MCP Host id used to select gateway server definitions.
    fn host_id(&self) -> &'static str;

    /// Resolve the host's user-level configuration file from its home directory.
    fn config_file(&self, host_home: &Path) -> PathBuf;

    /// The semantic registration AI Fuel intends to write for this executable.
    fn expected_entry(&self, gateway_executable: &Path)
    -> Result<Value, AgentMcpRegistrationError>;

    /// Read the named registration as a format-independent semantic value.
    fn current_entry(
        &self,
        config: Option<&[u8]>,
    ) -> Result<Option<Value>, AgentMcpRegistrationError>;

    /// Add or replace the named registration while preserving unrelated config.
    fn write_entry(
        &self,
        config: Option<&[u8]>,
        gateway_executable: &Path,
    ) -> Result<Vec<u8>, AgentMcpRegistrationError>;

    /// Remove only the named registration from a previously validated config.
    fn remove_entry(&self, config: &[u8]) -> Result<Vec<u8>, AgentMcpRegistrationError>;
}

/// A safe adapter error that does not include user configuration contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMcpRegistrationError(String);

impl AgentMcpRegistrationError {
    pub fn new(detail: impl Into<String>) -> Self {
        Self(detail.into())
    }
}

impl fmt::Display for AgentMcpRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl error::Error for AgentMcpRegistrationError {}
