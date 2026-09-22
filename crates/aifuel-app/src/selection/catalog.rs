use aifuel_core::{CapabilityState, ProviderKey};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const MODEL_CATALOG_TTL: Duration = Duration::from_secs(5 * 60);
const MODEL_CATALOG_SCHEMA_VERSION: u32 = 1;

pub use aifuel_core::CapabilityState as ModelEvidenceState;

/// Account or billing context associated with catalog evidence.
///
/// Unknown identity is represented explicitly and never treated as equal to a
/// known account. This prevents a run-tested observation from becoming
/// entitlement evidence after the user changes login context.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AccountContext {
    Known(String),
    Unknown,
}

impl AccountContext {
    pub fn known(value: impl Into<String>) -> Self {
        Self::Known(value.into())
    }

    pub const fn unknown() -> Self {
        Self::Unknown
    }

    pub fn as_known(&self) -> Option<&str> {
        match self {
            Self::Known(value) => Some(value),
            Self::Unknown => None,
        }
    }
}

impl From<Option<String>> for AccountContext {
    fn from(value: Option<String>) -> Self {
        match value {
            Some(value) => Self::Known(value),
            None => Self::Unknown,
        }
    }
}

/// The context that scopes model and execution evidence. Unknown version or
/// platform values are kept unknown rather than being collapsed into a known
/// context.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CatalogScope {
    #[serde(deserialize_with = "deserialize_provider")]
    pub provider: ProviderKey,
    pub integration_version: Option<String>,
    pub platform: Option<String>,
    pub account: AccountContext,
}

impl CatalogScope {
    pub fn new(
        provider: ProviderKey,
        integration_version: Option<String>,
        platform: Option<String>,
        account: AccountContext,
    ) -> Self {
        Self {
            provider,
            integration_version,
            platform,
            account,
        }
    }

    pub fn reported(
        provider: ProviderKey,
        integration_version: impl Into<String>,
        platform: impl Into<String>,
        account: Option<String>,
    ) -> Self {
        Self::new(
            provider,
            Some(integration_version.into()),
            Some(platform.into()),
            account.into(),
        )
    }

    pub fn unknown_context(provider: ProviderKey) -> Self {
        Self::new(provider, None, None, AccountContext::Unknown)
    }
}

/// Where one model observation came from. No enum value means that a model is
/// supported; callers must supply independent state observations explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogProvenance {
    NativeInterface,
    ProviderApi,
    CompiledAdapter,
    UserOverride,
}

/// Independent evidence for model-specific effort values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffortEvidence {
    pub state: CapabilityState,
    #[serde(default)]
    pub values: Vec<String>,
}

impl EffortEvidence {
    pub fn unknown() -> Self {
        Self {
            state: CapabilityState::Unknown,
            values: Vec::new(),
        }
    }

    pub fn known(values: impl IntoIterator<Item = String>) -> Self {
        Self {
            state: CapabilityState::Supported,
            values: values.into_iter().collect(),
        }
    }
}

/// A catalog entry for one exact provider model identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogModel {
    #[serde(deserialize_with = "deserialize_provider")]
    pub provider: ProviderKey,
    pub model_id: String,
    pub display_label: Option<String>,
    pub provenance: CatalogProvenance,
    pub discovered_at: u64,
    pub efforts: EffortEvidence,
    pub advertisement: CapabilityState,
    pub entitlement: CapabilityState,
    pub execution: CapabilityState,
}

impl CatalogModel {
    pub fn new(
        provider: ProviderKey,
        model_id: impl Into<String>,
        provenance: CatalogProvenance,
        discovered_at: u64,
    ) -> Self {
        Self {
            provider,
            model_id: model_id.into(),
            display_label: None,
            provenance,
            discovered_at,
            efforts: EffortEvidence::unknown(),
            advertisement: CapabilityState::Unknown,
            entitlement: CapabilityState::Unknown,
            execution: CapabilityState::Unknown,
        }
    }

    pub fn with_display_label(mut self, display_label: impl Into<String>) -> Self {
        self.display_label = Some(display_label.into());
        self
    }

    pub fn with_efforts(mut self, efforts: EffortEvidence) -> Self {
        self.efforts = efforts;
        self
    }

    pub fn unknown(provider: ProviderKey, model_id: impl Into<String>, discovered_at: u64) -> Self {
        Self::new(
            provider,
            model_id,
            CatalogProvenance::UserOverride,
            discovered_at,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogFreshness {
    Fresh,
    Stale,
}

/// A successful model catalog and its scoped refresh timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogSnapshot {
    pub scope: CatalogScope,
    pub models: Vec<CatalogModel>,
    pub refreshed_at: u64,
    pub freshness: CatalogFreshness,
}

impl CatalogSnapshot {
    pub fn is_stale(&self) -> bool {
        self.freshness == CatalogFreshness::Stale
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogLookup {
    Fresh(CatalogSnapshot),
    Stale {
        snapshot: CatalogSnapshot,
        age: Duration,
    },
    Unknown {
        scope: CatalogScope,
    },
}

impl CatalogLookup {
    pub fn snapshot(&self) -> Option<&CatalogSnapshot> {
        match self {
            Self::Fresh(snapshot) => Some(snapshot),
            Self::Stale { snapshot, .. } => Some(snapshot),
            Self::Unknown { .. } => None,
        }
    }

    pub const fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown { .. })
    }
}

/// Outcome of an attempted catalog refresh. A failed refresh never replaces
/// an existing successful catalog with an empty one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogRefreshResult {
    Updated(CatalogSnapshot),
    Failed {
        scope: CatalogScope,
        retained: Option<CatalogSnapshot>,
        error: String,
    },
}

#[derive(Debug, Clone)]
pub struct CatalogEvidenceStore {
    snapshots: BTreeMap<CatalogScope, CatalogSnapshot>,
    ttl: Duration,
}

impl Default for CatalogEvidenceStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CatalogEvidenceStore {
    pub fn new() -> Self {
        Self {
            snapshots: BTreeMap::new(),
            ttl: MODEL_CATALOG_TTL,
        }
    }

    pub fn with_ttl(ttl: Duration) -> Self {
        Self { ttl, ..Self::new() }
    }

    pub const fn ttl(&self) -> Duration {
        self.ttl
    }

    pub fn scopes(&self) -> impl Iterator<Item = &CatalogScope> {
        self.snapshots.keys()
    }

    pub fn snapshot(&self, scope: &CatalogScope) -> Option<&CatalogSnapshot> {
        self.snapshots.get(scope)
    }

    pub fn lookup(&self, scope: &CatalogScope) -> CatalogLookup {
        self.lookup_at(scope, SystemTime::now())
    }

    pub fn lookup_at(&self, scope: &CatalogScope, now: SystemTime) -> CatalogLookup {
        let Some(snapshot) = self.snapshots.get(scope) else {
            return CatalogLookup::Unknown {
                scope: scope.clone(),
            };
        };
        let now = epoch_seconds(now);
        let age_seconds = now.saturating_sub(snapshot.refreshed_at);
        let age = Duration::from_secs(age_seconds);
        if snapshot.freshness == CatalogFreshness::Fresh && age <= self.ttl {
            let mut snapshot = snapshot.clone();
            snapshot.freshness = CatalogFreshness::Fresh;
            CatalogLookup::Fresh(snapshot)
        } else {
            let mut snapshot = snapshot.clone();
            snapshot.freshness = CatalogFreshness::Stale;
            CatalogLookup::Stale { snapshot, age }
        }
    }

    pub fn refresh<F, E>(&mut self, scope: CatalogScope, fetch: F) -> CatalogRefreshResult
    where
        F: FnOnce() -> Result<Vec<CatalogModel>, E>,
        E: fmt::Display,
    {
        self.refresh_at(scope, SystemTime::now(), fetch)
    }

    pub fn refresh_at<F, E>(
        &mut self,
        scope: CatalogScope,
        now: SystemTime,
        fetch: F,
    ) -> CatalogRefreshResult
    where
        F: FnOnce() -> Result<Vec<CatalogModel>, E>,
        E: fmt::Display,
    {
        match fetch() {
            Ok(models) => {
                let snapshot = CatalogSnapshot {
                    scope: scope.clone(),
                    models,
                    refreshed_at: epoch_seconds(now),
                    freshness: CatalogFreshness::Fresh,
                };
                self.snapshots.insert(scope, snapshot.clone());
                CatalogRefreshResult::Updated(snapshot)
            }
            Err(error) => {
                if let Some(snapshot) = self.snapshots.get_mut(&scope) {
                    snapshot.freshness = CatalogFreshness::Stale;
                }
                CatalogRefreshResult::Failed {
                    scope: scope.clone(),
                    retained: self.snapshots.get(&scope).cloned(),
                    error: error.to_string(),
                }
            }
        }
    }

    /// Load the last successful catalogs from a caller-supplied cache path.
    /// Missing cache files are represented as an empty, unknown store.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ModelCatalogError> {
        let path = path.as_ref();
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::new()),
            Err(error) => return Err(ModelCatalogError::Io(error)),
        };
        let persisted: PersistedCatalog = serde_json::from_slice(&bytes)?;
        if persisted.schema_version != MODEL_CATALOG_SCHEMA_VERSION {
            return Err(ModelCatalogError::UnsupportedSchema(
                persisted.schema_version,
            ));
        }
        let mut store = Self::new();
        for snapshot in persisted.snapshots {
            store.snapshots.insert(snapshot.scope.clone(), snapshot);
        }
        Ok(store)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ModelCatalogError> {
        let path = path.as_ref();
        let parent = path
            .parent()
            .filter(|directory| !directory.as_os_str().is_empty());
        if let Some(parent) = parent {
            fs::create_dir_all(parent)?;
        }
        let persisted = PersistedCatalog {
            schema_version: MODEL_CATALOG_SCHEMA_VERSION,
            snapshots: self.snapshots.values().cloned().collect(),
        };
        let bytes = serde_json::to_vec_pretty(&persisted)?;
        fs::write(path, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedCatalog {
    schema_version: u32,
    snapshots: Vec<CatalogSnapshot>,
}

#[derive(Debug)]
pub enum ModelCatalogError {
    Io(io::Error),
    Json(serde_json::Error),
    UnsupportedSchema(u32),
}

impl fmt::Display for ModelCatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "model catalog I/O failed: {error}"),
            Self::Json(error) => write!(f, "invalid model catalog JSON: {error}"),
            Self::UnsupportedSchema(version) => {
                write!(f, "unsupported model catalog schema version {version}")
            }
        }
    }
}

impl std::error::Error for ModelCatalogError {}

impl From<io::Error> for ModelCatalogError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for ModelCatalogError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

fn epoch_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn deserialize_provider<'de, D>(deserializer: D) -> Result<ProviderKey, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    value.parse().map_err(serde::de::Error::custom)
}
