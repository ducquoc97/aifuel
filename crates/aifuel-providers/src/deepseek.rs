//! DeepSeek account-balance collection for the `deepseek:api-key`
//! Provider Integration.
//!
//! `GET /user/balance` reports the account's remaining prepaid credit per
//! currency (documented at
//! https://api-docs.deepseek.com/quick_start/parameter_settings under
//! "Check balance"). It is a credit balance, not a quota window: there is
//! no published limit or reset, so percentage fields stay unknown. The
//! endpoint's `is_available` flag marks whether the balance can still pay
//! for requests; `false` reports the account as exhausted.

use aifuel_core::{EndpointConfig, IntegrationId, ProviderId};
use reqwest::Client;
use serde::Deserialize;

use crate::ResolvedAuth;
use crate::quota::{CollectFuture, QuotaError};

/// The compiled collector id `MonitoringConfig.collector` names.
pub const DEEPSEEK_BALANCE_COLLECTOR: &str = "deepseek-balance";

#[derive(Deserialize)]
struct BalanceResponse {
    is_available: Option<bool>,
    balance_infos: Option<Vec<BalanceInfo>>,
}

#[derive(Deserialize)]
struct BalanceInfo {
    currency: Option<String>,
    total_balance: Option<String>,
}

/// Collect the account balance. Multi-currency accounts prefer the USD row;
/// otherwise the first reported balance is used.
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
        let body: BalanceResponse = crate::quota::get_json(client, url, endpoint, auth).await?;
        let balances = body
            .balance_infos
            .ok_or_else(|| QuotaError::Unavailable("the response carried no balance".to_owned()))?;
        let balance = balances
            .iter()
            .find(|info| info.currency.as_deref() == Some("USD"))
            .or_else(|| balances.first());
        let label = match balance.and_then(|info| {
            Some(format!(
                "Account balance {} {}",
                info.total_balance.as_deref()?,
                info.currency.as_deref()?
            ))
        }) {
            Some(label) => label,
            None => "Account balance".to_owned(),
        };
        // `is_available` false means the balance cannot pay for requests -
        // report the pool exhausted. Otherwise the credit total has no
        // published cap, so percentages stay unknown.
        let (used_percent, remaining_percent) = match body.is_available {
            Some(false) => (Some(100.0), Some(0.0)),
            _ => (None, None),
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
    fn balance_infos_prefer_the_usd_row() {
        let body: BalanceResponse = serde_json::from_str(
            r#"{"is_available":true,"balance_infos":[
                {"currency":"CNY","total_balance":"110.00","granted_balance":"10.00","topped_up_balance":"100.00"},
                {"currency":"USD","total_balance":"8.20","granted_balance":"0.00","topped_up_balance":"8.20"}
            ]}"#,
        )
        .expect("the balance payload decodes");
        let balances = body.balance_infos.expect("balances");
        let usd = balances
            .iter()
            .find(|info| info.currency.as_deref() == Some("USD"))
            .expect("the USD row is present");
        assert_eq!(usd.total_balance.as_deref(), Some("8.20"));
    }

    #[test]
    fn an_unavailable_account_is_distinguished_from_unknown() {
        // `is_available: false` is the endpoint's own exhaustion signal;
        // it must not collapse into "unknown".
        let body: BalanceResponse =
            serde_json::from_str(r#"{"is_available":false,"balance_infos":[]}"#)
                .expect("the payload decodes");
        assert_eq!(body.is_available, Some(false));
    }
}
