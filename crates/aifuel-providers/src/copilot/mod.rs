use super::{CatalogProvider, MonitoringFuture, ProviderMonitoring};
use crate::usage_helpers::{next_month_first, number, response_json, timestamp, value_string};
use aifuel_core::{ProviderKey, ProviderUsage, QuotaWindow};
use serde_json::Value;
use std::fs;

mod agent_run;
pub(crate) use agent_run::ADAPTER as AGENT_RUN_ADAPTER;
mod registration;
pub(crate) use registration::COPILOT_REGISTRATION;

pub static DEFINITION: CatalogProvider =
    CatalogProvider::file_source(ProviderKey::Copilot, ".copilot/config.json");

pub(crate) fn collect(service: &ProviderMonitoring) -> MonitoringFuture<'_> {
    Box::pin(collect_live(service))
}

async fn collect_live(service: &ProviderMonitoring) -> ProviderUsage {
    let path = service.home_dir.join(".copilot/config.json");
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) => return ProviderUsage::error(ProviderKey::Copilot, error.to_string()),
    };
    let cleaned = raw
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let config: Value = match serde_json::from_str(&cleaned) {
        Ok(value) => value,
        Err(error) => return ProviderUsage::error(ProviderKey::Copilot, error.to_string()),
    };
    let Some(tokens) = config.get("copilotTokens").and_then(Value::as_object) else {
        return ProviderUsage::error(ProviderKey::Copilot, "No Copilot-specific token found");
    };
    let Some((account_id, token)) = tokens
        .iter()
        .find_map(|(account, token)| token.as_str().map(|token| (account.to_owned(), token)))
    else {
        return ProviderUsage::error(ProviderKey::Copilot, "No Copilot-specific token found");
    };
    let mut data = None;
    let mut last_error = None;
    for url in [
        &service.config.copilot_user_url,
        &service.config.copilot_token_url,
    ] {
        match service
            .client
            .get(url)
            .header("Authorization", format!("token {token}"))
            .header("User-Agent", "GithubCopilot/1.250.0")
            .header("Accept", "application/json")
            .send()
            .await
        {
            Ok(response) => match response_json(response).await {
                Ok(value) if value.is_object() => {
                    data = Some(value);
                    break;
                }
                Ok(_) => last_error = Some("unexpected response".to_owned()),
                Err(error) => last_error = Some(error),
            },
            Err(error) => last_error = Some(error.to_string()),
        }
    }
    let Some(data) = data else {
        return ProviderUsage::error(
            ProviderKey::Copilot,
            last_error.unwrap_or_else(|| "Copilot live usage endpoint unreachable".to_owned()),
        );
    };
    let reset_at = data
        .get("quota_reset_date_utc")
        .and_then(timestamp)
        .or_else(|| data.get("quota_reset_date").and_then(timestamp))
        .or_else(|| Some(next_month_first()));
    let mut windows = Vec::new();
    if let Some(snapshots) = data.get("quota_snapshots").and_then(Value::as_object) {
        for (name, snapshot) in snapshots {
            let Some(snapshot) = snapshot.as_object() else {
                continue;
            };
            let entitlement = snapshot.get("entitlement").and_then(number);
            let has_quota = snapshot
                .get("has_quota")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            if !has_quota && entitlement.is_none() {
                continue;
            }
            let label = if name == "premium_interactions" {
                "Premium Models".to_owned()
            } else {
                name.replace('_', " ")
            };
            let remaining = if snapshot
                .get("unlimited")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                Some(100.0)
            } else if let Some(percent) = snapshot.get("percent_remaining").and_then(number) {
                Some(percent)
            } else {
                snapshot
                    .get("remaining")
                    .or_else(|| snapshot.get("quota_remaining"))
                    .and_then(number)
                    .zip(entitlement)
                    .map(|(remaining, total)| remaining / total * 100.0)
            };
            let mut window = QuotaWindow::new(label, "monthly", None, remaining, reset_at);
            window.limit = entitlement;
            window.used = snapshot
                .get("remaining")
                .or_else(|| snapshot.get("quota_remaining"))
                .and_then(number)
                .zip(entitlement)
                .map(|(remaining, total)| total - remaining);
            windows.push(window);
        }
    }
    if windows.is_empty() {
        return ProviderUsage::error(
            ProviderKey::Copilot,
            "Copilot live usage returned no quota snapshots",
        );
    }
    let mut result = ProviderUsage::success(ProviderKey::Copilot, windows);
    result.plan = data.get("copilot_plan").and_then(value_string);
    result.account_id = account_id
        .rsplit_once(':')
        .map(|(_, account)| account.to_owned())
        .or(Some(account_id));
    result
}
