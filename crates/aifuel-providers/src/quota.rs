//! Shared plumbing for API-key quota and balance collectors.
//!
//! Per the Monitoring Collection Contract in `docs/specs/provider-integrations.md`,
//! a collector is an optional per-integration contract over a documented
//! provider endpoint. Every compiled collector shares the same mechanics:
//! issue one authenticated GET, decode the JSON body, and produce typed
//! [`StatusObservation`] values. Unknown or missing values stay `None`;
//! failures produce an unobserved observation rather than a fabricated
//! number.

use crate::ResolvedAuth;
use crate::monitoring::CollectionConfig;
use aifuel_core::{
    EndpointConfig, FreshnessState, IntegrationId, ObservationState, Provenance, ProviderId,
    StatusObservation,
};
use reqwest::{Client, StatusCode};
use serde::de::DeserializeOwned;
use std::future::Future;
use std::pin::Pin;

/// Why a quota collection produced no observed values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QuotaError {
    /// The credential is absent, malformed, or was rejected.
    Unauthenticated(String),
    /// The endpoint was unreachable or returned an unexpected shape.
    Unavailable(String),
}

impl From<crate::openrouter::KeyQuotaError> for QuotaError {
    fn from(error: crate::openrouter::KeyQuotaError) -> Self {
        match error {
            crate::openrouter::KeyQuotaError::Unauthenticated(reason) => {
                Self::Unauthenticated(reason)
            }
            crate::openrouter::KeyQuotaError::Unavailable(reason) => Self::Unavailable(reason),
        }
    }
}

/// The future a compiled collector returns. One collection may emit several
/// observations (for example per-window quota pools).
pub(crate) type CollectFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<StatusObservation>, QuotaError>> + Send + 'a>>;

/// Everything the monitoring loop needs to run one compiled collector.
pub(crate) struct CollectorSpec {
    /// The resolved default endpoint from [`CollectionConfig`], used when the
    /// integration does not carry an explicit `monitoring.endpoint` override.
    pub default_url: fn(&CollectionConfig) -> String,
    /// The collection call itself.
    pub collect: for<'a> fn(
        &'a Client,
        &'a str,
        Option<&'a EndpointConfig>,
        &'a ResolvedAuth,
        &'a IntegrationId,
        &'a ProviderId,
        f64,
    ) -> CollectFuture<'a>,
    /// The unobserved placeholder recorded when collection cannot run.
    pub unobserved: fn(&IntegrationId, &ProviderId, f64, ObservationState) -> StatusObservation,
}

/// Look up the compiled collector for a collector id, if one exists.
pub(crate) fn spec(id: &str) -> Option<CollectorSpec> {
    match id {
        crate::openrouter::OPENROUTER_KEY_COLLECTOR => Some(CollectorSpec {
            default_url: |config| config.openrouter_key_url.clone(),
            collect: openrouter_collect,
            unobserved: crate::openrouter::unobserved,
        }),
        crate::zai::ZAI_QUOTA_COLLECTOR => Some(CollectorSpec {
            default_url: |config| config.zai_quota_url.clone(),
            collect: crate::zai::collect,
            unobserved: |id, provider, now, state| {
                unobserved(id, provider, now, state, "quota", "Plan quota")
            },
        }),
        crate::deepseek::DEEPSEEK_BALANCE_COLLECTOR => Some(CollectorSpec {
            default_url: |config| config.deepseek_balance_url.clone(),
            collect: crate::deepseek::collect,
            unobserved: |id, provider, now, state| {
                unobserved(id, provider, now, state, "balance", "Account balance")
            },
        }),
        crate::siliconflow::SILICONFLOW_BALANCE_COLLECTOR => Some(CollectorSpec {
            default_url: |config| config.siliconflow_balance_url.clone(),
            collect: crate::siliconflow::collect,
            unobserved: |id, provider, now, state| {
                unobserved(id, provider, now, state, "balance", "Account balance")
            },
        }),
        _ => None,
    }
}

/// The collector ids the compiled binary can dispatch. Config validation
/// rejects configured collector names outside this set.
pub(crate) fn compiled_collector_ids() -> &'static [&'static str] {
    &[
        crate::openrouter::OPENROUTER_KEY_COLLECTOR,
        crate::zai::ZAI_QUOTA_COLLECTOR,
        crate::deepseek::DEEPSEEK_BALANCE_COLLECTOR,
        crate::siliconflow::SILICONFLOW_BALANCE_COLLECTOR,
    ]
}

/// GET `url` with the resolved monitoring credential applied, honoring the
/// optional per-integration endpoint override's extra headers and timeout,
/// and decode the JSON body. 401/403 map to [`QuotaError::Unauthenticated`];
/// other non-success statuses map to [`QuotaError::Unavailable`].
pub(crate) async fn get_json<T: DeserializeOwned>(
    client: &Client,
    url: &str,
    endpoint: Option<&EndpointConfig>,
    auth: &ResolvedAuth,
) -> Result<T, QuotaError> {
    let mut request = client.get(url);
    if let Some(endpoint) = endpoint {
        for (name, value) in &endpoint.extra_headers {
            if let (Ok(name), Ok(value)) = (
                name.parse::<reqwest::header::HeaderName>(),
                value.parse::<reqwest::header::HeaderValue>(),
            ) {
                request = request.header(name, value);
            }
        }
        if let Some(seconds) = endpoint.request_timeout_seconds {
            request = request.timeout(std::time::Duration::from_secs(seconds));
        }
    }
    match auth {
        ResolvedAuth::None => {}
        ResolvedAuth::ApiKey { key, delivery } => {
            request = match delivery {
                aifuel_core::KeyDelivery::Bearer => request.bearer_auth(key),
                aifuel_core::KeyDelivery::Header { name } => request.header(name.as_str(), key),
            };
        }
        ResolvedAuth::OAuth { access_token, .. } => {
            request = request.bearer_auth(access_token);
        }
    }
    let response = request
        .send()
        .await
        .map_err(|error| QuotaError::Unavailable(format!("request failed: {error}")))?;
    if response.status() == StatusCode::UNAUTHORIZED || response.status() == StatusCode::FORBIDDEN {
        return Err(QuotaError::Unauthenticated(format!(
            "the endpoint rejected the credential ({})",
            response.status()
        )));
    }
    if !response.status().is_success() {
        return Err(QuotaError::Unavailable(format!(
            "the endpoint answered {}",
            response.status()
        )));
    }
    response.json::<T>().await.map_err(|error| {
        QuotaError::Unavailable(format!("the response shape was not recognized: {error}"))
    })
}

/// Build one [`ObservationState::Observed`] observation over a named quota
/// pool. Percent fields stay `None` when the endpoint does not publish them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn observed(
    integration_id: &IntegrationId,
    provider_id: &ProviderId,
    now: f64,
    pool_suffix: &str,
    label: impl Into<String>,
    used_percent: Option<f64>,
    remaining_percent: Option<f64>,
    resets_at: Option<f64>,
) -> StatusObservation {
    StatusObservation {
        id: format!("{integration_id}:{pool_suffix}"),
        provider_id: provider_id.as_str().to_owned(),
        account_id: None,
        quota_pool_id: format!("{integration_id}:{pool_suffix}"),
        label: label.into(),
        used_percent,
        remaining_percent,
        resets_at,
        state: ObservationState::Observed,
        integration_id: Some(integration_id.clone()),
        observed_at: Some(now),
        collected_at: now,
        freshness: FreshnessState::Fresh,
        provenance: Provenance::ProviderApi,
    }
}

/// The observation recorded when a collection produced no values. The label
/// names the pool the contract would have observed so reports stay honest.
pub(crate) fn unobserved(
    integration_id: &IntegrationId,
    provider_id: &ProviderId,
    now: f64,
    state: ObservationState,
    pool_suffix: &str,
    label: impl Into<String>,
) -> StatusObservation {
    StatusObservation {
        id: format!("{integration_id}:{pool_suffix}"),
        provider_id: provider_id.as_str().to_owned(),
        account_id: None,
        quota_pool_id: format!("{integration_id}:{pool_suffix}"),
        label: label.into(),
        used_percent: None,
        remaining_percent: None,
        resets_at: None,
        state,
        integration_id: Some(integration_id.clone()),
        observed_at: None,
        collected_at: now,
        freshness: FreshnessState::Unknown,
        provenance: Provenance::ProviderApi,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_compiled_collector_id_dispatches() {
        // Config validation accepts exactly `compiled_collector_ids`; an id
        // listed here but missing from `spec` would validate yet never run.
        for id in compiled_collector_ids() {
            assert!(spec(id).is_some(), "{id} is listed but has no spec");
        }
    }
}

fn openrouter_collect<'a>(
    client: &'a Client,
    url: &'a str,
    endpoint: Option<&'a EndpointConfig>,
    auth: &'a ResolvedAuth,
    integration_id: &'a IntegrationId,
    provider_id: &'a ProviderId,
    now: f64,
) -> CollectFuture<'a> {
    Box::pin(async move {
        crate::openrouter::collect_key_quota(
            client,
            url,
            endpoint,
            auth,
            integration_id,
            provider_id,
            now,
        )
        .await
        .map(|observation| vec![observation])
        .map_err(QuotaError::from)
    })
}
