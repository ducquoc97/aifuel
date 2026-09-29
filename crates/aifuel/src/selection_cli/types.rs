//! The picker's public data types: the model and integration entries it
//! renders, the options a caller supplies, and the selection/error values it
//! returns. Kept separate so `selection_cli.rs` reads as interaction flow.

use aifuel_app::selection::{CatalogModel, CatalogProvenance, SelectionSettings};
use aifuel_core::{CapabilityState, IntegrationId, ProviderId, ProviderKey};
use std::fmt;
use std::io;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerModel {
    pub provider: ProviderKey,
    pub model_id: String,
    pub display_label: Option<String>,
    pub provenance: CatalogProvenance,
    pub advertisement: CapabilityState,
    pub entitlement: CapabilityState,
    pub execution: CapabilityState,
    pub scope_label: Option<String>,
    pub catalog_age_seconds: u64,
    pub default_effort: Option<String>,
    pub effort_values: Vec<String>,
    pub effort_state: CapabilityState,
}

impl From<&CatalogModel> for PickerModel {
    fn from(model: &CatalogModel) -> Self {
        Self {
            provider: model.provider,
            model_id: model.model_id.clone(),
            display_label: model.display_label.clone(),
            provenance: model.provenance,
            advertisement: model.advertisement,
            entitlement: model.entitlement,
            execution: model.execution,
            scope_label: None,
            catalog_age_seconds: 0,
            default_effort: model.default_effort.clone(),
            effort_values: model.efforts.values.clone(),
            effort_state: model.efforts.state,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerModelEvidence {
    Catalog,
    ExplicitOverrideUnknown,
}

/// One configured integration the picker may select. The upstream provider id
/// is retained so catalog models can be filtered by provider while routing
/// still uses the opaque integration id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerIntegration {
    pub id: IntegrationId,
    pub provider: ProviderId,
}

impl PickerIntegration {
    pub fn new(id: IntegrationId, provider: ProviderId) -> Self {
        Self { id, provider }
    }

    /// The catalog provider key when the upstream provider is a known catalog
    /// entry. Non-catalog providers yield no catalog models.
    pub(super) fn provider_key(&self) -> Option<ProviderKey> {
        self.provider.as_str().parse().ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerOptions {
    pub integrations: Vec<PickerIntegration>,
    pub models: Vec<PickerModel>,
    pub initial: SelectionSettings,
    pub working_directory: Option<PathBuf>,
}

impl Default for PickerOptions {
    fn default() -> Self {
        Self {
            integrations: ProviderKey::ALL
                .iter()
                .map(|provider| {
                    PickerIntegration::new(
                        IntegrationId::from(*provider),
                        ProviderId::from(*provider),
                    )
                })
                .collect(),
            models: Vec::new(),
            initial: SelectionSettings::default(),
            working_directory: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerSelection {
    pub settings: SelectionSettings,
    pub working_directory: Option<PathBuf>,
    pub model_evidence: PickerModelEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerOutcome {
    Cancelled,
}

#[derive(Debug)]
pub enum PickerError {
    NotInteractive,
    Io(io::Error),
    InvalidInput(String),
}

impl fmt::Display for PickerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInteractive => f.write_str("selection requires an interactive terminal"),
            Self::Io(error) => write!(f, "selection picker I/O failed: {error}"),
            Self::InvalidInput(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PickerError {}

impl From<io::Error> for PickerError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
