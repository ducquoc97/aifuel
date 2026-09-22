use aifuel_app::selection::{
    CatalogEvidenceStore, CatalogFreshness, CatalogLookup, CatalogModel, CatalogProvenance,
    CatalogRefreshResult, CatalogScope, GlobalSelectionConfig, SelectionError, SelectionInputs,
    SelectionSettings, SelectionSource, SelectionStore, StoredSession,
};
use aifuel_core::{AccessMode, ProviderKey};
use std::time::{Duration, UNIX_EPOCH};

#[test]
fn explicit_values_override_profile_and_global_defaults_with_sources() {
    let mut config = GlobalSelectionConfig {
        defaults: SelectionSettings {
            provider: Some(ProviderKey::Codex),
            model: Some("global-model".to_owned()),
            effort: Some("low".to_owned()),
            access: Some(AccessMode::ReadOnly),
            overall_deadline_seconds: Some(60),
        },
        ..GlobalSelectionConfig::default()
    };
    config.profiles.insert(
        "review".to_owned(),
        SelectionSettings {
            provider: Some(ProviderKey::Claude),
            model: Some("profile-model".to_owned()),
            effort: None,
            access: Some(AccessMode::WorkspaceWrite),
            overall_deadline_seconds: None,
        },
    );

    let mut inputs = SelectionInputs::named_profile("review");
    inputs.explicit.model = Some("explicit-model".to_owned());
    let resolved = config
        .resolve(&inputs, None)
        .expect("selection should resolve");

    assert_eq!(resolved.provider, ProviderKey::Claude);
    assert_eq!(resolved.model.as_deref(), Some("explicit-model"));
    assert_eq!(resolved.effort.as_deref(), Some("low"));
    assert_eq!(resolved.access, AccessMode::WorkspaceWrite);
    assert_eq!(resolved.overall_deadline, Some(Duration::from_secs(60)));
    assert_eq!(
        resolved.sources.provider,
        SelectionSource::Profile("review".to_owned())
    );
    assert_eq!(resolved.sources.model, SelectionSource::Explicit);
    assert_eq!(resolved.sources.effort, SelectionSource::GlobalDefault);
}

#[test]
fn resume_ignores_global_defaults_and_does_not_inherit_access_or_deadline() {
    let config = GlobalSelectionConfig {
        defaults: SelectionSettings {
            provider: Some(ProviderKey::Codex),
            model: Some("changed-global".to_owned()),
            effort: Some("changed-effort".to_owned()),
            access: Some(AccessMode::WorkspaceWrite),
            overall_deadline_seconds: Some(900),
        },
        ..GlobalSelectionConfig::default()
    };
    let session = StoredSession {
        session_id: "session-1".to_owned(),
        provider: ProviderKey::Claude,
        model: Some("stored-model".to_owned()),
        effort: Some("stored-effort".to_owned()),
        access: Some(AccessMode::WorkspaceWrite),
        overall_deadline_seconds: Some(120),
        account_id: None,
    };

    let resolved = config
        .resolve(&SelectionInputs::default(), Some(&session))
        .expect("same-provider resume should resolve");

    assert_eq!(resolved.provider, ProviderKey::Claude);
    assert_eq!(resolved.model.as_deref(), Some("stored-model"));
    assert_eq!(resolved.effort.as_deref(), Some("stored-effort"));
    assert_eq!(resolved.access, AccessMode::ReadOnly);
    assert_eq!(resolved.overall_deadline, None);
    assert_eq!(resolved.sources.model, SelectionSource::StoredSession);
    assert_eq!(resolved.sources.access, SelectionSource::NativeDefault);
}

#[test]
fn resume_rejects_profile_provider_conflict_even_when_explicit_provider_matches() {
    let mut config = GlobalSelectionConfig::default();
    config.profiles.insert(
        "wrong".to_owned(),
        SelectionSettings {
            provider: Some(ProviderKey::Codex),
            ..SelectionSettings::default()
        },
    );
    let session = StoredSession::new("session-1", ProviderKey::Claude);
    let mut inputs = SelectionInputs::named_profile("wrong");
    inputs.explicit.provider = Some(ProviderKey::Claude);

    assert!(matches!(
        config.resolve(&inputs, Some(&session)),
        Err(SelectionError::ProviderConflict {
            session_provider: ProviderKey::Claude,
            requested_provider: ProviderKey::Codex,
        })
    ));
}

#[test]
fn failed_catalog_refresh_retains_stale_evidence_and_account_scope_isolated() {
    let scope = CatalogScope::reported(
        ProviderKey::Codex,
        "1.0.0",
        "linux",
        Some("account-a".to_owned()),
    );
    let other_account = CatalogScope::reported(
        ProviderKey::Codex,
        "1.0.0",
        "linux",
        Some("account-b".to_owned()),
    );
    let mut store = CatalogEvidenceStore::new();
    let model = CatalogModel::new(
        ProviderKey::Codex,
        "provider-exact-id",
        CatalogProvenance::NativeInterface,
        100,
    );
    assert!(matches!(
        store.refresh_at(scope.clone(), UNIX_EPOCH + Duration::from_secs(100), || {
            Ok::<_, &str>(vec![model.clone()])
        }),
        CatalogRefreshResult::Updated(_)
    ));
    assert!(matches!(
        store.lookup_at(&other_account, UNIX_EPOCH + Duration::from_secs(100)),
        CatalogLookup::Unknown { .. }
    ));

    let failed = store.refresh_at(scope.clone(), UNIX_EPOCH + Duration::from_secs(101), || {
        Err::<Vec<CatalogModel>, _>("native catalog unavailable")
    });
    let CatalogRefreshResult::Failed {
        retained, error, ..
    } = failed
    else {
        panic!("refresh failure should be reported");
    };
    assert_eq!(error, "native catalog unavailable");
    assert_eq!(
        retained.expect("prior evidence should remain").freshness,
        CatalogFreshness::Stale
    );
    assert!(matches!(
        store.lookup_at(&scope, UNIX_EPOCH + Duration::from_secs(101)),
        CatalogLookup::Stale { .. }
    ));
}

#[test]
fn selection_store_persists_only_selection_and_policy_metadata() {
    let directory = tempfile_directory();
    let path = directory.join("aifuel").join("execution.json");
    let mut config = GlobalSelectionConfig::default();
    config.defaults.provider = Some(ProviderKey::Codex);
    config.policy.allowed_roots.push(directory.clone());
    SelectionStore::save(&path, &config).expect("config should save");
    let bytes = std::fs::read(&path).expect("config should be readable");
    let text = String::from_utf8(bytes).expect("config is JSON");
    assert!(!text.contains("prompt"));
    assert_eq!(
        SelectionStore::load(&path).expect("config should load"),
        config
    );
}

fn tempfile_directory() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "aifuel-selection-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).expect("temporary directory should be created");
    path
}
