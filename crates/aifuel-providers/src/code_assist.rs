use super::usage::UsageService;
use super::usage_helpers::{
    deep_find, json_metadata, post_json, project_from_environment, project_id, quota_windows,
    rank_windows, read_json, value_string,
};
use aifuel_core::{ProviderKey, ProviderUsage, QuotaWindow};
use serde_json::Value;

pub(crate) async fn collect_gemini(service: &UsageService) -> ProviderUsage {
    let path = service.home_dir.join(".gemini/oauth_creds.json");
    let credentials = match read_json(&path) {
        Ok(value) => value,
        Err(error) => return ProviderUsage::error(ProviderKey::Gemini, error),
    };
    let Some(token) =
        deep_find(&credentials, &["access_token", "accessToken"]).and_then(Value::as_str)
    else {
        return ProviderUsage::error(ProviderKey::Gemini, "No access token in oauth_creds.json");
    };
    let project = project_from_environment();
    let (plan, windows, detail) = collect_code_assist(
        service,
        token,
        project.as_deref(),
        "gemini-cli/usage-monitor",
        true,
    )
    .await;
    if let Some(detail) = detail {
        let mut result = ProviderUsage::error(ProviderKey::Gemini, detail);
        result.plan = plan;
        return result;
    }
    let mut result = ProviderUsage::success(ProviderKey::Gemini, rank_windows(windows));
    result.plan = plan;
    result
}

pub(crate) async fn collect_antigravity(service: &UsageService) -> ProviderUsage {
    let path = service
        .home_dir
        .join(".gemini/antigravity-cli/antigravity-oauth-token");
    let credentials = match read_json(&path) {
        Ok(value) => value,
        Err(error) => return ProviderUsage::error(ProviderKey::Antigravity, error),
    };
    let Some(token) =
        deep_find(&credentials, &["access_token", "accessToken"]).and_then(Value::as_str)
    else {
        return ProviderUsage::error(
            ProviderKey::Antigravity,
            "No access token in Antigravity credentials",
        );
    };
    let project = service
        .home_dir
        .join(".gemini/antigravity-cli/settings.json");
    let project = read_json(&project).ok().and_then(|value| {
        value
            .get("gcp")
            .and_then(|gcp| gcp.get("project"))
            .and_then(value_string)
    });
    let (plan, windows, detail) = collect_code_assist(
        service,
        token,
        project.as_deref(),
        "antigravity/usage-monitor",
        false,
    )
    .await;
    if let Some(detail) = detail {
        let mut result = ProviderUsage::error(ProviderKey::Antigravity, detail);
        result.plan = plan;
        return result;
    }
    let mut result = ProviderUsage::success(ProviderKey::Antigravity, rank_windows(windows));
    result.plan = plan;
    result
}

async fn collect_code_assist(
    service: &UsageService,
    token: &str,
    project_hint: Option<&str>,
    user_agent: &str,
    daily: bool,
) -> (Option<String>, Vec<QuotaWindow>, Option<String>) {
    let load_url = format!("{}loadCodeAssist", service.config.gemini_api_url);
    let load = match post_json(service, &load_url, token, json_metadata(), user_agent).await {
        Ok(value) => value,
        Err(error) => return (None, Vec::new(), Some(format!("loadCodeAssist {error}"))),
    };
    let tier = load
        .get("currentTier")
        .or_else(|| {
            load.get("allowedTiers")
                .and_then(Value::as_array)
                .and_then(|tiers| {
                    tiers.iter().find(|tier| {
                        tier.get("isDefault")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    })
                })
        })
        .cloned()
        .unwrap_or(Value::Null);
    let plan = tier
        .get("name")
        .or_else(|| tier.get("id"))
        .and_then(value_string);
    let response_project = deep_find(&load, &["cloudaicompanionProject"]).and_then(project_id);
    let project = if tier
        .get("userDefinedCloudaicompanionProject")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        project_hint.or(response_project.as_deref())
    } else {
        response_project.as_deref()
    };
    let Some(project) = project else {
        return (
            plan,
            Vec::new(),
            Some("Code Assist project is not configured".to_owned()),
        );
    };
    let quota_url = format!("{}retrieveUserQuota", service.config.gemini_api_url);
    let quota = match post_json(
        service,
        &quota_url,
        token,
        serde_json::json!({"project": project}),
        user_agent,
    )
    .await
    {
        Ok(value) => value,
        Err(error) => return (plan, Vec::new(), Some(format!("retrieveUserQuota {error}"))),
    };
    let windows = quota_windows(&quota, daily.then_some("daily"));
    if windows.is_empty() {
        return (
            plan,
            Vec::new(),
            Some("Quota returned no model buckets".to_owned()),
        );
    }
    (plan, windows, None)
}
