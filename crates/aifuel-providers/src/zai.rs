//! Z.AI coding-plan quota collection for the `zai:api-key` Provider
//! Integration.
//!
//! `GET /api/monitor/usage/quota/limit` reports the plan-scoped quota
//! windows Z.AI publishes for GLM coding-plan subscribers (documented at
//! https://docs.z.ai/scenario-example/develop-tools/claude#api-usage-query).
//! Each `limits[]` entry carries a rolling window (`unit` 3 = hours,
//! `unit` 6 = weeks), its used `percentage`, and a `nextResetTime` in
//! epoch milliseconds. Unknown entries are skipped rather than mapped to a
//! window they are not.

use aifuel_core::{EndpointConfig, IntegrationId, ProviderId};
use reqwest::Client;
use serde::Deserialize;

use crate::ResolvedAuth;
use crate::quota::{CollectFuture, QuotaError};

/// The compiled collector id `MonitoringConfig.collector` names.
pub const ZAI_QUOTA_COLLECTOR: &str = "zai-quota";

#[derive(Deserialize)]
struct QuotaResponse {
    code: Option<i64>,
    msg: Option<String>,
    data: Option<QuotaData>,
}

#[derive(Deserialize)]
struct QuotaData {
    limits: Option<Vec<QuotaLimit>>,
}

#[derive(Deserialize)]
struct QuotaLimit {
    /// `TOKENS_LIMIT`, `CREDIT_LIMIT`, or `TIME_LIMIT` (tool-call quota).
    #[serde(rename = "type")]
    kind: Option<String>,
    /// Rolling-window unit: 3 = hours, 6 = weeks.
    unit: Option<i64>,
    /// The window multiplier paired with `unit` (for example 5 with unit 3
    /// is the five-hour window).
    number: Option<i64>,
    /// Used fraction of the window, 0-100.
    percentage: Option<f64>,
    /// Epoch milliseconds when the window resets.
    #[serde(rename = "nextResetTime")]
    next_reset_ms: Option<f64>,
}

/// Collect every quota window the endpoint publishes.
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
        let body: QuotaResponse = crate::quota::get_json(client, url, endpoint, auth).await?;
        if let Some(code) = body.code
            && code != 200
        {
            return Err(QuotaError::Unavailable(format!(
                "the endpoint reported code {code}: {}",
                body.msg.as_deref().unwrap_or("no message")
            )));
        }
        let data = body
            .data
            .ok_or_else(|| QuotaError::Unavailable("the response carried no data".to_owned()))?;
        let limits = data
            .limits
            .ok_or_else(|| QuotaError::Unavailable("the response carried no limits".to_owned()))?;
        if limits.is_empty() {
            return Err(QuotaError::Unavailable(
                "the response published no quota windows".to_owned(),
            ));
        }
        Ok(limits
            .iter()
            .map(|limit| {
                let (suffix, label) = describe(limit);
                crate::quota::observed(
                    integration_id,
                    provider_id,
                    now,
                    &format!("quota:{suffix}"),
                    label,
                    limit.percentage,
                    limit.percentage.map(|used| (100.0 - used).max(0.0)),
                    limit.next_reset_ms.map(|ms| ms / 1000.0),
                )
            })
            .collect())
    })
}

/// The observation id suffix and label for one quota window. `unit` 3 is a
/// rolling hour window and `unit` 6 a rolling week window; `TIME_LIMIT` is
/// the tool-call quota.
fn describe(limit: &QuotaLimit) -> (String, String) {
    match limit.kind.as_deref() {
        Some("TIME_LIMIT") => ("tools".to_owned(), "Tool-call quota".to_owned()),
        Some("TOKENS_LIMIT") | Some("CREDIT_LIMIT") => match (limit.unit, limit.number) {
            (Some(3), Some(n)) => (format!("{n}h"), format!("{n}-hour token quota")),
            (Some(6), Some(n)) => (format!("{n}w"), format!("{n}-week token quota")),
            _ => ("tokens".to_owned(), "Token quota".to_owned()),
        },
        Some(kind) => (kind.to_lowercase(), format!("{kind} quota")),
        None => ("unknown".to_owned(), "Quota".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_published_window_reports_used_and_remaining_with_reset() {
        let body: QuotaResponse = serde_json::from_str(
            r#"{"success":true,"code":200,"msg":"","data":{"level":"pro","limits":[
                {"type":"TOKENS_LIMIT","unit":3,"number":5,"usage":1000000,"currentValue":250000,"remaining":750000,"percentage":25,"nextResetTime":1755757704492},
                {"type":"TIME_LIMIT","unit":5,"number":1,"usage":499,"currentValue":0,"remaining":499,"percentage":0,"nextResetTime":1757059200000}
            ]}}"#,
        )
        .expect("the quota payload decodes");
        let limits = body.data.expect("data").limits.expect("limits");
        assert_eq!(limits.len(), 2);
        let (suffix, label) = describe(&limits[0]);
        assert_eq!(suffix, "5h");
        assert_eq!(label, "5-hour token quota");
        assert_eq!(limits[0].percentage, Some(25.0));
        // Epoch milliseconds convert to seconds for `resets_at`.
        assert_eq!(
            limits[0].next_reset_ms.map(|ms| ms / 1000.0),
            Some(1755757704.492)
        );
        let (suffix, label) = describe(&limits[1]);
        assert_eq!(suffix, "tools");
        assert_eq!(label, "Tool-call quota");
    }

    #[test]
    fn a_reported_failure_code_is_unavailable() {
        let body: QuotaResponse =
            serde_json::from_str(r#"{"success":false,"code":401,"msg":"unauthorized"}"#)
                .expect("the failure payload decodes");
        assert_eq!(body.code, Some(401));
    }
}
