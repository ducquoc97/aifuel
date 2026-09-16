use super::usage::UsageService;
use super::usage_helpers::{
    deep_find, json_metadata, post_json, project_from_environment, project_id, quota_windows,
    rank_windows, read_json, value_string,
};
use aifuel_core::{ProviderKey, ProviderUsage, QuotaWindow};
use serde_json::Value;

pub(crate) enum CodeAssistPeriod {
    Daily,
    Unknown,
}

impl CodeAssistPeriod {
    fn as_str(&self) -> Option<&'static str> {
        match self {
            Self::Daily => Some("daily"),
            Self::Unknown => None,
        }
    }
}

pub(crate) async fn collect_gemini(service: &UsageService) -> ProviderUsage {
    collect_file_code_assist(
        service,
        ProviderKey::Gemini,
        ".gemini/oauth_creds.json",
        project_from_environment(),
        "gemini-cli/usage-monitor",
        CodeAssistPeriod::Daily,
    )
    .await
}

pub(crate) async fn collect_antigravity(service: &UsageService) -> ProviderUsage {
    let project = service
        .home_dir
        .join(".gemini/antigravity-cli/settings.json");
    let project = read_json(&project).ok().and_then(|value| {
        value
            .get("gcp")
            .and_then(|gcp| gcp.get("project"))
            .and_then(value_string)
    });
    collect_file_code_assist(
        service,
        ProviderKey::Antigravity,
        ".gemini/antigravity-cli/antigravity-oauth-token",
        project,
        "antigravity/usage-monitor",
        CodeAssistPeriod::Unknown,
    )
    .await
}

async fn collect_file_code_assist(
    service: &UsageService,
    provider: ProviderKey,
    credential_relative_path: &str,
    project: Option<String>,
    user_agent: &str,
    period: CodeAssistPeriod,
) -> ProviderUsage {
    let credentials = match read_json(&service.home_dir.join(credential_relative_path)) {
        Ok(value) => value,
        Err(error) => return ProviderUsage::error(provider, error),
    };
    let Some(token) =
        deep_find(&credentials, &["access_token", "accessToken"]).and_then(Value::as_str)
    else {
        return ProviderUsage::error(provider, "No access token in provider credentials");
    };
    let (plan, windows, detail) =
        collect_code_assist(service, token, project.as_deref(), user_agent, period).await;
    if let Some(detail) = detail {
        let mut result = ProviderUsage::error(provider, detail);
        result.plan = plan;
        return result;
    }
    let mut result = ProviderUsage::success(provider, rank_windows(windows));
    result.plan = plan;
    result
}

async fn collect_code_assist(
    service: &UsageService,
    token: &str,
    project_hint: Option<&str>,
    user_agent: &str,
    period: CodeAssistPeriod,
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
    let windows = quota_windows(&quota, period.as_str());
    if windows.is_empty() {
        return (
            plan,
            Vec::new(),
            Some("Quota returned no model buckets".to_owned()),
        );
    }
    (plan, windows, None)
}
