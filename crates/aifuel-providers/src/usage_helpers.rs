use super::ProviderMonitoring;
use aifuel_core::QuotaWindow;
use chrono::{DateTime, Datelike};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) async fn post_json(
    service: &ProviderMonitoring,
    url: &str,
    token: &str,
    body: Value,
    user_agent: &str,
) -> Result<Value, String> {
    let response = service
        .client
        .post(url)
        .bearer_auth(token)
        .header("User-Agent", user_agent)
        .json(&body)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    response_json(response).await
}

pub(crate) async fn response_json(response: reqwest::Response) -> Result<Value, String> {
    let status = response.status();
    let body = response.text().await.map_err(|error| error.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    serde_json::from_str(&body).map_err(|error| format!("invalid JSON response: {error}"))
}

pub(crate) fn json_metadata() -> Value {
    serde_json::json!({"metadata": {"pluginType": "GEMINI"}})
}

pub(crate) fn quota_windows(quota: &Value, period: Option<&str>) -> Vec<QuotaWindow> {
    quota
        .get("buckets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|bucket| {
            let model = bucket.get("modelId").and_then(Value::as_str)?;
            if model.starts_with("tab_")
                || (model.starts_with("chat_") && model[5..].chars().all(|c| c.is_ascii_digit()))
            {
                return None;
            }
            let label = match model {
                "gemini-3-flash" => "gemini-3.5-flash",
                "gemini-3.1-pro-preview-customtools" => "gemini-3.1-pro-preview",
                value => value,
            };
            let remaining = bucket
                .get("remainingFraction")
                .and_then(number)
                .map(|value| value * 100.0);
            let reset = bucket.get("resetTime").and_then(timestamp);
            if remaining.is_none() && bucket.get("remainingAmount").is_none() {
                return None;
            }
            Some(QuotaWindow::new(
                label,
                period.unwrap_or("unknown"),
                None,
                remaining,
                reset,
            ))
        })
        .collect()
}

pub(crate) fn rank_windows(mut windows: Vec<QuotaWindow>) -> Vec<QuotaWindow> {
    windows.sort_by(|left, right| {
        left.remaining_percent
            .is_none()
            .cmp(&right.remaining_percent.is_none())
            .then_with(|| {
                left.remaining_percent
                    .partial_cmp(&right.remaining_percent)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| left.resets_at.is_none().cmp(&right.resets_at.is_none()))
    });
    windows
}

/// True when a collection failure means the stored credential was rejected,
/// which is the only failure a credential refresh can repair. A 403 is left
/// alone because it reports a permission problem fresh credentials will not
/// fix, so a re-authentication hint would mislead.
pub(crate) fn is_auth_error(detail: &str) -> bool {
    detail.contains("HTTP 401")
}

/// Error text guiding the user to re-authenticate the provider's own CLI when
/// stored credentials cannot be renewed from local material.
pub(crate) fn with_reauth_hint(detail: &str, command: &str) -> String {
    if is_auth_error(detail) {
        format!("{detail} - re-authenticate with `{command}`")
    } else {
        detail.to_owned()
    }
}

/// True when the credential document records an expiry that has passed, so a
/// request can be refreshed before spending a doomed API call.
pub(crate) fn credentials_expired(credentials: &Value) -> bool {
    deep_find(
        credentials,
        &["expiresAt", "expires_at", "expiry_date", "expired"],
    )
    .and_then(timestamp)
    .is_some_and(|expiry| expiry <= unix_timestamp())
}

pub(crate) fn read_json(path: &Path) -> Result<Value, String> {
    let content = fs::read_to_string(path)
        .map_err(|error| format!("could not read provider credentials: {error}"))?;
    serde_json::from_str(&content)
        .map_err(|error| format!("provider credentials are invalid JSON: {error}"))
}

pub(crate) fn deep_find<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    match value {
        Value::Object(object) => {
            for key in keys {
                if let Some(value) = object.get(*key)
                    && !value.is_null()
                    && value != ""
                {
                    return Some(value);
                }
            }
            object.values().find_map(|value| deep_find(value, keys))
        }
        Value::Array(values) => values.iter().find_map(|value| deep_find(value, keys)),
        _ => None,
    }
}

pub(crate) fn value_string(value: &Value) -> Option<String> {
    value.as_str().map(ToOwned::to_owned).or_else(|| {
        value
            .as_i64()
            .map(|number| number.to_string())
            .or_else(|| value.as_f64().map(|number| number.to_string()))
    })
}

pub(crate) fn project_id(value: &Value) -> Option<String> {
    value
        .get("id")
        .or_else(|| value.get("name"))
        .and_then(value_string)
        .or_else(|| value.as_str().map(ToOwned::to_owned))
}

pub(crate) fn number(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.parse().ok())
}

pub(crate) fn percent(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| object.get(*key))
        .and_then(percent_value)
}

pub(crate) fn percent_value(value: &Value) -> Option<f64> {
    let value = number(value)?;
    Some(if value <= 1.0 { value * 100.0 } else { value })
}

pub(crate) fn timestamp(value: &Value) -> Option<f64> {
    if let Some(number) = number(value) {
        return Some(if number > 1_000_000_000_000.0 {
            number / 1000.0
        } else {
            number
        });
    }
    let text = value_string(value)?;
    DateTime::parse_from_rfc3339(&text)
        .ok()
        .map(|date| date.timestamp_millis() as f64 / 1000.0)
}

pub(crate) fn period_for_seconds(seconds: f64) -> (&'static str, String) {
    let minutes = seconds / 60.0;
    if minutes <= 360.0 {
        ("5h", format!("{}-hour", (minutes / 60.0).round() as i64))
    } else if minutes <= 1500.0 {
        ("daily", "Daily".to_owned())
    } else if minutes <= 20_160.0 {
        ("weekly", "Weekly".to_owned())
    } else {
        ("monthly", "Monthly".to_owned())
    }
}

pub(crate) fn next_month_first() -> f64 {
    let now = chrono::Utc::now();
    let (year, month) = if now.month() == 12 {
        (now.year() + 1, 1)
    } else {
        (now.year(), now.month() + 1)
    };
    chrono::NaiveDate::from_ymd_opt(year, month, 1)
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|date| date.and_utc().timestamp() as f64)
        .unwrap_or_else(unix_timestamp)
}

pub(crate) fn project_from_environment() -> Option<String> {
    std::env::var("GOOGLE_CLOUD_PROJECT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| std::env::var("GOOGLE_CLOUD_PROJECT_ID").ok())
}

pub(crate) fn unix_timestamp() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_expired_reads_millisecond_expiry_fields() {
        let expired = serde_json::json!({"claudeAiOauth": {"expiresAt": 1_600_000_000_000_i64}});
        assert!(credentials_expired(&expired));

        let fresh = serde_json::json!({"expiry_date": 9_999_999_999_999_i64});
        assert!(!credentials_expired(&fresh));
    }

    #[test]
    fn credentials_without_expiry_are_not_treated_as_expired() {
        // A missing expiry must not disable a token that may still be valid.
        let credentials = serde_json::json!({"access_token": "tok"});
        assert!(!credentials_expired(&credentials));
    }

    #[test]
    fn only_rejected_credential_errors_trigger_a_refresh_attempt() {
        assert!(is_auth_error("loadCodeAssist HTTP 401"));
        // A 403 is a permission denial, not an expired credential; hinting a
        // re-authentication would send users down the wrong path.
        assert!(!is_auth_error("HTTP 403"));
        assert!(!is_auth_error("HTTP 500"));
        assert!(!is_auth_error("connection refused"));
    }

    #[test]
    fn reauth_hint_is_appended_only_for_auth_failures() {
        assert_eq!(
            with_reauth_hint("HTTP 401", "codex login"),
            "HTTP 401 - re-authenticate with `codex login`"
        );
        assert_eq!(
            with_reauth_hint("invalid JSON response", "codex login"),
            "invalid JSON response"
        );
    }
}
