use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

const MAX_GATEWAY_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_SERVER_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_OUTPUT_BUFFER_BYTES: usize = 128 * 1024 * 1024;
const MAX_LIST_SNAPSHOT_BYTES: usize = 256 * 1024 * 1024;
const MAX_LIST_ENTRIES: usize = 100_000;

/// The selected external MCP server definitions for one MCP Host.
///
/// The facade validates the complete catalog before resolving a host. A host
/// without an explicit entry inherits defaults; an explicit empty list stays
/// empty.
#[derive(Clone, Debug)]
pub struct McpGatewayFacade {
    host_id: String,
    selected_servers: Vec<SelectedMcpServer>,
    gateway_limits: GatewayLimits,
}

impl McpGatewayFacade {
    pub fn from_json(
        bytes: &[u8],
        host_id: impl Into<String>,
        user_home: impl Into<PathBuf>,
    ) -> Result<Self, GatewayConfigError> {
        let catalog: GatewayCatalog = serde_json::from_slice(bytes).map_err(|error| {
            GatewayConfigError(format!(
                "invalid MCP gateway JSON near line {} column {}",
                error.line(),
                error.column()
            ))
        })?;
        let host_id = host_id.into();
        let user_home = user_home.into();
        catalog.validate()?;

        let selected_ids = catalog
            .agents
            .get(&host_id)
            .map(|selection| selection.servers.as_slice())
            .unwrap_or(catalog.defaults.as_slice());
        let selected_servers = selected_ids
            .iter()
            .map(|id| SelectedMcpServer {
                id: id.clone(),
                definition: catalog
                    .servers
                    .get(id)
                    .expect("validated server reference")
                    .clone(),
                user_home: user_home.clone(),
            })
            .collect();

        Ok(Self {
            host_id,
            selected_servers,
            gateway_limits: catalog.gateway,
        })
    }

    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    pub fn selected_servers(&self) -> &[SelectedMcpServer] {
        &self.selected_servers
    }

    pub fn gateway_limits(&self) -> &GatewayLimits {
        &self.gateway_limits
    }
}

#[derive(Clone, Debug)]
pub struct SelectedMcpServer {
    pub id: String,
    pub definition: McpServerDefinition,
    pub user_home: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "transport")]
pub enum McpServerDefinition {
    #[serde(rename = "stdio")]
    Stdio(StdioServerDefinition),
    #[serde(rename = "streamable-http")]
    StreamableHttp(StreamableHttpServerDefinition),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StdioServerDefinition {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, LiteralEnvironmentValue>,
    #[serde(default)]
    pub env_from: BTreeMap<String, String>,
    #[serde(default)]
    pub limits: ServerLimits,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiteralEnvironmentValue {
    pub value: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StreamableHttpServerDefinition {
    pub url: String,
    pub auth: Option<BearerTokenAuth>,
    #[serde(default)]
    pub secret_headers: BTreeMap<String, NamedSecretHeader>,
    #[serde(default)]
    pub limits: ServerLimits,
}

/// Static authentication for one remote MCP endpoint.
///
/// The catalog stores only the environment variable name. The referenced
/// value is resolved by the gateway process at startup and is never part of
/// the catalog or an agent registration.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BearerTokenAuth {
    pub bearer_token_env: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedSecretHeader {
    pub env: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ServerLimits {
    pub connect_seconds: u64,
    pub discovery_seconds: u64,
    pub operation_seconds: u64,
    pub shutdown_seconds: u64,
    pub max_concurrent_requests: usize,
    pub max_message_bytes: usize,
    pub max_list_snapshot_bytes: usize,
    pub max_list_entries: usize,
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            connect_seconds: 15,
            discovery_seconds: 30,
            operation_seconds: 120,
            shutdown_seconds: 5,
            max_concurrent_requests: 8,
            max_message_bytes: 8 * 1024 * 1024,
            max_list_snapshot_bytes: 32 * 1024 * 1024,
            max_list_entries: 10_000,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct GatewayLimits {
    pub max_concurrent_requests: usize,
    pub max_message_bytes: usize,
    pub max_output_buffer_bytes: usize,
    pub output_stall_seconds: u64,
}

impl Default for GatewayLimits {
    fn default() -> Self {
        Self {
            max_concurrent_requests: 64,
            max_message_bytes: 8 * 1024 * 1024,
            max_output_buffer_bytes: 16 * 1024 * 1024,
            output_stall_seconds: 30,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GatewayConfigError(String);

impl std::fmt::Display for GatewayConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for GatewayConfigError {}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct GatewayCatalog {
    servers: BTreeMap<String, McpServerDefinition>,
    defaults: Vec<String>,
    agents: BTreeMap<String, AgentServerSelection>,
    gateway: GatewayLimits,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentServerSelection {
    servers: Vec<String>,
}

impl GatewayCatalog {
    fn validate(&self) -> Result<(), GatewayConfigError> {
        validate_selection("defaults", &self.defaults, &self.servers)?;
        for (host_id, selection) in &self.agents {
            if host_id.trim().is_empty() {
                return Err(GatewayConfigError(
                    "MCP Host ids cannot be empty".to_owned(),
                ));
            }
            validate_selection(
                &format!("agents.{host_id}.servers"),
                &selection.servers,
                &self.servers,
            )?;
        }
        validate_gateway_limits(&self.gateway)?;
        for (server_id, definition) in &self.servers {
            if server_id.trim().is_empty() {
                return Err(GatewayConfigError(
                    "MCP server ids cannot be empty".to_owned(),
                ));
            }
            match definition {
                McpServerDefinition::Stdio(config) => {
                    if config.command.trim().is_empty() {
                        return Err(GatewayConfigError(format!(
                            "servers.{server_id}.command cannot be empty"
                        )));
                    }
                    if config.cwd.as_ref().is_some_and(|path| !path.is_absolute()) {
                        return Err(GatewayConfigError(format!(
                            "servers.{server_id}.cwd must be an absolute path"
                        )));
                    }
                    validate_environment(server_id, config)?;
                    validate_server_limits(server_id, &config.limits)?;
                }
                McpServerDefinition::StreamableHttp(config) => {
                    if config.url.trim().is_empty() {
                        return Err(GatewayConfigError(format!(
                            "servers.{server_id}.url cannot be empty"
                        )));
                    }
                    if let Some(auth) = &config.auth {
                        validate_environment_reference(
                            &format!("servers.{server_id}.auth.bearerTokenEnv"),
                            &auth.bearer_token_env,
                        )?;
                    }
                    validate_secret_headers(server_id, &config.secret_headers)?;
                    validate_server_limits(server_id, &config.limits)?;
                }
            }
        }
        Ok(())
    }
}

fn validate_selection(
    location: &str,
    selection: &[String],
    servers: &BTreeMap<String, McpServerDefinition>,
) -> Result<(), GatewayConfigError> {
    let mut seen = HashSet::new();
    for server_id in selection {
        if !seen.insert(server_id) {
            return Err(GatewayConfigError(format!(
                "{location} contains duplicate server id {server_id:?}"
            )));
        }
        if !servers.contains_key(server_id) {
            return Err(GatewayConfigError(format!(
                "{location} references unknown server id {server_id:?}"
            )));
        }
    }
    Ok(())
}

fn validate_environment(
    server_id: &str,
    config: &StdioServerDefinition,
) -> Result<(), GatewayConfigError> {
    let mut keys = HashSet::new();
    for key in config.env.keys().chain(config.env_from.keys()) {
        if key.is_empty() || key.contains('=') || key.contains('\0') {
            return Err(GatewayConfigError(format!(
                "servers.{server_id} contains an invalid environment key"
            )));
        }
        let normalized = if cfg!(windows) {
            key.to_ascii_lowercase()
        } else {
            key.clone()
        };
        if !keys.insert(normalized) {
            return Err(GatewayConfigError(format!(
                "servers.{server_id} defines an environment key more than once"
            )));
        }
    }
    if config
        .env_from
        .values()
        .any(|source| source.trim().is_empty())
    {
        return Err(GatewayConfigError(format!(
            "servers.{server_id}.envFrom contains an empty environment reference"
        )));
    }
    Ok(())
}

fn validate_environment_reference(
    location: &str,
    reference: &str,
) -> Result<(), GatewayConfigError> {
    if reference.trim().is_empty() || reference.contains('=') || reference.contains('\0') {
        return Err(GatewayConfigError(format!(
            "{location} must name a non-empty environment variable"
        )));
    }
    Ok(())
}

fn validate_secret_headers(
    server_id: &str,
    headers: &BTreeMap<String, NamedSecretHeader>,
) -> Result<(), GatewayConfigError> {
    let mut seen = HashSet::new();
    for (name, header) in headers {
        if !is_valid_header_name(name) {
            return Err(GatewayConfigError(format!(
                "servers.{server_id}.secretHeaders contains an invalid header name"
            )));
        }
        let normalized = name.to_ascii_lowercase();
        if !seen.insert(normalized.clone()) {
            return Err(GatewayConfigError(format!(
                "servers.{server_id}.secretHeaders contains duplicate header names"
            )));
        }
        if forbidden_secret_header_names()
            .iter()
            .any(|forbidden| *forbidden == normalized)
        {
            return Err(GatewayConfigError(format!(
                "servers.{server_id}.secretHeaders contains a forbidden protocol header"
            )));
        }
        validate_environment_reference(
            &format!("servers.{server_id}.secretHeaders.{name}.env"),
            &header.env,
        )?;
    }
    Ok(())
}

fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            matches!(
                byte,
                b'0'..=b'9'
                    | b'a'..=b'z'
                    | b'A'..=b'Z'
                    | b'!'
                    | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
            )
        })
}

fn forbidden_secret_header_names() -> &'static [&'static str] {
    &[
        "accept",
        "accept-charset",
        "accept-encoding",
        "accept-language",
        "authorization",
        "connection",
        "content-encoding",
        "content-language",
        "content-length",
        "content-type",
        "host",
        "keep-alive",
        "last-event-id",
        "mcp-method",
        "mcp-name",
        "mcp-protocol-version",
        "mcp-session-id",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ]
}

fn validate_server_limits(
    server_id: &str,
    limits: &ServerLimits,
) -> Result<(), GatewayConfigError> {
    if limits.connect_seconds == 0
        || limits.connect_seconds > 300
        || limits.discovery_seconds == 0
        || limits.discovery_seconds > 1800
        || limits.operation_seconds == 0
        || limits.operation_seconds > 86_400
        || limits.shutdown_seconds == 0
        || limits.shutdown_seconds > 60
        || limits.max_concurrent_requests == 0
        || limits.max_concurrent_requests > 64
        || limits.max_message_bytes == 0
        || limits.max_message_bytes > MAX_SERVER_MESSAGE_BYTES
        || limits.max_list_snapshot_bytes == 0
        || limits.max_list_snapshot_bytes > MAX_LIST_SNAPSHOT_BYTES
        || limits.max_list_entries == 0
        || limits.max_list_entries > MAX_LIST_ENTRIES
    {
        return Err(GatewayConfigError(format!(
            "servers.{server_id}.limits values must be positive"
        )));
    }
    Ok(())
}

fn validate_gateway_limits(limits: &GatewayLimits) -> Result<(), GatewayConfigError> {
    if limits.max_concurrent_requests == 0
        || limits.max_concurrent_requests > 64
        || limits.max_message_bytes == 0
        || limits.max_message_bytes > MAX_GATEWAY_MESSAGE_BYTES
        || limits.max_output_buffer_bytes == 0
        || limits.max_output_buffer_bytes > MAX_OUTPUT_BUFFER_BYTES
        || limits.max_output_buffer_bytes < limits.max_message_bytes.saturating_add(1)
        || limits.output_stall_seconds == 0
        || limits.output_stall_seconds > 300
    {
        return Err(GatewayConfigError(
            "gateway limits values must be positive".to_owned(),
        ));
    }
    Ok(())
}

pub fn default_cwd(server: &SelectedMcpServer) -> &Path {
    match &server.definition {
        McpServerDefinition::Stdio(config) => config.cwd.as_deref().unwrap_or(&server.user_home),
        McpServerDefinition::StreamableHttp(_) => &server.user_home,
    }
}
