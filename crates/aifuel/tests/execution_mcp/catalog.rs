use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::support::TestDirectory;
use crate::support_protocol::{call_tools, write_execution_config};
use aifuel_app::selection::{
    AccountContext, CatalogEvidenceStore, CatalogModel, CatalogProvenance, CatalogScope,
    EffortEvidence,
};
use aifuel_core::{CapabilityState, ProviderKey};

#[test]
fn list_models_reports_unknown_freshness_and_stale_cache_age() {
    let missing = TestDirectory::new("execution-catalog-missing");
    write_execution_config(
        &missing,
        json!({"schema_version":1,"policy":{"allowed_roots":[]}}),
    );
    let unknown = call_tools(
        &missing,
        &[json!({"name":"list_models","arguments":{"provider":"codex"}})],
        None,
    );
    assert_eq!(structured(&unknown[0])["freshness"], "unknown");
    assert_eq!(
        structured(&unknown[0])["diagnostics"][0]["code"],
        "catalog_unknown"
    );

    let fresh = TestDirectory::new("execution-catalog-fresh");
    write_execution_config(
        &fresh,
        json!({"schema_version":1,"policy":{"allowed_roots":[]}}),
    );
    seed_catalog(&fresh, ProviderKey::Codex, SystemTime::now());
    let fresh_response = call_tools(
        &fresh,
        &[json!({"name":"list_models","arguments":{"provider":"codex"}})],
        None,
    );
    assert_eq!(structured(&fresh_response[0])["freshness"], "fresh");
    assert_eq!(
        structured(&fresh_response[0])["models"][0]["model_id"],
        "cached-model"
    );
    assert_eq!(
        structured(&fresh_response[0])["catalogs"][0]["age_seconds"].as_u64(),
        Some(0)
    );

    let stale = TestDirectory::new("execution-catalog-stale");
    write_execution_config(
        &stale,
        json!({"schema_version":1,"policy":{"allowed_roots":[]}}),
    );
    seed_catalog(
        &stale,
        ProviderKey::Codex,
        SystemTime::now() - Duration::from_secs(600),
    );
    let stale_response = call_tools(
        &stale,
        &[json!({"name":"list_models","arguments":{"provider":"codex"}})],
        None,
    );
    assert_eq!(structured(&stale_response[0])["freshness"], "stale");
    assert!(
        structured(&stale_response[0])["age_seconds"]
            .as_u64()
            .is_some_and(|age| age >= 600)
    );
}

#[test]
fn list_models_refresh_reports_updated_failed_and_unsupported_catalogs() {
    let updated = TestDirectory::new("execution-catalog-updated");
    write_execution_config(
        &updated,
        json!({"schema_version":1,"policy":{"allowed_roots":[]}}),
    );
    install_fake_codex_catalog(
        &updated,
        r#"{"models":[{"slug":"fresh-model","supported_reasoning_levels":[{"effort":"high"}]}]}"#,
        true,
    );
    let updates = call_tools(
        &updated,
        &[
            json!({"name":"list_models","arguments":{"provider":"codex","refresh":true}}),
            json!({"name":"list_models","arguments":{"provider":"codex"}}),
        ],
        Some(updated.path()),
    );
    assert_eq!(structured(&updates[0])["refresh_status"], "updated");
    assert_eq!(structured(&updates[0])["freshness"], "fresh");
    assert_eq!(
        structured(&updates[0])["models"][0]["model_id"],
        "fresh-model"
    );
    assert_eq!(structured(&updates[1])["freshness"], "fresh");
    assert_eq!(
        structured(&updates[1])["models"][0]["model_id"],
        "fresh-model"
    );

    let failed = TestDirectory::new("execution-catalog-failed");
    write_execution_config(
        &failed,
        json!({"schema_version":1,"policy":{"allowed_roots":[]}}),
    );
    seed_catalog(
        &failed,
        ProviderKey::Codex,
        SystemTime::now() - Duration::from_secs(600),
    );
    install_fake_codex_catalog(&failed, "bad-catalog", false);
    let failures = call_tools(
        &failed,
        &[
            json!({"name":"list_models","arguments":{"provider":"codex","refresh":true}}),
            json!({"name":"list_models","arguments":{"provider":"codex"}}),
        ],
        Some(failed.path()),
    );
    assert_eq!(structured(&failures[0])["refresh_status"], "failed");
    assert_eq!(structured(&failures[0])["freshness"], "stale");
    assert_eq!(
        structured(&failures[0])["models"][0]["model_id"],
        "cached-model"
    );
    assert!(
        structured(&failures[0])["diagnostics"][0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("invalid JSON"))
    );
    assert_eq!(structured(&failures[1])["freshness"], "stale");
    assert_eq!(
        structured(&failures[1])["models"][0]["model_id"],
        "cached-model"
    );

    let unsupported = TestDirectory::new("execution-catalog-unsupported");
    write_execution_config(
        &unsupported,
        json!({"schema_version":1,"policy":{"allowed_roots":[]}}),
    );
    let unsupported_response = call_tools(
        &unsupported,
        &[json!({"name":"list_models","arguments":{"provider":"claude","refresh":true}})],
        None,
    );
    assert_eq!(
        structured(&unsupported_response[0])["refresh_status"],
        "unsupported"
    );
    assert!(
        structured(&unsupported_response[0])["diagnostics"][0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("not implemented"))
    );
}

fn structured(response: &Value) -> &Value {
    &response["result"]["structuredContent"]
}

fn seed_catalog(directory: &TestDirectory, provider: ProviderKey, refreshed_at: SystemTime) {
    let path = catalog_path(directory);
    let timestamp = refreshed_at
        .duration_since(UNIX_EPOCH)
        .expect("test timestamp should be after epoch")
        .as_secs();
    let scope = CatalogScope::new(
        provider,
        Some("fixture-1".to_owned()),
        Some(catalog_platform()),
        AccountContext::Unknown,
    );
    let mut model = CatalogModel::new(
        provider,
        "cached-model",
        CatalogProvenance::BundledCatalog,
        timestamp,
    );
    model.advertisement = CapabilityState::Supported;
    model.efforts = EffortEvidence::known(["low".to_owned(), "high".to_owned()]);
    let mut store = CatalogEvidenceStore::new();
    store.refresh_at(scope, refreshed_at, || Ok::<_, String>(vec![model]));
    store.save(path).expect("catalog cache should persist");
}

fn catalog_platform() -> String {
    let wsl =
        std::env::var_os("WSL_INTEROP").is_some() || std::env::var_os("WSL_DISTRO_NAME").is_some();
    let proc_version = fs::read_to_string("/proc/version").unwrap_or_default();
    if std::env::consts::OS == "linux"
        && (wsl || proc_version.to_ascii_lowercase().contains("microsoft"))
    {
        "wsl".to_owned()
    } else {
        std::env::consts::OS.to_owned()
    }
}

fn install_fake_codex_catalog(directory: &TestDirectory, catalog: &str, success: bool) {
    let bin = directory.path();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = if success {
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'codex-cli fixture-1'; exit 0; fi\nif [ \"$1\" = \"debug\" ] && [ \"$2\" = \"models\" ] && [ \"$3\" = \"--bundled\" ]; then printf '%s' '{}'; exit 0; fi\necho unexpected invocation >&2; exit 64\n",
                catalog
            )
        } else {
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'codex-cli fixture-1'; exit 0; fi\nif [ \"$1\" = \"debug\" ]; then echo '{not json}'; exit 0; fi\nexit 64\n".to_owned()
        };
        fs::write(bin.join("codex"), script).expect("fake Codex command should be written");
        fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o755))
            .expect("fake Codex command should be executable");
    }
    #[cfg(windows)]
    {
        let script = if success {
            format!(
                "@echo off\r\nif \"%~1\"==\"--version\" goto version\r\nif \"%~1\"==\"debug\" goto debug\r\necho unexpected invocation 1>&2\r\nexit /b 64\r\n:version\r\necho codex-cli fixture-1\r\nexit /b 0\r\n:debug\r\nif \"%~2\"==\"models\" if \"%~3\"==\"--bundled\" goto models\r\necho unexpected invocation 1>&2\r\nexit /b 64\r\n:models\r\necho {catalog}\r\nexit /b 0\r\n"
            )
        } else {
            "@echo off\r\nif \"%~1\"==\"--version\" goto version\r\nif \"%~1\"==\"debug\" goto invalid\r\nexit /b 64\r\n:version\r\necho codex-cli fixture-1\r\nexit /b 0\r\n:invalid\r\necho {not json}\r\nexit /b 0\r\n".to_owned()
        };
        fs::write(bin.join("codex.cmd"), script).expect("fake Codex command should be written");
    }
}

fn catalog_path(directory: &TestDirectory) -> PathBuf {
    crate::support::ai_fuel_config_dir(directory.path()).join("model-catalog.json")
}
