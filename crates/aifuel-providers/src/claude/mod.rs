use super::{CatalogProvider, MonitoringFuture, ProviderMonitoring};
use crate::usage_helpers::{deep_find, percent, read_json, response_json, timestamp, value_string};
use aifuel_core::{ProviderKey, ProviderUsage, QuotaWindow};
use serde_json::Value;

pub static DEFINITION: CatalogProvider =
    CatalogProvider::file_source(ProviderKey::Claude, ".claude/.credentials.json");

pub(crate) fn collect(service: &ProviderMonitoring) -> MonitoringFuture<'_> {
    Box::pin(collect_live(service))
}

async fn collect_live(service: &ProviderMonitoring) -> ProviderUsage {
    let path = service.home_dir.join(".claude/.credentials.json");
    let credentials = match read_json(&path) {
        Ok(value) => value,
        Err(error) => return ProviderUsage::error(ProviderKey::Claude, error),
    };
    let Some(token) =
        deep_find(&credentials, &["accessToken", "access_token"]).and_then(Value::as_str)
    else {
        return ProviderUsage::error(ProviderKey::Claude, "No access token in credentials");
    };
    let response = match service
        .client
        .get(&service.config.claude_usage_url)
        .bearer_auth(token)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("anthropic-version", "2023-06-01")
        .header("User-Agent", "claude-cli/usage-monitor")
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return ProviderUsage::error(ProviderKey::Claude, error.to_string()),
    };
    let data = match response_json(response).await {
        Ok(data) => data,
        Err(error) => return ProviderUsage::error(ProviderKey::Claude, error),
    };
    let mut windows = Vec::new();
    let claims = [
        ("five_hour", "Current session", "5h"),
        ("5_hour", "Current session", "5h"),
        ("5h", "Current session", "5h"),
        ("seven_day", "Current week (all models)", "weekly"),
        ("weekly", "Current week (all models)", "weekly"),
        ("7d", "Current week (all models)", "weekly"),
        ("seven_day_opus", "Current week (Opus only)", "weekly"),
        ("seven_day_sonnet", "Current week (Sonnet only)", "weekly"),
        ("overage", "Usage credits", "monthly"),
    ];
    if let Some(object) = data.as_object() {
        for (key, label, period) in claims {
            let Some(window) = object.get(key).and_then(Value::as_object) else {
                continue;
            };
            let used = percent(window, &["used_percentage", "used_percent", "utilization"]);
            let remaining = percent(
                window,
                &["remaining_percentage", "remaining_percent", "remaining"],
            );
            let resets = window
                .get("resets_at")
                .or_else(|| window.get("resetsAt"))
                .or_else(|| window.get("reset_at"))
                .or_else(|| window.get("resetAt"))
                .and_then(timestamp);
            if used.is_some() || remaining.is_some() || resets.is_some() {
                windows.push(QuotaWindow::new(label, period, used, remaining, resets));
            }
        }
    }
    if windows.is_empty() {
        return ProviderUsage::error(
            ProviderKey::Claude,
            "connected but no usage windows in response",
        );
    }
    let mut result = ProviderUsage::success(ProviderKey::Claude, windows);
    result.plan = deep_find(&data, &["plan", "subscription", "tier"]).and_then(value_string);
    result
}
