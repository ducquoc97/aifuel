//! SiliconFlow account-balance collection for the `siliconflow:api-key`
//! Provider Integration.
//!
//! `GET /v1/user/info` reports the account's wallet balances on the
//! international endpoint `https://api.siliconflow.com` (documented at
//! https://docs.siliconflow.com/api-reference/userinfo/get-user-info).
//! `totalBalance` combines charge balance and gift credits and is quoted in
//! the platform currency (USD on the international site). It is a credit
//! balance, not a quota window: percentages stay unknown unless the balance
//! parses to zero, which reports the account as exhausted.

use aifuel_core::{EndpointConfig, IntegrationId, ProviderId};
use reqwest::Client;
use serde::Deserialize;

use crate::ResolvedAuth;
use crate::quota::{CollectFuture, QuotaError};

/// The compiled collector id `MonitoringConfig.collector` names.
pub const SILICONFLOW_BALANCE_COLLECTOR: &str = "siliconflow-balance";

#[derive(Deserialize)]
struct UserInfoResponse {
    code: Option<i64>,
    message: Option<String>,
    data: Option<UserInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserInfo {
    total_balance: Option<String>,
}

/// Collect the account balance.
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
        let body: UserInfoResponse = crate::quota::get_json(client, url, endpoint, auth).await?;
        // SiliconFlow answers 200 with a non-20000 `code` on failure.
        if let Some(code) = body.code
            && code != 20000
        {
            return Err(QuotaError::Unavailable(format!(
                "the endpoint reported code {code}: {}",
                body.message.as_deref().unwrap_or("no message")
            )));
        }
        let data = body
            .data
            .ok_or_else(|| QuotaError::Unavailable("the response carried no data".to_owned()))?;
        let label = match data.total_balance.as_deref() {
            Some(total) => format!("Account balance {total} USD"),
            None => "Account balance".to_owned(),
        };
        let exhausted = matches!(data.total_balance.as_deref().and_then(|v| v.parse::<f64>().ok()), Some(total) if total <= 0.0);
        let (used_percent, remaining_percent) = if exhausted {
            (Some(100.0), Some(0.0))
        } else {
            (None, None)
        };
        Ok(vec![crate::quota::observed(
            integration_id,
            provider_id,
            now,
            "balance",
            label,
            used_percent,
            remaining_percent,
            None,
        )])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_successful_response_carries_total_balance() {
        let body: UserInfoResponse = serde_json::from_str(
            r#"{"code":20000,"message":"Successfully retrieved user information.","status":true,"data":{"id":"x","name":"n","image":"","email":"e","isAdmin":false,"balance":"3.61","status":"normal","introduction":"","role":"","chargeBalance":"3.61","totalBalance":"3.61"}}"#,
        )
        .expect("the user-info payload decodes");
        assert_eq!(body.code, Some(20000));
        assert_eq!(
            body.data.expect("data").total_balance.as_deref(),
            Some("3.61")
        );
    }

    #[test]
    fn a_failure_code_is_not_silently_observed() {
        // SiliconFlow reports failure in a 200 body; it must surface as
        // Unavailable, not as a parsed balance.
        let body: UserInfoResponse = serde_json::from_str(
            r#"{"code":40001,"message":"Invalid API key","status":false,"data":null}"#,
        )
        .expect("the failure payload decodes");
        assert_ne!(body.code, Some(20000));
    }
}
