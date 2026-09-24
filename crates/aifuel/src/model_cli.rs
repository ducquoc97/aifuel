use crate as aifuel;
use aifuel_core::ProviderKey;
use serde_json::Value;

pub fn run(args: &[String]) -> Result<u8, String> {
    if args.is_empty() || args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return Ok(0);
    }
    let command = args[0].as_str();
    let mut provider = None;
    let mut json = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--provider" => {
                if provider.is_some() {
                    return Err("model command accepts one --provider value".to_owned());
                }
                let value = crate::run_cli::next_value(args, &mut index, "--provider")?;
                provider = Some(
                    value
                        .parse()
                        .map_err(|error: aifuel_core::InvalidProviderKey| error.to_string())?,
                );
            }
            "--json" => json = true,
            unknown => return Err(format!("unknown argument {unknown:?} for model {command}")),
        }
        index += 1;
    }

    match command {
        "list" => {
            let snapshots = aifuel::model_catalog_snapshot()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&filter_snapshots(&snapshots, provider))
                        .map_err(|error| format!("could not encode model catalog: {error}"))?
                );
            } else {
                print!("{}", render_catalog_list(&snapshots, provider));
            }
            Ok(0)
        }
        "refresh" => refresh(provider, json),
        unknown => Err(format!(
            "unknown model command {unknown:?}; use list or refresh"
        )),
    }
}

fn refresh(provider: Option<ProviderKey>, json: bool) -> Result<u8, String> {
    let providers = provider.map_or_else(|| ProviderKey::ALL.to_vec(), |provider| vec![provider]);
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| format!("could not start model catalog runtime: {error}"))?;
    let mut results = Vec::with_capacity(providers.len());
    for provider in providers {
        results.push(runtime.block_on(aifuel::refresh_model_catalog(provider))?);
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&results)
                .map_err(|error| format!("could not encode model catalog refresh: {error}"))?
        );
        return Ok(refresh_exit_code(&results));
    }

    let (output, status) = render_refresh_results(&results);
    print!("{output}");
    Ok(status)
}

fn filter_snapshots(snapshots: &[Value], provider: Option<ProviderKey>) -> Vec<Value> {
    snapshots
        .iter()
        .filter(|snapshot| {
            provider.is_none_or(|provider| {
                snapshot["scope"]["provider"]
                    .as_str()
                    .is_some_and(|value| value == provider.to_string())
            })
        })
        .cloned()
        .collect()
}

fn render_catalog_list(snapshots: &[Value], provider: Option<ProviderKey>) -> String {
    let snapshots = filter_snapshots(snapshots, provider);
    if snapshots.is_empty() {
        return match provider {
            Some(provider) => format!("No cached model catalog evidence for {provider}.\n"),
            None => "No cached model catalog evidence.\n".to_owned(),
        };
    }

    let mut output = String::new();
    for snapshot in snapshots {
        let scope = &snapshot["scope"];
        let provider = scope["provider"].as_str().unwrap_or("unknown");
        let version = scope["integration_version"].as_str().unwrap_or("unknown");
        let platform = scope["platform"].as_str().unwrap_or("unknown");
        let account = if scope["account"].get("Known").is_some() {
            "identified"
        } else {
            "unknown"
        };
        let freshness = snapshot["freshness"].as_str().unwrap_or("unknown");
        let age_seconds = snapshot["age_seconds"].as_u64().unwrap_or_default();
        output.push_str(&format!(
            "{provider} - integration {version}, platform {platform}, account scope {account}\n"
        ));
        output.push_str(&format!("  freshness: {freshness}, age: {age_seconds}s\n"));
        let models = snapshot["models"].as_array();
        if let Some(models) = models.filter(|models| !models.is_empty()) {
            for model in models {
                let model_id = model["model_id"].as_str().unwrap_or("unknown");
                let label = model["display_label"].as_str();
                if let Some(label) = label {
                    output.push_str(&format!("  {model_id} ({label})\n"));
                } else {
                    output.push_str(&format!("  {model_id}\n"));
                }
                output.push_str(&format!(
                    "    advertisement: {}, entitlement: {}, execution: {}\n",
                    state_label(model["advertisement"].as_str()),
                    state_label(model["entitlement"].as_str()),
                    state_label(model["execution"].as_str())
                ));
                let effort_values = model["efforts"]["values"]
                    .as_array()
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                let effort_state = state_label(model["efforts"]["state"].as_str());
                let default_effort = model["default_effort"].as_str();
                output.push_str(&format!("    effort evidence: {effort_state}"));
                if !effort_values.is_empty() {
                    output.push_str(&format!("; supported levels: {effort_values}"));
                }
                if let Some(default_effort) = default_effort {
                    output.push_str(&format!("; catalog default: {default_effort}"));
                }
                output.push('\n');
            }
        } else {
            output.push_str("  no model entries in this scope\n");
        }
    }
    output
}

fn render_refresh_results(results: &[Value]) -> (String, u8) {
    let mut output = String::new();
    for result in results {
        let status = result["status"].as_str().unwrap_or("unknown");
        let provider = result["scope"]["provider"].as_str().unwrap_or("unknown");
        match status {
            "updated" => {
                let snapshot = &result["snapshot"];
                let freshness = snapshot["freshness"].as_str().unwrap_or("unknown");
                let models = snapshot["models"].as_array().map_or(0, Vec::len);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let age = snapshot["refreshed_at"]
                    .as_u64()
                    .map_or(0, |refreshed_at| now.saturating_sub(refreshed_at));
                output.push_str(&format!(
                    "{provider}: updated {models} model records; freshness: {freshness}, age: {age}s\n"
                ));
            }
            "unsupported" => output.push_str(&format!(
                "{provider}: unsupported: {}\n",
                result["diagnostic"]
                    .as_str()
                    .unwrap_or("catalog discovery is unsupported")
            )),
            "failed" => output.push_str(&format!(
                "{provider}: refresh failed: {}\n",
                result["diagnostic"]
                    .as_str()
                    .unwrap_or("provider catalog request failed")
            )),
            _ => output.push_str(&format!("{provider}: unexpected refresh result\n")),
        }
    }
    (output, refresh_exit_code(results))
}

fn refresh_exit_code(results: &[Value]) -> u8 {
    if results
        .iter()
        .any(|result| result["status"].as_str() == Some("failed"))
    {
        4
    } else if results
        .iter()
        .any(|result| result["status"].as_str() == Some("unsupported"))
    {
        3
    } else {
        0
    }
}

fn state_label(value: Option<&str>) -> &str {
    value.unwrap_or("unknown")
}

fn print_help() {
    println!("Usage: aifuel model list|refresh [--provider PROVIDER_ID] [--json]");
    println!();
    println!("  list      show cached model records and per-scope freshness");
    println!("  refresh   query the provider catalog capability and persist its result");
    println!(
        "  --provider limits the operation to one provider; refresh defaults to all providers"
    );
    println!("  --json    emit the scoped catalog or refresh diagnostics as JSON");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_list_keeps_freshness_age_and_evidence_axes_separate() {
        let snapshots = vec![serde_json::json!({
            "scope": {
                "provider": "codex",
                "integration_version": "0.156",
                "platform": "linux",
                "account": "Unknown"
            },
            "freshness": "stale",
            "age_seconds": 420,
            "models": [{
                "model_id": "exact-model",
                "display_label": "Observed model",
                "advertisement": "supported",
                "entitlement": "unknown",
                "execution": "unknown",
                "default_effort": "medium",
                "efforts": {"state":"supported", "values":["low", "high"]}
            }]
        })];

        let output = render_catalog_list(&snapshots, Some(ProviderKey::Codex));

        assert!(output.contains("freshness: stale, age: 420s"));
        assert!(
            output.contains("advertisement: supported, entitlement: unknown, execution: unknown")
        );
        assert!(output.contains("catalog default: medium"));
        assert!(output.contains("supported levels: low, high"));
    }

    #[test]
    fn unsupported_refresh_keeps_provider_diagnostic_visible() {
        let results = vec![serde_json::json!({
            "status":"unsupported",
            "scope":{"provider":"gemini"},
            "diagnostic":"native catalog interface is unavailable"
        })];

        let (output, code) = render_refresh_results(&results);

        assert_eq!(code, 3);
        assert!(output.contains("gemini: unsupported"));
        assert!(output.contains("native catalog interface is unavailable"));
    }
}
