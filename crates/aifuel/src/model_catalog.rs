use aifuel_app::selection::{
    AccountContext, CatalogEvidenceStore, CatalogModel, CatalogProvenance, CatalogRefreshResult,
    CatalogScope, EffortEvidence,
};
use aifuel_core::{CapabilityState, ProviderKey};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Load the last successful model catalog snapshot for the execution MCP
/// endpoint. A missing cache is an explicit unknown catalog, not an empty
/// successful discovery.
pub fn model_catalog_snapshot() -> Result<Vec<serde_json::Value>, String> {
    let store =
        CatalogEvidenceStore::load(model_catalog_path()?).map_err(|error| error.to_string())?;
    let scopes = store.scopes().cloned().collect::<Vec<_>>();
    let mut snapshots = Vec::new();
    for scope in scopes {
        let (snapshot, age_seconds) = match store.lookup(&scope) {
            aifuel_app::selection::CatalogLookup::Fresh(snapshot) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let age = now.saturating_sub(snapshot.refreshed_at);
                (snapshot, age)
            }
            aifuel_app::selection::CatalogLookup::Stale { snapshot, age } => {
                (snapshot, age.as_secs())
            }
            aifuel_app::selection::CatalogLookup::Unknown { .. } => continue,
        };
        let mut value = serde_json::to_value(snapshot)
            .map_err(|error| format!("could not encode model catalog snapshot: {error}"))?;
        value["age_seconds"] = serde_json::Value::from(age_seconds);
        snapshots.push(value);
    }
    Ok(snapshots)
}

/// Refresh one provider's model catalog through its compiled native catalog
/// capability and persist the scoped evidence. Providers without a verified
/// discovery interface return an explicit `unsupported` result and no models.
pub async fn refresh_model_catalog(provider: ProviderKey) -> Result<serde_json::Value, String> {
    let discovery = aifuel_providers::discover_model_catalog(provider).await;
    persist_model_catalog_discovery(
        &model_catalog_path()?,
        provider,
        &catalog_platform(),
        discovery,
    )
}

fn catalog_platform() -> String {
    let wsl = env::var_os("WSL_INTEROP").is_some() || env::var_os("WSL_DISTRO_NAME").is_some();
    let proc_version = fs::read_to_string("/proc/version").unwrap_or_default();
    catalog_platform_from_evidence(std::env::consts::OS, wsl, &proc_version)
}

fn catalog_platform_from_evidence(os: &str, wsl_environment: bool, proc_version: &str) -> String {
    if os == "linux" && (wsl_environment || proc_version.to_ascii_lowercase().contains("microsoft"))
    {
        "wsl".to_owned()
    } else {
        os.to_owned()
    }
}

fn persist_model_catalog_discovery(
    catalog_path: &Path,
    provider: ProviderKey,
    platform: &str,
    discovery: aifuel_providers::ProviderCatalogDiscovery,
) -> Result<serde_json::Value, String> {
    match discovery {
        aifuel_providers::ProviderCatalogDiscovery::Unsupported { diagnostic } => {
            let scope = CatalogScope::new(
                provider,
                None,
                Some(platform.to_owned()),
                AccountContext::Unknown,
            );
            Ok(serde_json::json!({
                "status": "unsupported",
                "scope": scope,
                "diagnostic": diagnostic,
            }))
        }
        aifuel_providers::ProviderCatalogDiscovery::Available {
            integration_version,
            models,
        } => {
            let scope = CatalogScope::new(
                provider,
                integration_version,
                Some(platform.to_owned()),
                AccountContext::Unknown,
            );
            let now = std::time::SystemTime::now();
            let discovered_at = now
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let models = models
                .into_iter()
                .map(|entry| {
                    let mut model = CatalogModel::new(
                        provider,
                        entry.model_id,
                        CatalogProvenance::BundledCatalog,
                        discovered_at,
                    );
                    model.advertisement = CapabilityState::Supported;
                    model.display_label = entry.display_label;
                    model.default_effort = entry.default_effort;
                    model.efforts = entry
                        .supported_efforts
                        .map(EffortEvidence::known)
                        .unwrap_or_else(EffortEvidence::unknown);
                    model
                })
                .collect();

            let mut store =
                CatalogEvidenceStore::load(catalog_path).map_err(|error| error.to_string())?;
            let refresh = store.refresh_at(scope, now, || Ok::<_, String>(models));
            store
                .save(catalog_path)
                .map_err(|error| error.to_string())?;
            match refresh {
                CatalogRefreshResult::Updated(snapshot) => Ok(serde_json::json!({
                    "status": "updated",
                    "snapshot": snapshot,
                })),
                CatalogRefreshResult::Failed { error, .. } => Ok(serde_json::json!({
                    "status": "failed",
                    "diagnostic": error,
                })),
            }
        }
        aifuel_providers::ProviderCatalogDiscovery::Failed {
            integration_version,
            diagnostic,
        } => {
            let scope = CatalogScope::new(
                provider,
                integration_version,
                Some(platform.to_owned()),
                AccountContext::Unknown,
            );
            let mut store =
                CatalogEvidenceStore::load(catalog_path).map_err(|error| error.to_string())?;
            let refresh = store.refresh(scope, || Err::<Vec<CatalogModel>, _>(diagnostic));
            store
                .save(catalog_path)
                .map_err(|error| error.to_string())?;
            match refresh {
                CatalogRefreshResult::Failed {
                    scope,
                    retained,
                    error,
                } => Ok(serde_json::json!({
                    "status": "failed",
                    "scope": scope,
                    "retained": retained,
                    "diagnostic": error,
                })),
                CatalogRefreshResult::Updated(_) => unreachable!("refresh closure always fails"),
            }
        }
    }
}

fn model_catalog_path() -> Result<PathBuf, String> {
    let home = crate::user_home_dir()?;
    Ok(crate::user_config_dir(&home)?
        .join("aifuel")
        .join("model-catalog.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aifuel_app::selection::{CatalogFreshness, CatalogLookup};

    #[test]
    fn catalog_platform_separates_wsl_from_native_linux() {
        assert_eq!(
            catalog_platform_from_evidence("linux", false, "Linux version"),
            "linux"
        );
        assert_eq!(
            catalog_platform_from_evidence("linux", true, "Linux version"),
            "wsl"
        );
        assert_eq!(
            catalog_platform_from_evidence("linux", false, "Linux version Microsoft WSL2"),
            "wsl"
        );
        assert_eq!(
            catalog_platform_from_evidence("windows", true, "Microsoft"),
            "windows"
        );
    }

    #[test]
    fn empty_success_is_saved_as_a_fresh_empty_catalog() {
        let directory = temporary_directory();
        let path = directory.join("model-catalog.json");
        let result = persist_model_catalog_discovery(
            &path,
            ProviderKey::Codex,
            "linux",
            aifuel_providers::ProviderCatalogDiscovery::Available {
                integration_version: Some("0.156.0".to_owned()),
                models: Vec::new(),
            },
        )
        .expect("successful empty catalog should persist");

        assert_eq!(result["status"], "updated");
        assert_eq!(result["snapshot"]["models"], serde_json::json!([]));
        assert_eq!(result["snapshot"]["scope"]["account"], "Unknown");
        assert_eq!(result["snapshot"]["scope"]["platform"], "linux");

        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn failed_refresh_persists_diagnostics_and_retains_prior_catalog_as_stale() {
        let directory = temporary_directory();
        let path = directory.join("model-catalog.json");
        let initial = aifuel_providers::ProviderCatalogDiscovery::Available {
            integration_version: Some("0.156.0".to_owned()),
            models: vec![aifuel_providers::ProviderCatalogModel {
                model_id: "gpt-6-astra".to_owned(),
                display_label: Some("GPT-6-Astra".to_owned()),
                default_effort: Some("medium".to_owned()),
                supported_efforts: Some(vec!["low".to_owned(), "medium".to_owned()]),
            }],
        };
        persist_model_catalog_discovery(&path, ProviderKey::Codex, "linux", initial)
            .expect("initial catalog should persist");

        let result = persist_model_catalog_discovery(
            &path,
            ProviderKey::Codex,
            "linux",
            aifuel_providers::ProviderCatalogDiscovery::Failed {
                integration_version: Some("0.156.0".to_owned()),
                diagnostic: "fixture refresh failure".to_owned(),
            },
        )
        .expect("failed refresh should retain prior evidence");

        assert_eq!(result["status"], "failed");
        assert_eq!(result["diagnostic"], "fixture refresh failure");
        assert_eq!(result["retained"]["freshness"], "stale");
        assert_eq!(result["retained"]["models"][0]["model_id"], "gpt-6-astra");
        assert_eq!(result["retained"]["models"][0]["default_effort"], "medium");

        let store = CatalogEvidenceStore::load(&path).expect("retained cache should reload");
        let scope = store.scopes().next().expect("scope should persist").clone();
        assert!(matches!(
            store.lookup_at(
                &scope,
                std::time::SystemTime::now() + std::time::Duration::from_secs(1)
            ),
            CatalogLookup::Stale { snapshot, .. }
                if snapshot.freshness == CatalogFreshness::Stale
        ));

        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn unsupported_provider_has_diagnostic_without_a_fabricated_catalog() {
        let directory = temporary_directory();
        let path = directory.join("model-catalog.json");
        let result = persist_model_catalog_discovery(
            &path,
            ProviderKey::Claude,
            "linux",
            aifuel_providers::ProviderCatalogDiscovery::Unsupported {
                diagnostic: "model listing is not implemented for claude".to_owned(),
            },
        )
        .expect("unsupported discovery should remain explicit");

        assert_eq!(result["status"], "unsupported");
        assert_eq!(
            result["diagnostic"],
            "model listing is not implemented for claude"
        );
        assert!(result.get("models").is_none());
        assert!(!path.exists());

        let _ = fs::remove_dir_all(directory);
    }

    fn temporary_directory() -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after the epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "aifuel-catalog-test-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temporary directory should be created");
        path
    }
}
