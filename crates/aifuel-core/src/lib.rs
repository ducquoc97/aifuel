//! Stable domain values shared by AI Fuel applications and provider adapters.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;

mod agent_mcp_registration;
mod execution;
mod run_management;
mod status;
pub use agent_mcp_registration::{
    AIFUEL_GATEWAY_REGISTRATION_NAME, AgentMcpRegistrationAdapter, AgentMcpRegistrationError,
};
pub use execution::{
    AccessMode, AgentExecutionAdapter, AgentRunError, ExecutionMode, OutputFormat,
    RunCancellationToken, RunRequest, RunResult, RunStatus,
};
pub use run_management::{
    DEFAULT_EVENT_PAGE_BYTES, MAX_ACTIVE_RUNS, MAX_ANSWER_BYTES_PER_RUN, MAX_COMPLETED_CONTENT,
    MAX_EVENT_BYTES_PER_RUN, MAX_EVENT_PAGE_BYTES, MAX_OWNER_CONTENT_BYTES, MAX_RUN_RECORDS,
    ManagedRun, ManagedRunResult, PendingRunInput, RUN_MANAGEMENT_SCHEMA_VERSION, ResolvedRun,
    RunEvent, RunEventKind, RunEvents, RunInputKind, RunManagementError, RunManagementErrorCode,
    RunState,
};
pub use status::{
    CapabilityKind, CapabilityState, CatalogPlatformStatus, CatalogProviderStatus,
    CollectionOutcome, CollectionScope, CollectionState, CollectionStatus, FreshnessState,
    ModelState, ObservationState, Provenance, STATUS_SCHEMA_VERSION, StatusAccount,
    StatusCapability, StatusEntitlement, StatusError, StatusErrorCode, StatusModel,
    StatusObservation, StatusQuotaPool, StatusReport,
};

/// The schema version for the initial Rust discovery output.
pub const DISCOVERY_SCHEMA_VERSION: u32 = 1;

/// The read-only monitoring boundary consumed by AI Fuel application workflows.
///
/// Implementations collect a fresh normalized status report. Caching and
/// interface-specific presentation remain outside provider adapters.
pub trait StatusCollector: Send + Sync {
    fn collect_status(&self) -> Pin<Box<dyn Future<Output = StatusReport> + Send + '_>>;
}

/// A provider represented in the built-in Catalog Provider catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKey {
    Claude,
    Codex,
    Copilot,
    Gemini,
    Antigravity,
}

impl ProviderKey {
    /// The keys in the stable catalog order used by the current application.
    pub const ALL: [Self; 5] = [
        Self::Claude,
        Self::Codex,
        Self::Copilot,
        Self::Gemini,
        Self::Antigravity,
    ];

    /// The stable serialized key for this provider.
    pub const fn as_str(self) -> &'static str {
        self.metadata().key
    }

    /// The user-facing provider name.
    pub const fn display_name(self) -> &'static str {
        self.metadata().name
    }

    const fn metadata(self) -> ProviderMetadata {
        match self {
            Self::Claude => ProviderMetadata {
                key: "claude",
                name: "Claude Code",
            },
            Self::Codex => ProviderMetadata {
                key: "codex",
                name: "Codex CLI",
            },
            Self::Copilot => ProviderMetadata {
                key: "copilot",
                name: "GitHub Copilot",
            },
            Self::Gemini => ProviderMetadata {
                key: "gemini",
                name: "Gemini CLI",
            },
            Self::Antigravity => ProviderMetadata {
                key: "antigravity",
                name: "Antigravity CLI",
            },
        }
    }
}

struct ProviderMetadata {
    key: &'static str,
    name: &'static str,
}

impl fmt::Display for ProviderKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProviderKey {
    type Err = InvalidProviderKey;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "copilot" => Ok(Self::Copilot),
            "gemini" => Ok(Self::Gemini),
            "antigravity" => Ok(Self::Antigravity),
            _ => Err(InvalidProviderKey(value.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidProviderKey(String);

impl fmt::Display for InvalidProviderKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown provider {:?}", self.0)
    }
}

impl std::error::Error for InvalidProviderKey {}

/// The stable identity and display name of a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ProviderDescriptor {
    pub key: ProviderKey,
    pub name: &'static str,
}

impl ProviderDescriptor {
    /// Construct the canonical descriptor for a provider key.
    pub const fn for_key(key: ProviderKey) -> Self {
        Self {
            key,
            name: key.display_name(),
        }
    }
}

/// The result of a local Provider Discovery check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryState {
    /// The provider-specific credential source exists locally.
    Present,
    /// The provider-specific credential source does not exist locally.
    Absent,
}

/// A safe, application-level reason that Provider Discovery could not finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryError {
    /// The provider-specific source could not be inspected.
    SourceUnavailable,
    /// A source exists but is not the expected filesystem shape.
    UnexpectedSourceType,
}

impl DiscoveryError {
    /// A user-facing explanation that contains no path or credential data.
    pub const fn detail(self) -> &'static str {
        match self {
            Self::SourceUnavailable => "provider credential source could not be inspected",
            Self::UnexpectedSourceType => {
                "provider credential source has an unexpected filesystem type"
            }
        }
    }
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.detail())
    }
}

impl std::error::Error for DiscoveryError {}

/// A discovery check that could not determine whether a provider is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DiscoveryFailure {
    pub provider: ProviderDescriptor,
    pub detail: &'static str,
}

impl DiscoveryFailure {
    /// Convert an internal discovery error into the safe public result form.
    pub const fn from_error(provider: ProviderDescriptor, error: DiscoveryError) -> Self {
        Self {
            provider,
            detail: error.detail(),
        }
    }
}

/// The provider set and independent discovery diagnostics for one collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscoveryReport {
    /// Only providers whose local source was present and initialized.
    pub providers: Vec<ProviderDescriptor>,
    /// Providers whose source could not be inspected. They are excluded above.
    pub discovery_errors: Vec<DiscoveryFailure>,
}

impl DiscoveryReport {
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
            discovery_errors: Vec::new(),
        }
    }
}

impl Default for DiscoveryReport {
    fn default() -> Self {
        Self::new()
    }
}

/// A quota period reported by a provider.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuotaWindow {
    pub label: String,
    pub period: String,
    pub used_percent: Option<f64>,
    pub remaining_percent: Option<f64>,
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub resets_at: Option<f64>,
}

impl QuotaWindow {
    pub fn new(
        label: impl Into<String>,
        period: impl Into<String>,
        used_percent: Option<f64>,
        remaining_percent: Option<f64>,
        resets_at: Option<f64>,
    ) -> Self {
        let used_percent = used_percent.map(|value| value.clamp(0.0, 100.0));
        let remaining_percent = remaining_percent
            .or_else(|| used_percent.map(|value| 100.0 - value))
            .map(|value| value.clamp(0.0, 100.0));
        let used_percent = used_percent.or_else(|| remaining_percent.map(|value| 100.0 - value));
        Self {
            label: label.into(),
            period: period.into(),
            used_percent,
            remaining_percent,
            used: None,
            limit: None,
            resets_at,
        }
    }
}

/// One provider's normalized usage result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderUsage {
    pub key: ProviderKey,
    pub name: &'static str,
    pub status: ProviderStatus,
    pub plan: Option<String>,
    pub account_id: Option<String>,
    pub source: Option<String>,
    pub detail: Option<String>,
    pub windows: Vec<QuotaWindow>,
    pub reset_at: Option<f64>,
    pub reset_credits: Option<ResetCredits>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStatus {
    Ok,
    Error,
}

impl fmt::Display for ProviderStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => f.write_str("ok"),
            Self::Error => f.write_str("error"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResetCredits {
    pub available_count: u64,
    pub credits: Vec<ResetCredit>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResetCredit {
    pub reset_type: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub expires_at: Option<f64>,
}

impl ProviderUsage {
    pub fn error(key: ProviderKey, detail: impl Into<String>) -> Self {
        Self {
            key,
            name: key.display_name(),
            status: ProviderStatus::Error,
            plan: None,
            account_id: None,
            source: None,
            detail: Some(detail.into()),
            windows: Vec::new(),
            reset_at: None,
            reset_credits: None,
        }
    }

    pub fn success(key: ProviderKey, windows: Vec<QuotaWindow>) -> Self {
        let reset_at = windows
            .iter()
            .filter_map(|window| window.resets_at)
            .min_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
        Self {
            key,
            name: key.display_name(),
            status: ProviderStatus::Ok,
            plan: None,
            account_id: None,
            source: Some("live".to_owned()),
            detail: None,
            windows,
            reset_at,
            reset_credits: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_keys_have_stable_identity_and_names() {
        assert_eq!(ProviderKey::ALL.len(), 5);
        assert_eq!(ProviderKey::Claude.as_str(), "claude");
        assert_eq!(ProviderKey::Copilot.display_name(), "GitHub Copilot");
        assert_eq!(
            ProviderDescriptor::for_key(ProviderKey::Gemini),
            ProviderDescriptor {
                key: ProviderKey::Gemini,
                name: "Gemini CLI",
            }
        );
    }

    #[test]
    fn discovery_failure_has_safe_detail() {
        let failure = DiscoveryFailure::from_error(
            ProviderDescriptor::for_key(ProviderKey::Claude),
            DiscoveryError::SourceUnavailable,
        );

        assert_eq!(failure.provider.key, ProviderKey::Claude);
        assert_eq!(
            failure.detail,
            "provider credential source could not be inspected"
        );
    }
}
