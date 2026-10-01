pub mod auth_cli;
pub mod connect;
pub mod launcher;
pub mod mcp_catalog;
mod model_catalog;
pub mod model_cli;
pub mod profile;
pub mod run_cli;
mod run_selection;
pub mod selection_cli;

use aifuel_app::{
    AgentMcpSetupFacade, AgentRunFacade, McpGatewayFacade, MonitoringFacade, WebhookNotifier,
};
use aifuel_core::{
    AgentCapability, AgentCapabilityEvidence, AgentExecutionAdapter, AgentIntegrationInfo,
    AgentRunError, AgentRunOutputHandler, AgentSetupGuidance, ExecutionConfig, IntegrationId,
    ProviderId, RunCancellationToken, RunRequest, RunResult,
};
use aifuel_providers::{CollectionConfig, DiscoveryContext, ProviderMonitoring};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

pub use model_catalog::{model_catalog_snapshot, refresh_model_catalog};

/// Construct the monitoring dependencies at the executable boundary.
pub fn monitoring_facade() -> Result<MonitoringFacade<ProviderMonitoring>, String> {
    let context = DiscoveryContext::from_environment().map_err(|error| error.to_string())?;
    let mut monitoring =
        ProviderMonitoring::new(context.home_dir(), CollectionConfig::from_environment())?;
    match integration_registry() {
        Ok(registry) => {
            let credentials = aifuel_providers::CredentialStore::new(aifuel_config_dir()?);
            monitoring =
                monitoring.with_integrations(registry.list().cloned().collect(), credentials);
        }
        // A malformed registry is a configuration error: catalog-provider
        // monitoring still runs, and the failure is reported as a collection
        // error in the report rather than silently narrowing coverage.
        Err(error) => {
            monitoring = monitoring.with_registry_error(error);
        }
    }
    let facade = MonitoringFacade::new(monitoring);
    // A malformed webhooks.json is reported but never blocks collection.
    match WebhookNotifier::load(&aifuel_config_dir()?) {
        Ok(notifier) => Ok(facade.with_notifier(notifier)),
        Err(error) => {
            eprintln!("aifuel: webhook configuration ignored: {error}");
            Ok(facade)
        }
    }
}

/// Compose the shared Agent Run facade over the runtime Integration set.
pub fn agent_run_facade() -> Result<AgentRunFacade, String> {
    Ok(AgentRunFacade::new(execution_adapters()?))
}

/// The user-level AI Fuel configuration directory (`credentials.json`,
/// `providers.json`, `execution.json`, ... live inside it).
fn aifuel_config_dir() -> Result<PathBuf, String> {
    Ok(user_config_dir(&user_home_dir()?)?.join("aifuel"))
}

/// The runtime Integration Registry: built-in descriptors plus validated
/// `providers.json` entries. Registry errors are configuration errors and
/// surface to the caller rather than narrowing the integration set.
pub fn integration_registry() -> Result<aifuel_providers::IntegrationRegistry, String> {
    let config_dir = aifuel_config_dir()?;
    let config = aifuel_providers::ProvidersConfig::load(
        config_dir.join(aifuel_providers::PROVIDERS_FILE_NAME),
    )
    .map_err(|error| error.to_string())?;
    aifuel_providers::IntegrationRegistry::build(
        aifuel_providers::builtin_integrations(),
        config.into_entries(),
    )
    .map_err(|error| error.to_string())
}

/// Build the owned execution adapter set from the runtime registry: compiled
/// CLI adapters behind their `Cli` descriptors and wire adapters behind
/// `Http` descriptors. A `Cli` descriptor naming an adapter outside the
/// compiled set is served by the agent runtime (`aifuel runtime`) instead -
/// it stays a valid registry entry for listing and auth, and simply has no
/// adapter on this execution surface.
fn runtime_adapters() -> Result<Vec<Arc<dyn AgentExecutionAdapter>>, String> {
    let registry = integration_registry()?;
    let credentials = aifuel_providers::CredentialStore::new(aifuel_config_dir()?);
    let compiled = aifuel_providers::agent_run_adapters();
    let mut adapters: Vec<Arc<dyn AgentExecutionAdapter>> = Vec::new();
    for descriptor in registry.list() {
        match &descriptor.integration.execution {
            ExecutionConfig::Cli { adapter } => {
                let Some(compiled) = compiled
                    .iter()
                    .find(|candidate| candidate.integration().as_str() == adapter.as_str())
                else {
                    eprintln!(
                        "aifuel: {adapter} is served by the agent runtime, not the compiled run surface"
                    );
                    continue;
                };
                adapters.push(Arc::new(StaticAdapter(*compiled)));
            }
            ExecutionConfig::Http { .. } => {
                match aifuel_providers::WireExecutionAdapter::from_integration(
                    &descriptor.integration,
                    credentials.clone(),
                ) {
                    Ok(adapter) => adapters.push(Arc::new(adapter)),
                    // A descriptor for a Wire Api with no compiled engine is
                    // still a valid registry entry for listing and auth, but
                    // has no execution adapter - selection reports it
                    // unsupported instead of failing the whole run surface.
                    Err(aifuel_providers::WireAdapterError::IncompatibleExecution(detail)) => {
                        eprintln!("aifuel: {detail}");
                    }
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
    }
    Ok(adapters)
}

/// Delegates a compiled `&'static` adapter through the owned adapter set so
/// built-in CLI integrations can share the runtime registry.
struct StaticAdapter(&'static dyn AgentExecutionAdapter);

impl AgentExecutionAdapter for StaticAdapter {
    fn integration(&self) -> IntegrationId {
        self.0.integration()
    }

    fn provider(&self) -> ProviderId {
        self.0.provider()
    }

    fn setup_guidance(&self) -> Option<AgentSetupGuidance> {
        self.0.setup_guidance()
    }

    fn declared_agent_capabilities(&self) -> BTreeMap<AgentCapability, AgentCapabilityEvidence> {
        self.0.declared_agent_capabilities()
    }

    fn agent_info(&self) -> AgentIntegrationInfo {
        self.0.agent_info()
    }

    fn validate(&self, request: &RunRequest) -> Result<(), AgentRunError> {
        self.0.validate(request)
    }

    fn execute(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
    ) -> Result<RunResult, AgentRunError> {
        self.0.execute(request, cancellation)
    }

    fn execute_with_output_handler(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
        output_handler: &dyn AgentRunOutputHandler,
    ) -> Result<RunResult, AgentRunError> {
        self.0
            .execute_with_output_handler(request, cancellation, output_handler)
    }
}

/// Build the owned execution adapter set over one shared `AgentRuntime`:
/// `Cli` descriptors resolve to the adapter the runtime registered for
/// their Integration Identity behind a `RuntimeExecutionAdapter` shim, and
/// `Http` descriptors to wire adapters. A `Cli` descriptor the runtime does
/// not serve stays a valid registry entry for listing and auth, and simply
/// has no adapter on this execution surface. Every shim in the set shares
/// the one `AgentRuntime`, so sessions, resume cursors, and the durable
/// session event log have a single owner per constructed adapter set.
fn execution_adapters() -> Result<Vec<Arc<dyn AgentExecutionAdapter>>, String> {
    let runtime = Arc::new(agent_runtime()?);
    let registry = integration_registry()?;
    let credentials = aifuel_providers::CredentialStore::new(aifuel_config_dir()?);
    let mut adapters: Vec<Arc<dyn AgentExecutionAdapter>> = Vec::new();
    for descriptor in registry.list() {
        match &descriptor.integration.execution {
            ExecutionConfig::Cli { adapter } => {
                match aifuel_runtime::RuntimeExecutionAdapter::resolve(&runtime, descriptor.id()) {
                    Some(shim) => adapters.push(Arc::new(shim)),
                    None => eprintln!(
                        "aifuel: {adapter} has no adapter registered in the agent runtime"
                    ),
                }
            }
            ExecutionConfig::Http { .. } => {
                match aifuel_providers::WireExecutionAdapter::from_integration(
                    &descriptor.integration,
                    credentials.clone(),
                ) {
                    Ok(adapter) => adapters.push(Arc::new(adapter)),
                    // A descriptor for a Wire Api with no compiled engine is
                    // still a valid registry entry for listing and auth, but
                    // has no execution adapter - selection reports it
                    // unsupported instead of failing the whole run surface.
                    Err(aifuel_providers::WireAdapterError::IncompatibleExecution(detail)) => {
                        eprintln!("aifuel: {detail}");
                    }
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
    }
    Ok(adapters)
}

/// Open the portable agent runtime over the per-user run store for the
/// `aifuel runtime` stdio bridge. The store directory is hardened the same
/// way `execution_run_manager` does it, since WAL sidecar files inherit
/// directory permissions.
pub fn agent_runtime() -> Result<aifuel_runtime::AgentRuntime, String> {
    let db_path = run_store_path()?;
    #[cfg(unix)]
    if let Some(directory) = db_path.parent() {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = std::fs::create_dir_all(directory).and_then(|()| {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
        }) {
            eprintln!("aifuel: could not restrict the run history directory: {error}");
        }
    }
    aifuel_runtime::AgentRuntime::open(db_path).map_err(|error| error.to_string())
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
    let manager = aifuel_app::RunManager::new(execution_adapters()?)
        .with_execution_policy(&config.policy)
        .with_session_store(session_store_path()?)?;
    let db_path = run_store_path()?;
    // WAL sidecar files carry the same history and inherit directory
    // permissions, so the application directory itself is owner-only.
    #[cfg(unix)]
    if let Some(directory) = db_path.parent() {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = std::fs::create_dir_all(directory).and_then(|()| {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
        }) {
            eprintln!("aifuel: could not restrict the run history directory: {error}");
        }
    }
    let manager = match aifuel_app::RunStore::open(db_path) {
        Ok(store) => {
            if let Err(error) = store.import_sessions(session_store_path()?) {
                eprintln!("aifuel: could not import legacy session store: {error}");
            }
            manager.with_run_store(store)
        }
        Err(error) => {
            eprintln!("aifuel: run history store is unavailable: {error}");
            manager
        }
    };
    #[cfg(any(unix, windows))]
    let manager = manager.with_local_approval_channel(approval_owner_directory()?)?;
    if config.policy.retain_content {
        manager.with_content_store(content_store_path()?)
    } else {
        Ok(manager)
    }
}

pub fn execution_config_path() -> Result<PathBuf, String> {
    let home = user_home_dir()?;
    Ok(user_config_dir(&home)?
        .join("aifuel")
        .join("execution.json"))
}

pub fn session_store_path() -> Result<PathBuf, String> {
    let home = user_home_dir()?;
    Ok(user_config_dir(&home)?
        .join("aifuel")
        .join("agent-sessions.json"))
}

pub fn run_store_path() -> Result<PathBuf, String> {
    let home = user_home_dir()?;
    Ok(user_config_dir(&home)?.join("aifuel").join("aifuel.db"))
}

pub fn content_store_path() -> Result<PathBuf, String> {
    let home = user_home_dir()?;
    Ok(user_config_dir(&home)?.join("aifuel").join("run-content"))
}

pub fn approval_owner_directory() -> Result<PathBuf, String> {
    let home = user_home_dir()?;
    Ok(user_config_dir(&home)?
        .join("aifuel")
        .join("approval-owners"))
}

#[cfg(any(unix, windows))]
pub fn submit_local_approval(
    run_id: &str,
    input_id: &str,
    decision: aifuel_app::LocalApprovalDecision,
) -> Result<(), String> {
    aifuel_app::submit_local_approval(&approval_owner_directory()?, run_id, input_id, decision)
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
            integration: Some(request.integration.clone()),
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

pub(crate) fn user_home_dir() -> Result<PathBuf, String> {
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
pub(crate) fn user_config_dir(_user_home: &std::path::Path) -> Result<PathBuf, String> {
    env::var_os("APPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            "the user's application configuration directory could not be resolved".to_owned()
        })
}

#[cfg(target_os = "macos")]
pub(crate) fn user_config_dir(user_home: &std::path::Path) -> Result<PathBuf, String> {
    Ok(user_home.join("Library").join("Application Support"))
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn user_config_dir(user_home: &std::path::Path) -> Result<PathBuf, String> {
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        && config_home.is_absolute()
    {
        return Ok(config_home);
    }
    Ok(user_home.join(".config"))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn user_config_dir(_user_home: &std::path::Path) -> Result<PathBuf, String> {
    Err("the user's application configuration directory could not be resolved".to_owned())
}
