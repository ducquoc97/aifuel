//! Global Agent Run selection, profile, and model-catalog workflows.
//!
//! This module deliberately contains preferences and evidence only. It does
//! not launch a provider, read credentials, or persist prompts and answers.

mod catalog;
mod config;
mod resolver;
mod store;

pub use catalog::{
    AccountContext, CatalogEvidenceStore, CatalogFreshness, CatalogLookup, CatalogModel,
    CatalogProvenance, CatalogRefreshResult, CatalogScope, CatalogSnapshot, EffortEvidence,
    MODEL_CATALOG_TTL, ModelCatalogError, ModelEvidenceState,
};
pub use config::{
    ContentRetention, ExecutionPolicy, GLOBAL_SELECTION_SCHEMA_VERSION, GlobalSelectionConfig,
    ProfileSettings, SelectionSettings,
};
pub use resolver::{
    ResolvedSelection, SelectionError, SelectionInputs, SelectionResolver, SelectionSource,
    SelectionSources, StoredSession,
};
pub use store::{SelectionStore, SelectionStoreError};
