use super::super::model_catalog::ProviderCatalogModel;
use crate::agent_execution::{
    MAX_CAPTURE_BYTES, kill_and_wait, owned_command, program_candidates, read_bounded,
};
use serde::Deserialize;
use std::io;
use std::process::Stdio;
use std::time::{Duration, Instant};

const CATALOG_SETUP_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct CodexCatalog {
    pub integration_version: Option<String>,
    pub models: Vec<ProviderCatalogModel>,
}

pub(crate) struct CodexCatalogError {
    pub integration_version: Option<String>,
    pub diagnostic: String,
}

pub(crate) async fn discover() -> Result<CodexCatalog, CodexCatalogError> {
    // The bundled form reads only the catalog shipped with this executable.
    // Do not replace it with `model/list` or the default debug command: those
    // can refresh account-scoped data and their credential side effects are
    // not established.
    let integration_version = run_codex(&["--version"])
        .await
        .ok()
        .and_then(|output| parse_version(&output));
    let output = run_codex(&["debug", "models", "--bundled"])
        .await
        .map_err(|diagnostic| CodexCatalogError {
            integration_version: integration_version.clone(),
            diagnostic,
        })?;
    let models =
        parse_codex_catalog(output.as_bytes()).map_err(|diagnostic| CodexCatalogError {
            integration_version: integration_version.clone(),
            diagnostic,
        })?;
    Ok(CodexCatalog {
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
        line.strip_prefix("codex-cli ")
            .unwrap_or(line)
            .trim()
            .to_owned(),
    )
}

#[derive(Deserialize)]
struct BundledCatalog {
    models: Vec<BundledModel>,
}

#[derive(Deserialize)]
struct BundledModel {
    slug: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    default_reasoning_level: Option<String>,
    #[serde(default)]
    supported_reasoning_levels: Option<Vec<ReasoningLevel>>,
}

#[derive(Deserialize)]
struct ReasoningLevel {
    effort: String,
}

fn parse_codex_catalog(bytes: &[u8]) -> Result<Vec<ProviderCatalogModel>, String> {
    let catalog: BundledCatalog = serde_json::from_slice(bytes)
        .map_err(|error| format!("Codex bundled model catalog was invalid JSON: {error}"))?;

    catalog
        .models
        .into_iter()
        .map(|model| {
            if model.slug.trim().is_empty() {
                return Err("Codex bundled model catalog contained an empty model slug".to_owned());
            }
            Ok(ProviderCatalogModel {
                model_id: model.slug,
                display_label: model.display_name.filter(|value| !value.trim().is_empty()),
                default_effort: model
                    .default_reasoning_level
                    .filter(|value| !value.trim().is_empty()),
                supported_efforts: model
                    .supported_reasoning_levels
                    .map(|levels| levels.into_iter().map(|level| level.effort).collect()),
            })
        })
        .collect()
}

async fn run_codex(args: &[&str]) -> Result<String, String> {
    let mut child = None;
    for candidate in program_candidates("codex") {
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
                return Err("Codex catalog command could not be started".to_owned());
            }
        }
    }
    let (program, mut child) = child.ok_or_else(|| "Codex CLI was not found".to_owned())?;
    let stdout = child
        .stdout()
        .take()
        .ok_or_else(|| "Codex catalog command did not provide stdout".to_owned())?;
    let stderr = child
        .stderr()
        .take()
        .ok_or_else(|| "Codex catalog command did not provide stderr".to_owned())?;
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
                return Err("Codex catalog command status could not be read".to_owned());
            }
        }
        if Instant::now() >= deadline {
            let _ = kill_and_wait(&mut child).await;
            let _ = stdout_reader.await;
            let _ = stderr_reader.await;
            return Err("Codex catalog command exceeded the 10-second setup limit".to_owned());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    let stdout = match stdout_reader.await {
        Ok(Ok(output)) => output.text,
        _ => return Err("Codex catalog output could not be read".to_owned()),
    };
    let stderr = match stderr_reader.await {
        Ok(Ok(output)) => output.text,
        _ => return Err("Codex catalog diagnostics could not be drained".to_owned()),
    };
    if stdout.len() >= MAX_CAPTURE_BYTES || stderr.len() >= MAX_CAPTURE_BYTES {
        return Err("Codex catalog command output exceeded the capture limit".to_owned());
    }
    if !status.success() {
        return Err(format!(
            "Codex catalog command {program:?} {} failed with exit status {}",
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
    fn parses_only_catalog_fields_and_discards_unrelated_messages() {
        let input = br#"{
            "models": [{
                "slug": "gpt-6-astra",
                "display_name": "GPT-6-Astra",
                "default_reasoning_level": "medium",
                "supported_reasoning_levels": [
                    {"effort": "low", "description": "fast"},
                    {"effort": "high", "description": "deep"}
                ],
                "model_messages": {"persistent_instructions": "must not be retained"},
                "unknown_provider_field": {"nested": [1, 2, 3]}
            }]
        }"#;

        let models = parse_codex_catalog(input).expect("catalog fixture should parse");

        assert_eq!(
            models,
            vec![ProviderCatalogModel {
                model_id: "gpt-6-astra".to_owned(),
                display_label: Some("GPT-6-Astra".to_owned()),
                default_effort: Some("medium".to_owned()),
                supported_efforts: Some(vec!["low".to_owned(), "high".to_owned()]),
            }]
        );
        assert!(!format!("{models:?}").contains("persistent_instructions"));
        assert!(!format!("{models:?}").contains("unknown_provider_field"));
    }

    #[test]
    fn empty_catalog_is_a_successful_empty_list() {
        assert_eq!(parse_codex_catalog(br#"{"models":[]}"#), Ok(Vec::new()));
    }

    #[test]
    fn malformed_or_incomplete_catalog_is_an_error() {
        assert!(parse_codex_catalog(b"not json").is_err());
        assert!(parse_codex_catalog(br#"{"models":[{"display_name":"missing slug"}]}"#).is_err());
        assert!(parse_codex_catalog(br#"{"models":[{"slug":"   "}]}"#).is_err());
    }

    #[test]
    fn missing_effort_fields_remain_unknown() {
        let models = parse_codex_catalog(br#"{"models":[{"slug":"custom-model"}]}"#)
            .expect("catalog fixture should parse");
        assert_eq!(models[0].supported_efforts, None);
        assert_eq!(models[0].default_effort, None);
    }

    #[test]
    fn version_parser_keeps_only_the_reported_version() {
        assert_eq!(
            parse_version("codex-cli 0.156.0\n"),
            Some("0.156.0".to_owned())
        );
        assert_eq!(parse_version("\n"), None);
    }
}
