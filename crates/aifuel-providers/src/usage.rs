use crate::code_assist::{collect_antigravity, collect_gemini};
use crate::usage_helpers::*;
use aifuel_core::{
    ProviderKey, ProviderUsage, QuotaWindow, ResetCredit, ResetCredits, StatusReport,
};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct CollectionConfig {
    pub claude_usage_url: String,
    pub codex_usage_url: String,
    pub copilot_user_url: String,
    pub copilot_token_url: String,
    pub gemini_api_url: String,
}

impl Default for CollectionConfig {
    fn default() -> Self {
        Self {
            claude_usage_url: "https://api.anthropic.com/api/oauth/usage".to_owned(),
            codex_usage_url: "https://chatgpt.com/backend-api/codex/usage".to_owned(),
            copilot_user_url: "https://api.github.com/copilot_internal/user".to_owned(),
            copilot_token_url: "https://api.github.com/copilot_internal/v2/token".to_owned(),
            gemini_api_url: "https://cloudcode-pa.googleapis.com/v1internal:".to_owned(),
        }
    }
}

impl CollectionConfig {
    pub fn from_environment() -> Self {
        let mut config = Self::default();
        replace_from_env(&mut config.claude_usage_url, "AIFUEL_CLAUDE_USAGE_URL");
        replace_from_env(&mut config.codex_usage_url, "AIFUEL_CODEX_USAGE_URL");
        replace_from_env(&mut config.copilot_user_url, "AIFUEL_COPILOT_USER_URL");
        replace_from_env(&mut config.copilot_token_url, "AIFUEL_COPILOT_TOKEN_URL");
        replace_from_env(&mut config.gemini_api_url, "AIFUEL_GEMINI_API_URL");
        config
    }
}

fn replace_from_env(target: &mut String, name: &str) {
    if let Ok(value) = std::env::var(name) {
        if !value.trim().is_empty() {
            *target = value;
        }
    }
}

pub struct UsageService {
    pub(crate) home_dir: PathBuf,
    pub(crate) config: CollectionConfig,
    pub(crate) client: Client,
    cache: Mutex<Option<(Instant, StatusReport)>>,
}

impl UsageService {
    pub fn new(home_dir: impl Into<PathBuf>, config: CollectionConfig) -> Result<Self, String> {
        let client = Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|error| format!("could not create HTTP client: {error}"))?;
        Ok(Self {
            home_dir: home_dir.into(),
            config,
            client,
            cache: Mutex::new(None),
        })
    }

    pub fn home_dir(&self) -> &Path {
        &self.home_dir
    }

    pub async fn collect(&self) -> StatusReport {
        self.status(true).await
    }

    pub async fn status(&self, refresh: bool) -> StatusReport {
        if !refresh {
            if let Some((created_at, report)) = self.cache.lock().expect("cache mutex").as_ref() {
                if created_at.elapsed() < Duration::from_secs(300) {
                    return report.clone();
                }
            }
        }
        let report = self.collect_live().await;
        *self.cache.lock().expect("cache mutex") = Some((Instant::now(), report.clone()));
        report
    }

    async fn collect_live(&self) -> StatusReport {
        let discovery_context = crate::DiscoveryContext::new(&self.home_dir);
        let selection = crate::default_registry().discover_and_initialize(&discovery_context);
        let discovered: HashSet<_> = selection
            .report()
            .providers
            .iter()
            .map(|provider| provider.key)
            .collect();
        let discovery_errors = selection.report().discovery_errors.clone();

        let claude = collect_if(discovered.contains(&ProviderKey::Claude), || {
            collect_claude(self)
        });
        let codex = collect_if(discovered.contains(&ProviderKey::Codex), || {
            collect_codex(self)
        });
        let copilot = collect_if(discovered.contains(&ProviderKey::Copilot), || {
            collect_copilot(self)
        });
        let gemini = collect_if(discovered.contains(&ProviderKey::Gemini), || {
            collect_gemini(self)
        });
        let antigravity = collect_if(discovered.contains(&ProviderKey::Antigravity), || {
            collect_antigravity(self)
        });
        let (claude, codex, copilot, gemini, antigravity) =
            tokio::join!(claude, codex, copilot, gemini, antigravity);
        let providers = [claude, codex, copilot, gemini, antigravity]
            .into_iter()
            .flatten()
            .collect();

        let mut report = StatusReport::from_usage(unix_timestamp(), providers, discovery_errors);
        report.catalog = crate::catalog::statuses();
        report
    }
}

async fn collect_if<F, Fut>(discovered: bool, collect: F) -> Option<ProviderUsage>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ProviderUsage>,
{
    if discovered {
        Some(collect().await)
    } else {
        None
    }
}

async fn collect_claude(service: &UsageService) -> ProviderUsage {
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

async fn collect_codex(service: &UsageService) -> ProviderUsage {
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
    for (name, prefix) in [("primary_window", None), ("secondary_window", None)] {
        if let Some(window) = rate_limit.get(name).and_then(parse_codex_window) {
            windows.push(window.with_prefix(prefix));
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
            if let Some(window) = extra_object
                .get("rate_limit")
                .and_then(Value::as_object)
                .and_then(|rate| rate.get("primary_window"))
                .and_then(parse_codex_window)
            {
                windows.push(window.with_prefix(Some(prefix)));
            }
            if let Some(window) = extra_object
                .get("rate_limit")
                .and_then(Value::as_object)
                .and_then(|rate| rate.get("secondary_window"))
                .and_then(parse_codex_window)
            {
                windows.push(window.with_prefix(Some(prefix)));
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

struct ParsedWindow(QuotaWindow);

impl ParsedWindow {
    fn with_prefix(self, prefix: Option<&str>) -> QuotaWindow {
        let mut window = self.0;
        if let Some(prefix) = prefix {
            window.label = format!("{prefix} {}", window.label);
        }
        window
    }
}

fn parse_codex_window(value: &Value) -> Option<ParsedWindow> {
    let object = value.as_object()?;
    let seconds = object.get("limit_window_seconds").and_then(number)?;
    let (period, label) = period_for_seconds(seconds);
    let reset = object.get("reset_at").and_then(timestamp).or_else(|| {
        object
            .get("reset_after_seconds")
            .and_then(number)
            .map(|n| unix_timestamp() + n)
    });
    Some(ParsedWindow(QuotaWindow::new(
        label,
        period,
        object.get("used_percent").and_then(percent_value),
        None,
        reset,
    )))
}

async fn collect_copilot(service: &UsageService) -> ProviderUsage {
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
