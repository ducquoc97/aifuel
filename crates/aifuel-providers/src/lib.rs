//! Built-in Provider Discovery and read-only quota collection.
//!
//! Discovery inspects provider-owned source metadata without reading its
//! contents. Provider adapters read credentials only for an explicit
//! collection and never refresh or write user state.

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

pub use discovery::{DiscoveryContext, DiscoveryContextError};
pub use monitoring::{CollectionConfig, ProviderMonitoring};

pub(crate) use registry::{
    CatalogProvider, MonitoringFuture, default_monitoring_registry, default_registry,
};
