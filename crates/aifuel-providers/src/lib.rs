//! Built-in Provider Discovery, quota monitoring, and Agent Run adapters.
//!
//! Discovery inspects provider-owned source metadata without reading its
//! contents. Monitoring adapters read credentials only for an explicit
//! collection and never refresh or write user state. Agent Runs use separate
//! provider execution capabilities and require an explicit provider selection.

mod agent_execution;
mod agent_run_registry;
mod antigravity;
mod catalog;
mod claude;
mod code_assist;
mod codex;
mod copilot;
mod discovery;
mod gemini;
mod monitoring;
mod registry;
mod usage_helpers;

pub use agent_run_registry::agent_run_adapters;
pub use discovery::{DiscoveryContext, DiscoveryContextError};
pub use monitoring::{CollectionConfig, ProviderMonitoring};

pub(crate) use registry::{
    CatalogProvider, MonitoringFuture, default_monitoring_registry, default_registry,
};
