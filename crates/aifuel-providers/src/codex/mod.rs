use super::{CatalogProvider, MonitoringFuture, ProviderMonitoring};
use crate::usage_helpers::{
    deep_find, number, percent_value, period_for_seconds, read_json, response_json, timestamp,
    unix_timestamp, value_string,
};
use aifuel_core::{ProviderKey, ProviderUsage, QuotaWindow, ResetCredit, ResetCredits};
use serde_json::Value;

mod agent_run;
pub(crate) use agent_run::ADAPTER as AGENT_RUN_ADAPTER;

pub static DEFINITION: CatalogProvider =
    CatalogProvider::file_source(ProviderKey::Codex, ".codex/auth.json");

pub(crate) fn collect(service: &ProviderMonitoring) -> MonitoringFuture<'_> {
    Box::pin(collect_live(service))
}

async fn collect_live(service: &ProviderMonitoring) -> ProviderUsage {
    let path = service.home_dir.join(".codex/auth.json");
    let credentials = match read_json(&path) {
        Ok(value) => value,
        Err(error) => return ProviderUsage::error(ProviderKey::Codex, error),
    };
    let Some(token) = deep_find(&credentials, &["access_token"]).and_then(Value::as_str) else {
        return ProviderUsage::error(
            ProviderKey::Codex,
            "missing access_token in ~/.codex/auth.json",
        );
    };
    let account = deep_find(&credentials, &["account_id"])
        .and_then(value_string)
        .unwrap_or_default();
    let response = match service
        .client
        .get(&service.config.codex_usage_url)
        .bearer_auth(token)
        .header("chatgpt-account-id", account.clone())
        .header("originator", "codex_cli_rs")
        .header("User-Agent", "codex_cli_rs/usage-monitor")
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return ProviderUsage::error(ProviderKey::Codex, error.to_string()),
    };
    let data = match response_json(response).await {
        Ok(data) => data,
        Err(error) => return ProviderUsage::error(ProviderKey::Codex, error),
    };
    let Some(rate_limit) = data.get("rate_limit").and_then(Value::as_object) else {
        return ProviderUsage::error(
            ProviderKey::Codex,
            "usage endpoint returned no rate_limit windows",
        );
    };
    let mut windows = Vec::new();
    for name in ["primary_window", "secondary_window"] {
        if let Some(window) = rate_limit.get(name).and_then(parse_window) {
            windows.push(window);
        }
    }
    if let Some(extras) = data.get("additional_rate_limits").and_then(Value::as_array) {
        for extra in extras {
            let Some(extra_object) = extra.as_object() else {
                continue;
            };
            let prefix = extra_object
                .get("limit_name")
                .and_then(Value::as_str)
                .unwrap_or("Model");
            for name in ["primary_window", "secondary_window"] {
                if let Some(mut window) = extra_object
                    .get("rate_limit")
                    .and_then(Value::as_object)
                    .and_then(|rate| rate.get(name))
                    .and_then(parse_window)
                {
                    window.label = format!("{prefix} {}", window.label);
                    windows.push(window);
                }
            }
        }
    }
    if windows.is_empty() {
        return ProviderUsage::error(
            ProviderKey::Codex,
            "usage endpoint returned no rate_limit windows",
        );
    }
    let mut result = ProviderUsage::success(ProviderKey::Codex, windows);
    result.plan = data.get("plan_type").and_then(value_string);
    result.account_id = (!account.is_empty()).then_some(account);
    result.reset_credits = parse_reset_credits(&data);
    result
}

fn parse_window(value: &Value) -> Option<QuotaWindow> {
    let object = value.as_object()?;
    let seconds = object.get("limit_window_seconds").and_then(number)?;
    let (period, label) = period_for_seconds(seconds);
    let reset = object.get("reset_at").and_then(timestamp).or_else(|| {
        object
            .get("reset_after_seconds")
            .and_then(number)
            .map(|seconds| unix_timestamp() + seconds)
    });
    Some(QuotaWindow::new(
        label,
        period,
        object.get("used_percent").and_then(percent_value),
        None,
        reset,
    ))
}

fn parse_reset_credits(data: &Value) -> Option<ResetCredits> {
    let value = data.get("rate_limit_reset_credits")?.as_object()?;
    let available_count = value
        .get("available_count")
        .or_else(|| value.get("availableCount"))
        .and_then(number)?
        .max(0.0) as u64;
    let credits = value
        .get("credits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|credit| ResetCredit {
            reset_type: credit
                .get("reset_type")
                .or_else(|| credit.get("resetType"))
                .and_then(value_string),
            title: credit.get("title").and_then(value_string),
            description: credit.get("description").and_then(value_string),
            expires_at: credit
                .get("expires_at")
                .or_else(|| credit.get("expiresAt"))
                .and_then(timestamp),
        })
        .collect();
    Some(ResetCredits {
        available_count,
        credits,
    })
}
