use aifuel_core::{AccessMode, ProviderKey};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const GLOBAL_SELECTION_SCHEMA_VERSION: u32 = 1;

/// Whether AI Fuel may retain prompt, answer, tool, and diagnostic content.
///
/// The global configuration stores this policy as a boolean so that the
/// default remains obvious in a hand-edited JSON file. The enum is useful to
/// callers that want to describe the policy without carrying a boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentRetention {
    ConnectionOnly,
    Persistent,
}

impl ContentRetention {
    pub const fn from_retain_content(retain_content: bool) -> Self {
        if retain_content {
            Self::Persistent
        } else {
            Self::ConnectionOnly
        }
    }

    pub const fn retains_content(self) -> bool {
        matches!(self, Self::Persistent)
    }
}

/// Local policy consumed by the execution CLI and execution MCP entrypoint.
///
/// `allowed_roots` is intentionally empty by default. An empty list means no
/// repository path is admitted by the execution MCP policy; it does not grant
/// access to arbitrary paths. This type contains policy metadata only and
/// never contains credentials or run content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExecutionPolicy {
    #[serde(default)]
    pub allowed_roots: Vec<PathBuf>,
    #[serde(default)]
    pub retain_content: bool,
}

impl ExecutionPolicy {
    pub const fn content_retention(&self) -> ContentRetention {
        ContentRetention::from_retain_content(self.retain_content)
    }
}

/// A partial set of settings. Missing values inherit from the next lower
/// precedence source, or from the provider/native default when no source has a
/// value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionSettings {
    #[serde(default, deserialize_with = "deserialize_provider")]
    pub provider: Option<ProviderKey>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default, deserialize_with = "deserialize_access")]
    pub access: Option<AccessMode>,
    /// An optional overall deadline represented as whole seconds in the
    /// metadata file. Absence means no overall deadline.
    #[serde(default)]
    pub overall_deadline_seconds: Option<u64>,
}

/// Profile values use the same partial settings shape as global defaults.
pub type ProfileSettings = SelectionSettings;

impl SelectionSettings {
    pub fn with_provider(provider: ProviderKey) -> Self {
        Self {
            provider: Some(provider),
            ..Self::default()
        }
    }

    pub fn merge_over(&self, lower: &Self) -> Self {
        Self {
            provider: self.provider.or(lower.provider),
            model: self.model.clone().or_else(|| lower.model.clone()),
            effort: self.effort.clone().or_else(|| lower.effort.clone()),
            access: self.access.or(lower.access),
            overall_deadline_seconds: self
                .overall_deadline_seconds
                .or(lower.overall_deadline_seconds),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.provider.is_none()
            && self.model.is_none()
            && self.effort.is_none()
            && self.access.is_none()
            && self.overall_deadline_seconds.is_none()
    }
}

/// The private global configuration for selection and local execution policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalSelectionConfig {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub defaults: SelectionSettings,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileSettings>,
    #[serde(default)]
    pub policy: ExecutionPolicy,
}

impl Default for GlobalSelectionConfig {
    fn default() -> Self {
        Self {
            schema_version: GLOBAL_SELECTION_SCHEMA_VERSION,
            defaults: SelectionSettings::default(),
            profiles: BTreeMap::new(),
            policy: ExecutionPolicy::default(),
        }
    }
}

fn default_schema_version() -> u32 {
    GLOBAL_SELECTION_SCHEMA_VERSION
}

fn deserialize_provider<'de, D>(deserializer: D) -> Result<Option<ProviderKey>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)?
        .map(|value| value.parse().map_err(serde::de::Error::custom))
        .transpose()
}

fn deserialize_access<'de, D>(deserializer: D) -> Result<Option<AccessMode>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)?
        .map(|value| AccessMode::parse(&value).map_err(serde::de::Error::custom))
        .transpose()
}
