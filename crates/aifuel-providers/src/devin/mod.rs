use super::{CatalogProvider, MonitoringFuture, ProviderMonitoring};
use crate::usage_helpers::{number, response_json, timestamp, value_string};
use aifuel_core::{ProviderKey, ProviderUsage, QuotaWindow};
use std::fs;
use toml_edit::DocumentMut;

mod agent_run;
pub(crate) use agent_run::ADAPTER as AGENT_RUN_ADAPTER;
mod registration;
pub(crate) use registration::ADAPTER as MCP_REGISTRATION_ADAPTER;

const CREDENTIAL_PATHS: &[&str] = &[
    ".local/share/devin/credentials.toml",
    "Library/Application Support/devin/credentials.toml",
    "AppData/Roaming/devin/credentials.toml",
];

pub static DEFINITION: CatalogProvider = CatalogProvider::files_source(
    ProviderKey::Devin,
    &[
        ".local/share/devin/credentials.toml",
        "Library/Application Support/devin/credentials.toml",
        "AppData/Roaming/devin/credentials.toml",
    ],
);

pub(crate) fn collect(service: &ProviderMonitoring) -> MonitoringFuture<'_> {
    Box::pin(collect_live(service))
}

async fn collect_live(service: &ProviderMonitoring) -> ProviderUsage {
    let Some(path) = CREDENTIAL_PATHS
        .iter()
        .map(|relative| service.home_dir.join(relative))
        .find(|path| path.is_file())
    else {
        return ProviderUsage::error(ProviderKey::Devin, "No Devin credentials.toml found");
    };
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) => return ProviderUsage::error(ProviderKey::Devin, error.to_string()),
    };
    let credentials = match raw.parse::<DocumentMut>() {
        Ok(document) => document,
        Err(error) => {
            return ProviderUsage::error(
                ProviderKey::Devin,
                format!("Devin credentials.toml is malformed: {error}"),
            );
        }
    };
    let Some(api_key) = credentials
        .get("windsurf_api_key")
        .and_then(|item| item.as_str())
    else {
        return ProviderUsage::error(
            ProviderKey::Devin,
            "No windsurf_api_key in Devin credentials",
        );
    };
    let base = credentials
        .get("api_server_url")
        .and_then(|item| item.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(service.config.devin_api_server_url.as_str())
        .trim_end_matches('/');
    let response = match service
        .client
        .post(format!(
            "{base}/exa.seat_management_pb.SeatManagementService/GetUserStatus"
        ))
        .header("Connect-Protocol-Version", "1")
        .header("User-Agent", "devin/usage-monitor")
        .json(&serde_json::json!({
            "metadata": {
                "apiKey": api_key,
                "ideName": "devin",
                "ideVersion": "3000.11.3",
                "extensionName": "devin",
                "extensionVersion": "3000.11.3",
                "locale": "en",
            }
        }))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return ProviderUsage::error(ProviderKey::Devin, error.to_string()),
    };
    let data = match response_json(response).await {
        Ok(data) => data,
        Err(error) => return ProviderUsage::error(ProviderKey::Devin, error),
    };
    let plan_status = data
        .get("userStatus")
        .and_then(|status| status.get("planStatus"));
    let mut windows = Vec::new();
    for (label, period, remaining_key, reset_key) in [
        (
            "Daily quota",
            "daily",
            "dailyQuotaRemainingPercent",
            "dailyQuotaResetAtUnix",
        ),
        (
            "Weekly quota",
            "weekly",
            "weeklyQuotaRemainingPercent",
            "weeklyQuotaResetAtUnix",
        ),
    ] {
        let remaining = plan_status
            .and_then(|status| status.get(remaining_key))
            .and_then(number);
        let resets = plan_status
            .and_then(|status| status.get(reset_key))
            .and_then(timestamp);
        if remaining.is_some() || resets.is_some() {
            windows.push(QuotaWindow::new(label, period, None, remaining, resets));
        }
    }
    if let Some(micros) = plan_status
        .and_then(|status| status.get("overageBalanceMicros"))
        .and_then(number)
    {
        let mut window = QuotaWindow::new("Overage credits", "monthly", None, None, None);
        window.limit = Some(micros / 1_000_000.0);
        windows.push(window);
    }
    if windows.is_empty() {
        return ProviderUsage::error(
            ProviderKey::Devin,
            "connected but no usage windows in response",
        );
    }
    let mut result = ProviderUsage::success(ProviderKey::Devin, windows);
    let plan_info = plan_status.and_then(|status| status.get("planInfo"));
    result.plan = plan_info
        .and_then(|info| info.get("planName"))
        .and_then(value_string);
    result.account_id = plan_info
        .and_then(|info| info.get("devinInfo"))
        .and_then(|info| info.get("orgId"))
        .and_then(value_string)
        .or_else(|| {
            data.get("userStatus")
                .and_then(|status| status.get("teamId"))
                .and_then(value_string)
        });
    result
}
