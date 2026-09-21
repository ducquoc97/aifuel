use super::{CatalogProvider, MonitoringFuture, ProviderMonitoring};
use crate::usage_helpers::{
    deep_find, number, percent_value, period_for_seconds, read_json, response_json, timestamp,
    unix_timestamp, value_string,
};
use aifuel_core::{ProviderKey, ProviderUsage, QuotaWindow, ResetCredit, ResetCredits};
use serde_json::Value;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

const APP_SERVER_TIMEOUT: Duration = Duration::from_secs(12);
const APP_SERVER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

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
    result.reset_credits = reset_credits_for_usage(&data).await;
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
    data.get("rate_limit_reset_credits")
        .or_else(|| data.get("rateLimitResetCredits"))
        .and_then(parse_reset_credits_value)
}

async fn reset_credits_for_usage(data: &Value) -> Option<ResetCredits> {
    let reported = parse_reset_credits(data)?;
    if !reported.credits.is_empty() {
        return Some(reported);
    }
    app_server_reset_credits().await.or(Some(reported))
}

async fn app_server_reset_credits() -> Option<ResetCredits> {
    app_server_reset_credits_from("codex").await
}

async fn app_server_reset_credits_from(program: &str) -> Option<ResetCredits> {
    let deadline = Instant::now() + APP_SERVER_TIMEOUT;
    let mut child = None;
    for candidate in crate::agent_execution::program_candidates(program) {
        match Command::new(&candidate)
            .args(["app-server", "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(process) => {
                child = Some(process);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    let mut child = child?;
    let mut stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    let mut stdout = BufReader::new(stdout);
    let result = app_server_reset_credits_io(&mut stdin, &mut stdout, deadline).await;
    let _ = timeout(APP_SERVER_SHUTDOWN_TIMEOUT, child.kill()).await;
    let _ = timeout(APP_SERVER_SHUTDOWN_TIMEOUT, child.wait()).await;
    result
}

async fn app_server_reset_credits_io(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    deadline: Instant,
) -> Option<ResetCredits> {
    send_app_server_message(
        stdin,
        serde_json::json!({
            "id": 0,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "aifuel",
                    "title": "aifuel",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            },
        }),
        deadline,
    )
    .await?;
    let initialized = read_app_server_response(stdout, 0, deadline).await?;
    if initialized
        .get("error")
        .is_some_and(|error| !error.is_null())
    {
        return None;
    }
    send_app_server_message(
        stdin,
        serde_json::json!({
            "method": "initialized",
            "params": {},
        }),
        deadline,
    )
    .await?;
    send_app_server_message(
        stdin,
        serde_json::json!({
            "id": 1,
            "method": "account/rateLimits/read",
        }),
        deadline,
    )
    .await?;
    let response = read_app_server_response(stdout, 1, deadline).await?;
    let credits = response
        .get("result")
        .and_then(|result| result.get("rateLimitResetCredits"))?;
    parse_reset_credits_value(credits)
}

async fn send_app_server_message(
    stdin: &mut ChildStdin,
    message: Value,
    deadline: Instant,
) -> Option<()> {
    let mut line = serde_json::to_vec(&message).ok()?;
    line.push(b'\n');
    timeout(
        deadline.checked_duration_since(Instant::now())?,
        stdin.write_all(&line),
    )
    .await
    .ok()?
    .ok()?;
    timeout(
        deadline.checked_duration_since(Instant::now())?,
        stdin.flush(),
    )
    .await
    .ok()?
    .ok()?;
    Some(())
}

async fn read_app_server_response(
    stdout: &mut BufReader<ChildStdout>,
    response_id: i64,
    deadline: Instant,
) -> Option<Value> {
    let mut line = String::new();
    loop {
        line.clear();
        let read = timeout(
            deadline.checked_duration_since(Instant::now())?,
            stdout.read_line(&mut line),
        )
        .await
        .ok()?
        .ok()?;
        if read == 0 {
            return None;
        }
        let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if message.get("id").and_then(Value::as_i64) == Some(response_id) {
            return Some(message);
        }
    }
}

fn parse_reset_credits_value(value: &Value) -> Option<ResetCredits> {
    let value = value.as_object()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_app_server_reset_credit_details() {
        let data = serde_json::json!({
            "rateLimitResetCredits": {
                "availableCount": 2,
                "credits": [{
                    "resetType": "codexRateLimits",
                    "title": "Full reset",
                    "description": "Ready to redeem",
                    "expiresAt": 1_900_000_000_i64
                }]
            }
        });

        let parsed = parse_reset_credits(&data).expect("reset credits should parse");

        assert_eq!(parsed.available_count, 2);
        assert_eq!(parsed.credits.len(), 1);
        assert_eq!(parsed.credits[0].title.as_deref(), Some("Full reset"));
        assert_eq!(parsed.credits[0].expires_at, Some(1_900_000_000.0));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reads_reset_credit_details_through_the_codex_app_server_process() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::time::{SystemTime, UNIX_EPOCH};

        let path = std::env::temp_dir().join(format!(
            "aifuel-codex-app-server-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("test clock should be after the epoch")
                .as_nanos()
        ));
        fs::write(
            &path,
            r##"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"id":0'*) printf '%s\n' '{"id":0,"result":{}}' ;;
    *'"id":1'*) printf '%s\n' '{"id":1,"result":{"rateLimitResetCredits":{"availableCount":2,"credits":[{"resetType":"codexRateLimits","title":"Full reset","expiresAt":1900000000}]}}}' ; break ;;
  esac
done
"##,
        )
        .expect("app-server fixture should be writable");
        let mut permissions = fs::metadata(&path)
            .expect("app-server fixture metadata should be readable")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("app-server fixture should be executable");

        let parsed = app_server_reset_credits_from(path.to_str().expect("fixture path is UTF-8"))
            .await
            .expect("app-server fixture should return reset credits");

        assert_eq!(parsed.available_count, 2);
        assert_eq!(parsed.credits[0].title.as_deref(), Some("Full reset"));
        assert_eq!(parsed.credits[0].expires_at, Some(1_900_000_000.0));
        fs::remove_file(path).expect("app-server fixture should be removed");
    }
}
