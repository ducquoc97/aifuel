use super::ProviderMonitoring;
use super::usage_helpers::{
    credentials_expired, deep_find, is_auth_error, json_metadata, post_json, project_id,
    quota_windows, rank_windows, read_json, response_json, value_string,
};
use aifuel_core::{ProviderKey, ProviderUsage, QuotaWindow};
use serde_json::Value;

const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Per-provider credential recovery: the provider's own public installed-app
/// OAuth client, used to exchange the stored Google refresh token in memory
/// (Google does not rotate refresh grants, so this cannot invalidate the
/// provider CLI's credentials), plus the command that re-authenticates when
/// renewal is impossible.
pub(crate) struct CredentialRecovery {
    pub client_id: &'static str,
    pub client_secret: &'static str,
    pub reauth: &'static str,
}

pub(crate) async fn collect(
    service: &ProviderMonitoring,
    provider: ProviderKey,
    credential_relative_path: &str,
    project: Option<String>,
    user_agent: &str,
    period: Option<&str>,
    recovery: &CredentialRecovery,
) -> ProviderUsage {
    let credentials = match read_json(&service.home_dir.join(credential_relative_path)) {
        Ok(value) => value,
        Err(error) => return ProviderUsage::error(provider, error),
    };
    let Some(mut token) = deep_find(&credentials, &["access_token", "accessToken"])
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
    else {
        return ProviderUsage::error(provider, "No access token in provider credentials");
    };
    let expired = credentials_expired(&credentials);
    let mut renewed = false;
    if expired && let Some(fresh) = google_access_token(service, &credentials, recovery).await {
        token = fresh;
        renewed = true;
    }
    let (mut plan, mut windows, mut detail) =
        collect_quota(service, &token, project.as_deref(), user_agent, period).await;
    if let Some(detail_text) = &detail
        && is_auth_error(detail_text)
        && !renewed
        && let Some(fresh) = google_access_token(service, &credentials, recovery).await
    {
        renewed = true;
        (plan, windows, detail) =
            collect_quota(service, &fresh, project.as_deref(), user_agent, period).await;
    }
    if let Some(detail) = detail {
        // The call ran on a known-dead credential, or the credential was
        // rejected outright; either way re-authentication is the fix.
        let guided = is_auth_error(&detail) || (expired && !renewed);
        let mut result = ProviderUsage::error(
            provider,
            if guided {
                format!("{detail} - re-authenticate with `{}`", recovery.reauth)
            } else {
                detail
            },
        );
        result.plan = plan;
        return result;
    }
    let mut result = ProviderUsage::success(provider, rank_windows(windows));
    result.plan = plan;
    result
}

async fn google_access_token(
    service: &ProviderMonitoring,
    credentials: &Value,
    recovery: &CredentialRecovery,
) -> Option<String> {
    let refresh_token = deep_find(credentials, &["refresh_token", "refreshToken"])?.as_str()?;
    let body = [
        ("client_id", recovery.client_id),
        ("client_secret", recovery.client_secret),
        ("refresh_token", refresh_token),
        ("grant_type", "refresh_token"),
    ]
    .iter()
    .map(|(key, value)| format!("{key}={}", form_encode(value)))
    .collect::<Vec<_>>()
    .join("&");
    let response = service
        .client
        .post(GOOGLE_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .ok()?;
    let data = response_json(response).await.ok()?;
    data.get("access_token")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

// Hand-rolled because the workspace builds reqwest without the `form` feature.
fn form_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

async fn collect_quota(
    service: &ProviderMonitoring,
    token: &str,
    project_hint: Option<&str>,
    user_agent: &str,
    period: Option<&str>,
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
    let windows = quota_windows(&quota, period);
    if windows.is_empty() {
        return (
            plan,
            Vec::new(),
            Some("Quota returned no model buckets".to_owned()),
        );
    }
    (plan, windows, None)
}
