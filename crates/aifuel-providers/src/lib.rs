//! Built-in Provider Discovery, quota monitoring, and Agent Run adapters.
//!
//! Discovery inspects provider-owned source metadata without reading its
//! contents. Monitoring adapters read credentials only for an explicit
//! collection and never write user state. A non-rotating refresh grant may be
//! exchanged in memory (Google), or the provider CLI may renew its own
//! credentials (Codex app-server); rotating refresh grants are never consumed.
//! Agent Runs use separate provider execution capabilities and require an
//! explicit provider selection.

pub mod acp_runtime;
mod agent_execution;
mod agent_run_registry;
mod antigravity;
mod catalog;
mod claude;
pub mod claude_runtime;
mod claude_web;
mod cli_adapter;
mod code_assist;
mod codex;
mod codex_oauth;
pub mod codex_runtime;
mod copilot;
mod copilot_oauth;
mod credentials;
mod deepseek;
mod devin;
mod devin_oauth;
mod discovery;
mod embeddings;
mod forward;
mod integrations;
mod local_adapter;
mod model_catalog;
mod monitoring;
pub mod oauth;
pub mod opencode_runtime;
mod openrouter;
mod quota;
mod registration;
mod registry;
mod siliconflow;
mod usage_helpers;
mod wire;
mod zai;

pub use acp_runtime::{AcpAdapter, acp_adapter};
pub use agent_run_registry::{agent_run_adapters, oauth_execution_adapter};
pub use catalog::free_tier_note;
pub use claude_runtime::{ClaudeAdapter, claude_adapter};
pub use cli_adapter::{
    AdapterDiscovery, CliAdapter, cli_fallback_adapters, integration_summary, quota_summary,
};
pub use codex_runtime::{CodexAdapter, codex_adapter};
pub use credentials::{
    ApiKeyState, CredentialExpiry, CredentialKind, CredentialMetadata, CredentialStore,
    CredentialStoreError, KeyHealth, ManagedCredential, OAuthTokens, PoolKey, ResolvedAuth,
    env_override, is_pool_member, valid_env_var_name,
};
pub use discovery::{DiscoveryContext, DiscoveryContextError};
pub use embeddings::{EmbeddingsOutcome, embeddings};
pub use forward::{ForwardOutcome, post_json, post_raw};
pub use integrations::{
    ChainDescriptor, ChainStep, ChainStrategy, ConfigError, EvidenceContext, EvidenceSource,
    InstanceDescriptor, InstanceEnvSource, IntegrationDescriptor, IntegrationOrigin,
    IntegrationRegistry, PROVIDERS_FILE_NAME, PROVIDERS_SCHEMA_VERSION, ProvidersConfig,
    ProvidersConfigParts, RegistryError, ResolveError, builtin_integrations, edit_instances,
    inspect_any,
};
pub use model_catalog::{ProviderCatalogDiscovery, ProviderCatalogModel, discover_model_catalog};
pub use monitoring::{CollectionConfig, ProviderMonitoring};
pub use opencode_runtime::{OpenCodeAdapter, opencode_adapter};
pub(crate) use registration::codex_mcp_runtime_entry;
pub use registration::{agent_mcp_registration_adapter, agent_mcp_registration_host_ids};
pub use wire::{WireAdapterError, WireExecutionAdapter, serves as wire_serves};

pub(crate) use registry::{
    CatalogProvider, MonitoringFuture, default_monitoring_registry, default_registry,
};
