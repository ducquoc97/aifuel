//! OpenRouter quota collection for the `openrouter:api-key` Provider
//! Integration.
//!
//! `GET /api/v1/key` reports the allowance attached to the key itself - key
//! scope, not subscription quota. A missing `limit` means the key is
//! unmetered; it is never reported as zero remaining.

use aifuel_core::{
    FreshnessState, IntegrationId, ObservationState, Provenance, ProviderId, StatusObservation,
};
use reqwest::{Client, StatusCode};
use serde::Deserialize;

use crate::ResolvedAuth;

/// The compiled collector id `MonitoringConfig.collector` names.
pub const OPENROUTER_KEY_COLLECTOR: &str = "openrouter-key";

/// The `/key` payload. `usage`, `limit`, and `limit_remaining` are credit
/// amounts; every one is optional because OpenRouter omits `limit` on
/// unmetered keys.
#[derive(Deserialize)]
struct KeyResponse {
    data: KeyData,
}

#[derive(Deserialize)]
struct KeyData {
    usage: Option<f64>,
    limit: Option<f64>,
    limit_remaining: Option<f64>,
}

/// Why a key-quota collection produced no observed values.
#[derive(Debug)]
pub enum KeyQuotaError {
    /// The credential is absent, malformed, or was rejected.
    Unauthenticated(String),
    /// The endpoint was unreachable or returned an unexpected shape.
    Unavailable(String),
}

/// Collect the key-scoped credit allowance. `url` is the collection endpoint
/// (config/env overridable for tests); `auth` is the resolved credential the
/// integration's Authentication Binding produced. `endpoint` carries the
/// configured request headers and timeout when the Monitoring Contract
/// declared an endpoint override.
pub async fn collect_key_quota(
    client: &Client,
    url: &str,
    endpoint: Option<&aifuel_core::EndpointConfig>,
    auth: &ResolvedAuth,
    integration_id: &IntegrationId,
    provider_id: &ProviderId,
    now: f64,
) -> Result<StatusObservation, KeyQuotaError> {
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
        .map_err(|error| KeyQuotaError::Unavailable(format!("request failed: {error}")))?;
    if response.status() == StatusCode::UNAUTHORIZED || response.status() == StatusCode::FORBIDDEN {
        return Err(KeyQuotaError::Unauthenticated(format!(
            "the endpoint rejected the credential ({})",
            response.status()
        )));
    }
    if !response.status().is_success() {
        return Err(KeyQuotaError::Unavailable(format!(
            "the endpoint answered {}",
            response.status()
        )));
    }
    let body = response.json::<KeyResponse>().await.map_err(|error| {
        KeyQuotaError::Unavailable(format!("the response shape was not recognized: {error}"))
    })?;

    let (used_percent, remaining_percent) = quota_percents(&body.data);
    Ok(StatusObservation {
        id: format!("{integration_id}:key-credits"),
        provider_id: provider_id.as_str().to_owned(),
        account_id: None,
        quota_pool_id: format!("{integration_id}:key-credits"),
        label: "Key credits".to_owned(),
        used_percent,
        remaining_percent,
        resets_at: None,
        state: ObservationState::Observed,
        integration_id: Some(integration_id.clone()),
        observed_at: Some(now),
        collected_at: now,
        freshness: FreshnessState::Fresh,
        provenance: Provenance::ProviderApi,
    })
}

/// `(used_percent, remaining_percent)` from the `/key` payload. A missing or
/// non-positive `limit` yields unknown percentages - an unmetered key is
/// never reported as zero remaining.
fn quota_percents(data: &KeyData) -> (Option<f64>, Option<f64>) {
    let used_percent = match (data.usage, data.limit) {
        (Some(usage), Some(limit)) if limit > 0.0 => Some(usage / limit * 100.0),
        _ => None,
    };
    let remaining_percent = match (data.limit_remaining, data.limit) {
        (Some(remaining), Some(limit)) if limit > 0.0 => Some(remaining / limit * 100.0),
        _ => None,
    };
    (used_percent, remaining_percent)
}

/// The observation recorded when collection cannot produce values.
/// A missing or rejected credential is Unauthenticated; anything else is
/// Unavailable. Missing limits stay unknown - never zero remaining.
pub fn unobserved(
    integration_id: &IntegrationId,
    provider_id: &ProviderId,
    now: f64,
    state: ObservationState,
) -> StatusObservation {
    StatusObservation {
        id: format!("{integration_id}:key-credits"),
        provider_id: provider_id.as_str().to_owned(),
        account_id: None,
        quota_pool_id: format!("{integration_id}:key-credits"),
        label: "Key credits".to_owned(),
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
    fn unmetered_key_reports_unknown_remaining_not_zero() {
        // OpenRouter omits `limit` on unmetered keys. The observation must
        // carry unknown percentages rather than imply the key is exhausted.
        let body: KeyResponse =
            serde_json::from_str(r#"{"data":{"usage":12.34,"limit":null,"limit_remaining":null}}"#)
                .expect("the /key payload decodes");
        let (used_percent, remaining_percent) = quota_percents(&body.data);
        assert!(
            used_percent.is_none() && remaining_percent.is_none(),
            "an absent limit must yield unknown percentages, not zero"
        );
    }

    #[test]
    fn a_metered_key_reports_used_and_remaining_percents() {
        let body: KeyResponse =
            serde_json::from_str(r#"{"data":{"usage":2.5,"limit":10.0,"limit_remaining":7.5}}"#)
                .expect("the /key payload decodes");
        let (used_percent, remaining_percent) = quota_percents(&body.data);
        assert_eq!(used_percent, Some(25.0));
        assert_eq!(remaining_percent, Some(75.0));
    }

    #[test]
    fn unobserved_marks_the_integration_and_never_claims_values() {
        let observation = unobserved(
            &IntegrationId::new("openrouter:api-key"),
            &ProviderId::new("openrouter"),
            1_800_000_000.0,
            ObservationState::Unauthenticated,
        );
        assert_eq!(observation.state, ObservationState::Unauthenticated);
        assert_eq!(
            observation
                .integration_id
                .as_ref()
                .map(IntegrationId::as_str),
            Some("openrouter:api-key")
        );
        assert!(observation.remaining_percent.is_none());
        assert!(observation.used_percent.is_none());
    }
}
