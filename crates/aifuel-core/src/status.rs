use super::{DiscoveryFailure, ProviderKey, ProviderStatus, ProviderUsage};
use serde::Serialize;

pub const STATUS_SCHEMA_VERSION: u32 = 1;

/// The normalized status result shared by text, JSON, dashboard, and MCP.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusReport {
    pub schema_version: u32,
    pub generated_at: f64,
    pub collection: CollectionStatus,
    pub providers: Vec<ProviderUsage>,
    pub catalog: Vec<CatalogProviderStatus>,
    pub accounts: Vec<StatusAccount>,
    pub capabilities: Vec<StatusCapability>,
    pub models: Vec<StatusModel>,
    pub entitlements: Vec<StatusEntitlement>,
    pub quota_pools: Vec<StatusQuotaPool>,
    pub observations: Vec<StatusObservation>,
    pub discovery_errors: Vec<DiscoveryFailure>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CollectionStatus {
    pub state: CollectionState,
    pub outcome: Option<CollectionOutcome>,
    pub scope: CollectionScope,
    pub coverage: Vec<String>,
    pub errors: Vec<StatusError>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CollectionScope {
    pub provider_id: Option<String>,
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusError {
    pub provider_id: Option<String>,
    pub account_id: Option<String>,
    pub code: StatusErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusAccount {
    pub id: String,
    pub provider_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusCapability {
    pub provider_id: String,
    pub account_id: Option<String>,
    pub capability: CapabilityKind,
    pub state: CapabilityState,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CatalogProviderStatus {
    pub id: String,
    pub monitoring: CapabilityState,
    pub agent_execution: CapabilityState,
    pub evidence: String,
    pub platforms: Vec<CatalogPlatformStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CatalogPlatformStatus {
    pub platform: String,
    pub monitoring: CapabilityState,
    pub agent_execution: CapabilityState,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusModel {
    pub id: String,
    pub provider_id: String,
    pub account_id: Option<String>,
    pub quota_pool_id: String,
    pub state: ModelState,
    pub advertisement: CapabilityState,
    pub entitlement: CapabilityState,
    pub execution: CapabilityState,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusEntitlement {
    pub provider_id: String,
    pub account_id: Option<String>,
    pub model_id: String,
    pub state: CapabilityState,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusQuotaPool {
    pub id: String,
    pub provider_id: String,
    pub account_id: Option<String>,
    pub shared: bool,
    pub basis: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusObservation {
    pub id: String,
    pub provider_id: String,
    pub account_id: Option<String>,
    pub quota_pool_id: String,
    pub label: String,
    pub used_percent: Option<f64>,
    pub remaining_percent: Option<f64>,
    pub resets_at: Option<f64>,
    pub state: ObservationState,
    pub observed_at: Option<f64>,
    pub collected_at: f64,
    pub freshness: FreshnessState,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectionState {
    NotCollected,
    Collected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectionOutcome {
    Complete,
    Partial,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusErrorCode {
    DiscoveryFailure,
    CollectionFailed,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    ModelCatalog,
    AccountEntitlement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelState {
    Observed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationState {
    Known,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessState {
    Fresh,
    Stale,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    ProviderApi,
}

impl StatusReport {
    pub fn cold(generated_at: f64) -> Self {
        Self {
            schema_version: STATUS_SCHEMA_VERSION,
            generated_at,
            collection: CollectionStatus {
                state: CollectionState::NotCollected,
                outcome: None,
                scope: CollectionScope {
                    provider_id: None,
                    account_id: None,
                },
                coverage: Vec::new(),
                errors: Vec::new(),
            },
            providers: Vec::new(),
            catalog: Vec::new(),
            accounts: Vec::new(),
            capabilities: Vec::new(),
            models: Vec::new(),
            entitlements: Vec::new(),
            quota_pools: Vec::new(),
            observations: Vec::new(),
            discovery_errors: Vec::new(),
        }
    }

    pub fn from_usage(
        generated_at: f64,
        mut providers: Vec<ProviderUsage>,
        discovery_errors: Vec<DiscoveryFailure>,
    ) -> Self {
        let mut errors = discovery_errors
            .iter()
            .map(|failure| StatusError {
                provider_id: Some(failure.provider.key.as_str().to_owned()),
                account_id: None,
                code: StatusErrorCode::DiscoveryFailure,
                message: failure.detail.to_owned(),
            })
            .collect::<Vec<_>>();
        errors.extend(providers.iter().filter_map(|provider| {
            provider.detail.as_ref().map(|detail| StatusError {
                provider_id: Some(provider.key.as_str().to_owned()),
                account_id: provider.account_id.clone(),
                code: StatusErrorCode::CollectionFailed,
                message: detail.clone(),
            })
        }));
        let outcome = if providers
            .iter()
            .any(|provider| provider.status == ProviderStatus::Ok)
        {
            if errors.is_empty() {
                CollectionOutcome::Complete
            } else {
                CollectionOutcome::Partial
            }
        } else if providers.is_empty() && errors.is_empty() {
            CollectionOutcome::Complete
        } else {
            CollectionOutcome::Failed
        };
        providers.sort_by(|left, right| {
            let left_remaining = effective_remaining(left);
            let right_remaining = effective_remaining(right);
            (left_remaining <= 0.0)
                .cmp(&(right_remaining <= 0.0))
                .then_with(|| {
                    left.reset_at
                        .unwrap_or(f64::INFINITY)
                        .partial_cmp(&right.reset_at.unwrap_or(f64::INFINITY))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });
        let accounts = providers
            .iter()
            .filter_map(|provider| {
                provider
                    .account_id
                    .as_ref()
                    .map(|account_id| StatusAccount {
                        id: account_id.clone(),
                        provider_id: provider.key.as_str().to_owned(),
                    })
            })
            .collect();
        let capabilities = providers
            .iter()
            .flat_map(|provider| {
                [
                    StatusCapability {
                        provider_id: provider.key.as_str().to_owned(),
                        account_id: provider.account_id.clone(),
                        capability: CapabilityKind::ModelCatalog,
                        state: CapabilityState::Unknown,
                        reason:
                            "the quota endpoint does not establish an authoritative model catalog"
                                .to_owned(),
                    },
                    StatusCapability {
                        provider_id: provider.key.as_str().to_owned(),
                        account_id: provider.account_id.clone(),
                        capability: CapabilityKind::AccountEntitlement,
                        state: CapabilityState::Unknown,
                        reason: "quota observations do not establish account entitlement"
                            .to_owned(),
                    },
                ]
            })
            .collect();
        let models: Vec<StatusModel> = providers
            .iter()
            .filter(|provider| {
                matches!(provider.key, ProviderKey::Gemini | ProviderKey::Antigravity)
            })
            .flat_map(|provider| {
                let quota_pool_id = quota_pool_id(provider);
                provider.windows.iter().map(move |window| StatusModel {
                    id: window.label.clone(),
                    provider_id: provider.key.as_str().to_owned(),
                    account_id: provider.account_id.clone(),
                    quota_pool_id: quota_pool_id.clone(),
                    state: ModelState::Observed,
                    advertisement: CapabilityState::Unknown,
                    entitlement: CapabilityState::Unknown,
                    execution: CapabilityState::Unknown,
                })
            })
            .collect();
        let entitlements: Vec<StatusEntitlement> = models
            .iter()
            .map(|model| StatusEntitlement {
                provider_id: model.provider_id.clone(),
                account_id: model.account_id.clone(),
                model_id: model.id.clone(),
                state: model.entitlement,
            })
            .collect();
        let quota_pools = providers
            .iter()
            .filter(|provider| !provider.windows.is_empty())
            .map(|provider| StatusQuotaPool {
                id: quota_pool_id(provider),
                provider_id: provider.key.as_str().to_owned(),
                account_id: provider.account_id.clone(),
                shared: true,
                basis: "provider-reported quota scope".to_owned(),
            })
            .collect();
        let observations = providers
            .iter()
            .flat_map(|provider| {
                let quota_pool_id = quota_pool_id(provider);
                provider
                    .windows
                    .iter()
                    .map(move |window| StatusObservation {
                        id: format!("{}:{}", provider.key, window.label),
                        provider_id: provider.key.as_str().to_owned(),
                        account_id: provider.account_id.clone(),
                        quota_pool_id: quota_pool_id.clone(),
                        label: window.label.clone(),
                        used_percent: window.used_percent,
                        remaining_percent: window.remaining_percent,
                        resets_at: window.resets_at,
                        state: if provider.status == ProviderStatus::Ok {
                            ObservationState::Known
                        } else {
                            ObservationState::Unavailable
                        },
                        observed_at: Some(generated_at),
                        collected_at: generated_at,
                        freshness: FreshnessState::Fresh,
                        provenance: Provenance::ProviderApi,
                    })
            })
            .collect();
        Self {
            schema_version: STATUS_SCHEMA_VERSION,
            generated_at,
            collection: CollectionStatus {
                state: CollectionState::Collected,
                outcome: Some(outcome),
                scope: CollectionScope {
                    provider_id: None,
                    account_id: None,
                },
                coverage: providers
                    .iter()
                    .map(|provider| provider.key.as_str().to_owned())
                    .collect(),
                errors,
            },
            providers,
            catalog: Vec::new(),
            accounts,
            capabilities,
            models,
            entitlements,
            quota_pools,
            observations,
            discovery_errors,
        }
    }
}

fn effective_remaining(provider: &ProviderUsage) -> f64 {
    provider
        .windows
        .iter()
        .filter_map(|window| window.remaining_percent)
        .next()
        .unwrap_or(-1.0)
}

fn quota_pool_id(provider: &ProviderUsage) -> String {
    format!(
        "{}:{}:quota",
        provider.key,
        provider.account_id.as_deref().unwrap_or("unknown")
    )
}
