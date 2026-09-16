use super::{DiscoveryFailure, ProviderKey, ProviderUsage};
use serde::Serialize;

pub const STATUS_SCHEMA_VERSION: u32 = 1;

/// The normalized status result shared by text, JSON, dashboard, and MCP.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusReport {
    pub schema_version: u32,
    pub generated_at: f64,
    pub collection: CollectionStatus,
    pub providers: Vec<ProviderUsage>,
    pub accounts: Vec<StatusAccount>,
    pub models: Vec<StatusModel>,
    pub quota_pools: Vec<StatusQuotaPool>,
    pub observations: Vec<StatusObservation>,
    pub discovery_errors: Vec<DiscoveryFailure>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CollectionStatus {
    pub state: String,
    pub outcome: Option<String>,
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
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusAccount {
    pub id: String,
    pub provider_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusModel {
    pub id: String,
    pub provider_id: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusQuotaPool {
    pub id: String,
    pub provider_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusObservation {
    pub id: String,
    pub provider_id: String,
    pub state: String,
    pub observed_at: Option<f64>,
}

impl StatusReport {
    pub fn cold(generated_at: f64) -> Self {
        Self {
            schema_version: STATUS_SCHEMA_VERSION,
            generated_at,
            collection: CollectionStatus {
                state: "not_collected".to_owned(),
                outcome: None,
                scope: CollectionScope {
                    provider_id: None,
                    account_id: None,
                },
                coverage: Vec::new(),
                errors: Vec::new(),
            },
            providers: Vec::new(),
            accounts: Vec::new(),
            models: Vec::new(),
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
                code: "discovery_failure".to_owned(),
                message: failure.detail.to_owned(),
            })
            .collect::<Vec<_>>();
        errors.extend(providers.iter().filter_map(|provider| {
            provider.detail.as_ref().map(|detail| StatusError {
                provider_id: Some(provider.key.as_str().to_owned()),
                code: "collection_failed".to_owned(),
                message: detail.clone(),
            })
        }));
        let outcome = if providers.iter().any(|provider| provider.status == "ok") {
            if errors.is_empty() {
                "complete"
            } else {
                "partial"
            }
        } else if providers.is_empty() && errors.is_empty() {
            "complete"
        } else {
            "failed"
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
        let models = providers
            .iter()
            .filter(|provider| {
                matches!(provider.key, ProviderKey::Gemini | ProviderKey::Antigravity)
            })
            .flat_map(|provider| {
                provider.windows.iter().map(|window| StatusModel {
                    id: window.label.clone(),
                    provider_id: provider.key.as_str().to_owned(),
                    state: "observed".to_owned(),
                })
            })
            .collect();
        let quota_pools = providers
            .iter()
            .filter(|provider| !provider.windows.is_empty())
            .map(|provider| StatusQuotaPool {
                id: format!("{}:quota", provider.key),
                provider_id: provider.key.as_str().to_owned(),
            })
            .collect();
        let observations = providers
            .iter()
            .flat_map(|provider| {
                provider.windows.iter().map(|window| StatusObservation {
                    id: format!("{}:{}", provider.key, window.label),
                    provider_id: provider.key.as_str().to_owned(),
                    state: if provider.status == "ok" {
                        "known".to_owned()
                    } else {
                        "unavailable".to_owned()
                    },
                    observed_at: Some(generated_at),
                })
            })
            .collect();
        Self {
            schema_version: STATUS_SCHEMA_VERSION,
            generated_at,
            collection: CollectionStatus {
                state: "collected".to_owned(),
                outcome: Some(outcome.to_owned()),
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
            accounts,
            models,
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
