//! Built-in Provider Discovery, quota monitoring, and Agent Run adapters.
//!
//! Discovery inspects provider-owned source metadata without reading its
//! contents. Monitoring adapters read credentials only for an explicit
//! collection and never write user state. A non-rotating refresh grant may be
//! exchanged in memory (Google), or the provider CLI may renew its own
//! credentials (Codex app-server); rotating refresh grants are never consumed.
//! Agent Runs use separate provider execution capabilities and require an
//! explicit provider selection.

mod agent_execution;
mod agent_run_registry;
mod antigravity;
mod catalog;
mod claude;
mod code_assist;
mod codex;
mod copilot;
mod credentials;
mod devin;
mod discovery;
mod gemini;
mod integrations;
mod model_catalog;
mod monitoring;
mod openrouter;
mod registration;
mod registry;
mod usage_helpers;
mod wire;

pub use agent_run_registry::agent_run_adapters;
pub use credentials::{
    CredentialExpiry, CredentialKind, CredentialMetadata, CredentialStore, CredentialStoreError,
    ManagedCredential, OAuthTokens, ResolvedAuth, env_override, valid_env_var_name,
};
pub use discovery::{DiscoveryContext, DiscoveryContextError};
pub use integrations::{
    ConfigError, EvidenceContext, EvidenceSource, IntegrationDescriptor, IntegrationOrigin,
    IntegrationRegistry, PROVIDERS_FILE_NAME, PROVIDERS_SCHEMA_VERSION, ProvidersConfig,
    RegistryError, ResolveError, builtin_integrations, inspect_any,
};
pub use model_catalog::{ProviderCatalogDiscovery, ProviderCatalogModel, discover_model_catalog};
pub use monitoring::{CollectionConfig, ProviderMonitoring};
pub use registration::agent_mcp_registration_adapter;
pub(crate) use registration::codex_mcp_runtime_entry;
pub use wire::{WireAdapterError, WireExecutionAdapter};

pub(crate) use registry::{
    CatalogProvider, MonitoringFuture, default_monitoring_registry, default_registry,
};
