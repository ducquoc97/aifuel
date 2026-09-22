pub mod launcher;
pub mod mcp_catalog;
pub mod profile;
pub mod selection_cli;

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

/// Compose a run owner with the local user's execution MCP admission policy.
pub fn execution_run_manager() -> Result<aifuel_app::RunManager, String> {
    if env::var_os("AIFUEL_MANAGED_RUN").is_some() {
        return Err(
            "nested AI Fuel execution is not available inside a managed Agent Run".to_owned(),
        );
    }
    let config = aifuel_app::selection::SelectionStore::load(execution_config_path()?)
        .map_err(|error| error.to_string())?;
    if config.policy.retain_content {
        return Err(
            "persistent Agent Run content retention is not supported by this execution owner"
                .to_owned(),
        );
    }
    Ok(
        aifuel_app::RunManager::new(aifuel_providers::agent_run_adapters())
            .with_allowed_roots(config.policy.allowed_roots),
    )
}

pub fn execution_config_path() -> Result<PathBuf, String> {
    let home = user_home_dir()?;
    Ok(user_config_dir(&home)?
        .join("aifuel")
        .join("execution.json"))
}

/// Load the last successful model catalog snapshot for the execution MCP
/// endpoint. A missing cache is an explicit unknown catalog, not an empty
/// successful discovery.
pub fn model_catalog_snapshot() -> Result<Vec<serde_json::Value>, String> {
    let home = user_home_dir()?;
    let path = user_config_dir(&home)?
        .join("aifuel")
        .join("model-catalog.json");
    let store = aifuel_app::selection::CatalogEvidenceStore::load(path)
        .map_err(|error| error.to_string())?;
    Ok(store
        .scopes()
        .filter_map(|scope| store.snapshot(scope))
        .map(|snapshot| serde_json::to_value(snapshot).expect("catalog snapshot serializes"))
        .collect())
}

/// Apply global defaults and an optional named profile to one explicit CLI
/// request. Explicit values remain highest precedence; flags that were omitted
/// are allowed to inherit from the profile and global defaults.
pub fn resolve_selection_for_run(
    mut request: aifuel_core::RunRequest,
    profile: Option<&str>,
    access_explicit: bool,
    deadline_explicit: bool,
) -> Result<aifuel_core::RunRequest, String> {
    let config = aifuel_app::selection::SelectionStore::load(execution_config_path()?)
        .map_err(|error| error.to_string())?;
    let inputs = aifuel_app::selection::SelectionInputs {
        explicit: aifuel_app::selection::SelectionSettings {
            provider: Some(request.provider),
            model: request.model.clone(),
            effort: request.effort.clone(),
            access: access_explicit.then_some(request.access),
            overall_deadline_seconds: None,
        },
        profile: profile.map(str::to_owned),
        interactive: false,
        deadline_override: deadline_explicit
            .then_some(request.timeout.map(|value| value.as_secs())),
    };
    let resolved = config
        .resolve(&inputs, None)
        .map_err(|error| error.to_string())?;
    request.model = resolved.model;
    request.effort = resolved.effort;
    request.access = resolved.access;
    request.timeout = resolved.overall_deadline;
    Ok(request)
}

/// Compose one independent MCP Host registration adapter with shared setup.
pub fn agent_mcp_setup_facade(host_id: &str) -> Result<AgentMcpSetupFacade<'static>, String> {
    let adapter = aifuel_providers::agent_mcp_registration_adapter(host_id)
        .ok_or_else(|| format!("MCP Host {host_id:?} has no Agent MCP Registration adapter"))?;
    let user_home = user_home_dir()?;
    let host_home = adapter
        .configuration_home(&user_home)
        .map_err(|error| error.to_string())?;
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
