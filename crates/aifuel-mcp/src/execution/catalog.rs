use aifuel_core::{ProviderKey, RunManagementError};
use serde_json::{Value, json};

use super::{only_fields, optional_string};

pub(super) fn list_models<F>(
    args: &Value,
    cached: &mut Vec<Value>,
    refresh_catalog: &F,
) -> Result<Value, RunManagementError>
where
    F: Fn(Option<ProviderKey>) -> Result<Vec<Value>, String>,
{
    only_fields(args, &["provider", "refresh"])?;
    let provider = optional_string(args, "provider")?
        .map(str::parse)
        .transpose()
        .map_err(|error: aifuel_core::InvalidProviderKey| {
            RunManagementError::invalid_request(error.to_string())
        })?;
    let refresh = args
        .get("refresh")
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| RunManagementError::invalid_request("refresh must be a boolean"))
        })
        .transpose()?
        .unwrap_or(false);
    let mut snapshots = cached
        .iter()
        .filter(|snapshot| includes_provider(snapshot, provider))
        .cloned()
        .collect::<Vec<_>>();
    let mut diagnostics = Vec::new();
    let mut refresh_states = Vec::new();

    if refresh {
        match refresh_catalog(provider) {
            Ok(results) => {
                for result in results
                    .iter()
                    .filter(|result| includes_provider(result, provider))
                {
                    let status = result["status"].as_str().unwrap_or("unknown");
                    refresh_states.push(status.to_owned());
                    if let Some(snapshot) = refreshed_snapshot(result) {
                        replace_snapshot(&mut snapshots, snapshot);
                        replace_snapshot(cached, snapshot);
                    }
                    if status != "updated" {
                        diagnostics.push(json!({
                            "provider": result_provider(result),
                            "code": status,
                            "message": result["diagnostic"].as_str().unwrap_or("catalog refresh returned no diagnostic")
                        }));
                    }
                }
                if results.is_empty() {
                    diagnostics.push(json!({
                        "provider": provider.map(|value| value.to_string()),
                        "code": "refresh_failed",
                        "message": "catalog refresh returned no provider results"
                    }));
                    refresh_states.push("failed".to_owned());
                }
            }
            Err(error) => {
                diagnostics.push(json!({
                    "provider": provider.map(|value| value.to_string()),
                    "code": "refresh_failed",
                    "message": error
                }));
                refresh_states.push("failed".to_owned());
            }
        }
    }

    if snapshots.is_empty() && diagnostics.is_empty() {
        diagnostics.push(json!({
            "provider": provider.map(|value| value.to_string()),
            "code": "catalog_unknown",
            "message": "model discovery has not produced a cached catalog"
        }));
    }
    let models = snapshots
        .iter()
        .flat_map(|snapshot| snapshot["models"].as_array().into_iter().flatten())
        .cloned()
        .collect::<Vec<_>>();
    let freshness = catalog_freshness(&snapshots);
    let age_seconds = snapshots
        .iter()
        .filter_map(|snapshot| snapshot["age_seconds"].as_u64())
        .max();
    Ok(json!({
        "schema_version": 1,
        "provider": provider.map(|value| value.to_string()),
        "models": models,
        "catalogs": snapshots,
        "evidence": if snapshots.is_empty() { "unknown" } else { "cached" },
        "freshness": freshness,
        "age_seconds": age_seconds,
        "refresh_status": if refresh { summarize_refresh(&refresh_states) } else { "not_requested" },
        "diagnostics": diagnostics
    }))
}

fn includes_provider(snapshot: &Value, provider: Option<ProviderKey>) -> bool {
    provider.is_none_or(|provider| {
        ["scope", "snapshot", "retained"]
            .iter()
            .any(|key| snapshot[*key]["scope"]["provider"] == provider.to_string())
            || snapshot["scope"]["provider"] == provider.to_string()
    })
}

fn result_provider(result: &Value) -> Option<&str> {
    result["scope"]["provider"]
        .as_str()
        .or_else(|| result["snapshot"]["scope"]["provider"].as_str())
        .or_else(|| result["retained"]["scope"]["provider"].as_str())
}

fn refreshed_snapshot(result: &Value) -> Option<&Value> {
    result["snapshot"]
        .as_object()
        .map(|_| &result["snapshot"])
        .or_else(|| result["retained"].as_object().map(|_| &result["retained"]))
}

fn replace_snapshot(snapshots: &mut Vec<Value>, replacement: &Value) {
    let mut replacement = replacement.clone();
    if replacement["age_seconds"].as_u64().is_none() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let age = replacement["refreshed_at"]
            .as_u64()
            .map_or(0, |refreshed_at| now.saturating_sub(refreshed_at));
        replacement["age_seconds"] = Value::from(age);
    }
    let scope = &replacement["scope"];
    snapshots.retain(|snapshot| snapshot["scope"] != *scope);
    snapshots.push(replacement);
}

fn catalog_freshness(snapshots: &[Value]) -> &'static str {
    if snapshots.is_empty() {
        "unknown"
    } else if snapshots
        .iter()
        .all(|snapshot| snapshot["freshness"] == "fresh")
    {
        "fresh"
    } else if snapshots
        .iter()
        .any(|snapshot| snapshot["freshness"] == "fresh")
    {
        "mixed"
    } else {
        "stale"
    }
}

fn summarize_refresh(states: &[String]) -> &'static str {
    if states.is_empty() {
        "failed"
    } else if states.iter().all(|state| state == "updated") {
        "updated"
    } else if states.iter().all(|state| state == "unsupported") {
        "unsupported"
    } else if states.iter().all(|state| state == "failed") {
        "failed"
    } else {
        "mixed"
    }
}
