use super::super::model_catalog::ProviderCatalogModel;
use crate::agent_execution::{
    MAX_CAPTURE_BYTES, kill_and_wait, owned_command, program_candidates, read_bounded,
};
use serde::Deserialize;
use std::io;
use std::process::Stdio;
use std::time::{Duration, Instant};

// `devin models list` answers an account-scoped request, unlike Codex's
// bundled read; the observed cold call ran past 10 seconds.
const CATALOG_SETUP_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct DevinCatalog {
    pub integration_version: Option<String>,
    pub models: Vec<ProviderCatalogModel>,
}

pub(crate) struct DevinCatalogError {
    pub integration_version: Option<String>,
    pub diagnostic: String,
}

pub(crate) async fn discover() -> Result<DevinCatalog, DevinCatalogError> {
    let integration_version = run_devin(&["version"])
        .await
        .ok()
        .and_then(|output| parse_version(&output));
    let output = run_devin(&["models", "list", "--format", "json"])
        .await
        .map_err(|diagnostic| DevinCatalogError {
            integration_version: integration_version.clone(),
            diagnostic,
        })?;
    let models =
        parse_devin_catalog(output.as_bytes()).map_err(|diagnostic| DevinCatalogError {
            integration_version: integration_version.clone(),
            diagnostic,
        })?;
    Ok(DevinCatalog {
        integration_version,
        models,
    })
}

fn parse_version(output: &str) -> Option<String> {
    let line = output.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    Some(
        line.strip_prefix("devin ")
            .unwrap_or(line)
            .split_whitespace()
            .next()
            .unwrap_or(line)
            .to_owned(),
    )
}

#[derive(Deserialize)]
struct ModelsList {
    families: Vec<ModelFamily>,
}

#[derive(Deserialize)]
struct ModelFamily {
    slug: String,
    #[serde(default)]
    family_label: Option<String>,
    #[serde(default)]
    variants: Vec<ModelVariant>,
}

#[derive(Deserialize)]
struct ModelVariant {
    model_uid: String,
    #[serde(default)]
    label: Option<String>,
}

/// Map `devin models list` families onto catalog entries. Devin has no
/// separate effort flag: effort is expressed by picking a variant
/// `model_uid` (`swe-2` + `swe-2-max`), and uids do not share a reliable
/// slug prefix (`MODEL_GPT_5_2_LOW`, `claude-5-fable-max`). Each family
/// therefore advertises its variant uids as the effort list - the values
/// are exactly what `--model` accepts - and every variant also lists as
/// its own fixed-effort model.
fn parse_devin_catalog(bytes: &[u8]) -> Result<Vec<ProviderCatalogModel>, String> {
    let list: ModelsList = serde_json::from_slice(bytes)
        .map_err(|error| format!("Devin model list was invalid JSON: {error}"))?;

    let mut models = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for family in list.families {
        let slug = family.slug.trim();
        if slug.is_empty() {
            return Err("Devin model list contained an empty family slug".to_owned());
        }
        let mut uids = Vec::new();
        for variant in family.variants {
            let uid = variant.model_uid.trim();
            if uid.is_empty() {
                continue;
            }
            uids.push(uid.to_owned());
            if seen.insert(uid.to_owned()) {
                models.push(ProviderCatalogModel {
                    model_id: uid.to_owned(),
                    display_label: variant
                        .label
                        .filter(|value| !value.trim().is_empty()),
                    default_effort: None,
                    supported_efforts: Some(Vec::new()),
                });
            }
        }
        if seen.insert(slug.to_owned()) {
            models.push(ProviderCatalogModel {
                model_id: slug.to_owned(),
                display_label: family
                    .family_label
                    .filter(|value| !value.trim().is_empty()),
                default_effort: None,
                supported_efforts: Some(uids),
            });
        }
    }
    Ok(models)
}

async fn run_devin(args: &[&str]) -> Result<String, String> {
    let mut child = None;
    for candidate in program_candidates("devin") {
        let mut command = owned_command(&candidate, |command| {
            command
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        });
        match command.spawn() {
            Ok(process) => {
                child = Some((candidate, process));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err("Devin catalog command could not be started".to_owned());
            }
        }
    }
    let (program, mut child) = child.ok_or_else(|| "Devin CLI was not found".to_owned())?;
    let stdout = child
        .stdout()
        .take()
        .ok_or_else(|| "Devin catalog command did not provide stdout".to_owned())?;
    let stderr = child
        .stderr()
        .take()
        .ok_or_else(|| "Devin catalog command did not provide stderr".to_owned())?;
    let stdout_reader = tokio::spawn(read_bounded(stdout, MAX_CAPTURE_BYTES));
    let stderr_reader = tokio::spawn(read_bounded(stderr, MAX_CAPTURE_BYTES));
    let deadline = Instant::now() + CATALOG_SETUP_TIMEOUT;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(_) => {
                let _ = kill_and_wait(&mut child).await;
                let _ = stdout_reader.await;
                let _ = stderr_reader.await;
                return Err("Devin catalog command status could not be read".to_owned());
            }
        }
        if Instant::now() >= deadline {
            let _ = kill_and_wait(&mut child).await;
            let _ = stdout_reader.await;
            let _ = stderr_reader.await;
            return Err("Devin catalog command exceeded the 30-second setup limit".to_owned());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    let stdout = match stdout_reader.await {
        Ok(Ok(output)) => output.text,
        _ => return Err("Devin catalog output could not be read".to_owned()),
    };
    let stderr = match stderr_reader.await {
        Ok(Ok(output)) => output.text,
        _ => return Err("Devin catalog diagnostics could not be drained".to_owned()),
    };
    if stdout.len() >= MAX_CAPTURE_BYTES || stderr.len() >= MAX_CAPTURE_BYTES {
        return Err("Devin catalog command output exceeded the capture limit".to_owned());
    }
    if !status.success() {
        return Err(format!(
            "Devin catalog command {program:?} {} failed with exit status {}",
            args.join(" "),
            status
                .code()
                .map_or_else(|| "signal".to_owned(), |code| code.to_string())
        ));
    }
    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_efforts_are_the_exact_variant_uids() {
        let input = br#"{
            "families": [{
                "family_label": "SWE-2",
                "family_uid": "swe-2",
                "slug": "swe-2",
                "aliases": ["swe"],
                "variants": [
                    {"model_uid": "swe-2-high", "label": "SWE-2 High"},
                    {"model_uid": "swe-2-max", "label": "SWE-2 Max"}
                ]
            }]
        }"#;

        let models = parse_devin_catalog(input).expect("catalog fixture should parse");

        assert!(models.contains(&ProviderCatalogModel {
            model_id: "swe-2".to_owned(),
            display_label: Some("SWE-2".to_owned()),
            default_effort: None,
            supported_efforts: Some(vec!["swe-2-high".to_owned(), "swe-2-max".to_owned()]),
        }));
        assert!(models.contains(&ProviderCatalogModel {
            model_id: "swe-2-max".to_owned(),
            display_label: Some("SWE-2 Max".to_owned()),
            default_effort: None,
            supported_efforts: Some(Vec::new()),
        }));
        assert_eq!(models.len(), 3);
    }

    #[test]
    fn variants_without_a_slug_prefix_still_list_verbatim() {
        // Some families report internal enum names as uids; they stay
        // callable because `--model` accepts them verbatim, so no
        // reconstruction is attempted.
        let input = br#"{
            "families": [{
                "family_label": "GPT-5.2",
                "family_uid": "gpt-5.2",
                "slug": "gpt-5.2",
                "variants": [
                    {"model_uid": "MODEL_GPT_5_2_LOW", "label": "GPT-5.2 Low Thinking"}
                ]
            }]
        }"#;

        let models = parse_devin_catalog(input).expect("catalog fixture should parse");

        assert_eq!(models[0].model_id, "MODEL_GPT_5_2_LOW");
        assert_eq!(
            models[1].supported_efforts,
            Some(vec!["MODEL_GPT_5_2_LOW".to_owned()])
        );
    }

    #[test]
    fn fixed_single_variant_family_reports_no_effort_choices() {
        let input = br#"{
            "families": [{
                "family_label": "Adaptive",
                "family_uid": "Adaptive",
                "slug": "adaptive",
                "variants": [{"model_uid": "adaptive", "label": "Adaptive"}]
            }]
        }"#;

        let models = parse_devin_catalog(input).expect("catalog fixture should parse");

        assert_eq!(
            models.iter().map(|m| m.model_id.as_str()).collect::<Vec<_>>(),
            vec!["adaptive"]
        );
    }

    #[test]
    fn malformed_or_incomplete_list_is_an_error() {
        assert!(parse_devin_catalog(b"not json").is_err());
        assert!(
            parse_devin_catalog(br#"{"families":[{"variants":[]}]}"#).is_err()
        );
    }

    #[test]
    fn empty_family_list_is_a_successful_empty_catalog() {
        assert_eq!(parse_devin_catalog(br#"{"families":[]}"#), Ok(Vec::new()));
    }

    #[test]
    fn version_parser_keeps_only_the_reported_version() {
        assert_eq!(
            parse_version("devin 3000.11.3 (9c803229faa4)\n"),
            Some("3000.11.3".to_owned())
        );
        assert_eq!(parse_version("\n"), None);
    }
}
