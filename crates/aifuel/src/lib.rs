pub mod launcher;

use aifuel_app::{AgentMcpSetupFacade, AgentRunFacade, McpGatewayFacade, MonitoringFacade};
use aifuel_providers::{CollectionConfig, DiscoveryContext, ProviderMonitoring};
use std::env;
use std::fs;
use std::path::PathBuf;

/// Construct the monitoring dependencies at the executable boundary.
pub fn monitoring_facade() -> Result<MonitoringFacade<ProviderMonitoring>, String> {
    let context = DiscoveryContext::from_environment().map_err(|error| error.to_string())?;
    let monitoring =
        ProviderMonitoring::new(context.home_dir(), CollectionConfig::from_environment())?;
    Ok(MonitoringFacade::new(monitoring))
}

/// Compose the shared Agent Run facade with the compiled provider adapters.
pub fn agent_run_facade() -> AgentRunFacade<'static> {
    AgentRunFacade::new(aifuel_providers::agent_run_adapters())
}

/// Compose one independent MCP Host registration adapter with shared setup.
pub fn agent_mcp_setup_facade(host_id: &str) -> Result<AgentMcpSetupFacade<'static>, String> {
    let adapter = aifuel_providers::agent_mcp_registration_adapter(host_id)
        .ok_or_else(|| format!("MCP Host {host_id:?} has no Agent MCP Registration adapter"))?;
    let user_home = user_home_dir()?;
    let host_home = env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| user_home.join(".codex"));
    if !host_home.is_absolute() {
        return Err("CODEX_HOME must be an absolute path".to_owned());
    }
    if !host_home.is_dir() {
        return Err("Codex home directory must already exist".to_owned());
    }
    let state_dir = user_config_dir(&user_home)?
        .join("aifuel")
        .join("mcp-registrations");
    let gateway_executable = env::current_exe()
        .map_err(|_| "AI Fuel executable path could not be resolved".to_owned())?;
    Ok(AgentMcpSetupFacade::new(
        adapter,
        &host_home,
        gateway_executable,
        state_dir,
    ))
}

/// Load and resolve the central gateway catalog at the executable boundary.
pub fn mcp_gateway_facade(host_id: &str) -> Result<McpGatewayFacade, String> {
    let user_home = user_home_dir()?;
    let config_file = user_config_dir(&user_home)?.join("aifuel").join("mcp.json");
    let bytes = fs::read(&config_file).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "MCP Gateway configuration file was not found".to_owned()
        } else {
            "MCP Gateway configuration file could not be read".to_owned()
        }
    })?;
    McpGatewayFacade::from_json(&bytes, host_id, user_home).map_err(|error| error.to_string())
}

fn user_home_dir() -> Result<PathBuf, String> {
    #[cfg(windows)]
    let home = env::var_os("USERPROFILE").map(PathBuf::from).or_else(|| {
        let mut path = PathBuf::from(env::var_os("HOMEDRIVE")?);
        path.push(env::var_os("HOMEPATH")?);
        Some(path)
    });
    #[cfg(not(windows))]
    let home = env::var_os("HOME").map(PathBuf::from);

    home.filter(|path| path.is_absolute())
        .ok_or_else(|| "the user's home directory could not be resolved".to_owned())
}

#[cfg(windows)]
fn user_config_dir(_user_home: &std::path::Path) -> Result<PathBuf, String> {
    env::var_os("APPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            "the user's application configuration directory could not be resolved".to_owned()
        })
}

#[cfg(target_os = "macos")]
fn user_config_dir(user_home: &std::path::Path) -> Result<PathBuf, String> {
    Ok(user_home.join("Library").join("Application Support"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn user_config_dir(user_home: &std::path::Path) -> Result<PathBuf, String> {
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        && config_home.is_absolute()
    {
        return Ok(config_home);
    }
    Ok(user_home.join(".config"))
}

#[cfg(not(any(unix, windows)))]
fn user_config_dir(_user_home: &std::path::Path) -> Result<PathBuf, String> {
    Err("the user's application configuration directory could not be resolved".to_owned())
}
