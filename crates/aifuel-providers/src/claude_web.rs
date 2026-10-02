//! claude.ai web-session usage collection for the `claude-web:web` Provider
//! Integration.
//!
//! OmniRoute's `claude_web` provider documents the unofficial web endpoints
//! a Claude browser session exposes: `GET /api/organizations` lists the
//! organizations the session belongs to, and
//! `GET /api/organizations/{orgId}/usage` reports the rolling quota windows
//! (`five_hour`, `seven_day`, and per-model `seven_day_opus` /
//! `seven_day_sonnet`) as a `utilization` percentage with an RFC 3339
//! `resets_at`. These are private web endpoints: the shape is implemented
//! as documented and may drift, so every field is optional and a response
//! that publishes no windows reports Unavailable rather than fabricating
//! values.
//!
//! The collector resolves the first organization the session belongs to,
//! then reads its usage. `AIFUEL_CLAUDE_WEB_USAGE_URL` points the
//! organizations endpoint at a stub for tests.

use aifuel_core::{EndpointConfig, IntegrationId, ProviderId};
use reqwest::Client;
use serde::Deserialize;

use crate::ResolvedAuth;
use crate::quota::{CollectFuture, QuotaError};

/// The compiled collector id `MonitoringConfig.collector` names.
pub const CLAUDE_WEB_USAGE_COLLECTOR: &str = "claude-web-usage";

/// One entry of the `GET /api/organizations` list: the organization uuid is
/// all the collector needs.
#[derive(Deserialize)]
struct Organization {
    uuid: Option<String>,
}

/// The `GET /api/organizations/{org}/usage` payload OmniRoute documents:
/// one object per rolling window, each carrying `utilization` percent and
/// `resets_at`. Windows the subscription does not publish stay `None`.
#[derive(Deserialize)]
struct UsageResponse {
    five_hour: Option<UsageWindow>,
    seven_day: Option<UsageWindow>,
    seven_day_opus: Option<UsageWindow>,
    seven_day_sonnet: Option<UsageWindow>,
}

#[derive(Deserialize)]
struct UsageWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

/// Collect the session's organization, then its published usage windows.
/// `url` is the organizations endpoint (config/env overridable); the usage
/// path extends it with `/{uuid}/usage`.
pub(crate) fn collect<'a>(
    client: &'a Client,
    url: &'a str,
    endpoint: Option<&'a EndpointConfig>,
    auth: &'a ResolvedAuth,
    integration_id: &'a IntegrationId,
    provider_id: &'a ProviderId,
    now: f64,
) -> CollectFuture<'a> {
    Box::pin(async move {
        let organizations: Vec<Organization> =
            crate::quota::get_json(client, url, endpoint, auth).await?;
        let Some(uuid) = organizations
            .iter()
            .find_map(|organization| organization.uuid.as_deref())
        else {
            return Err(QuotaError::Unavailable(
                "the session belongs to no organization".to_owned(),
            ));
        };
        let usage_url = format!("{}/{uuid}/usage", url.trim_end_matches('/'));
        let body: UsageResponse =
            crate::quota::get_json(client, &usage_url, endpoint, auth).await?;

        let windows = [
            ("usage:5h", "5-hour session window", &body.five_hour),
            ("usage:7d", "7-day window", &body.seven_day),
            ("usage:7d-opus", "7-day Opus window", &body.seven_day_opus),
            (
                "usage:7d-sonnet",
                "7-day Sonnet window",
                &body.seven_day_sonnet,
            ),
        ];
        let observations: Vec<_> = windows
            .iter()
            .filter_map(|(suffix, label, window)| {
                let window = window.as_ref()?;
                Some(crate::quota::observed(
                    integration_id,
                    provider_id,
                    now,
                    suffix,
                    *label,
                    window.utilization,
                    window.utilization.map(|used| (100.0 - used).max(0.0)),
                    window.resets_at.as_deref().and_then(rfc3339_epoch),
                ))
            })
            .collect();
        if observations.is_empty() {
            return Err(QuotaError::Unavailable(
                "the usage response published no quota windows".to_owned(),
            ));
        }
        Ok(observations)
    })
}

/// The unobserved placeholder recorded when the session cannot collect:
/// the pool identity the `usage:*` observations share.
pub(crate) fn unobserved(
    integration_id: &IntegrationId,
    provider_id: &ProviderId,
    now: f64,
    state: aifuel_core::ObservationState,
) -> aifuel_core::StatusObservation {
    crate::quota::unobserved(
        integration_id,
        provider_id,
        now,
        state,
        "usage",
        "Session usage",
    )
}

/// RFC 3339 to unix seconds. `resets_at` arrives as a timestamp string;
/// anything else reads as absent rather than being invented.
fn rfc3339_epoch(text: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|instant| instant.timestamp_millis() as f64 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documented_usage_shape_decodes_every_window() {
        // The OmniRoute-documented payload: utilization is a 0-100
        // percentage, resets_at is RFC 3339.
        let body: UsageResponse = serde_json::from_str(
            r#"{
                "five_hour": {"utilization": 17.0, "resets_at": "2030-01-01T18:59:59Z"},
                "seven_day": {"utilization": 11.0, "resets_at": "2030-01-07T16:59:59Z"},
                "seven_day_opus": {"utilization": 0.0, "resets_at": "2030-01-07T16:59:59Z"},
                "seven_day_sonnet": null
            }"#,
        )
        .expect("the documented usage payload decodes");
        let five_hour = body.five_hour.expect("five_hour");
        assert_eq!(five_hour.utilization, Some(17.0));
        assert_eq!(
            five_hour.resets_at.as_deref().and_then(rfc3339_epoch),
            Some(1_893_524_399.0)
        );
        assert!(body.seven_day_sonnet.is_none());
    }

    #[test]
    fn missing_windows_and_timestamps_stay_absent() {
        // A stripped payload decodes with None windows rather than failing
        // - a private endpoint's shape may drift.
        let body: UsageResponse = serde_json::from_str(r#"{"five_hour": {"utilization": 4.0}}"#)
            .expect("a partial payload decodes");
        assert!(body.seven_day.is_none());
        let window = body.five_hour.expect("five_hour");
        assert_eq!(window.resets_at.as_deref().and_then(rfc3339_epoch), None);
        assert!(rfc3339_epoch("not a timestamp").is_none());
    }
}
