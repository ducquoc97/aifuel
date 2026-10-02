use super::*;
use aifuel_core::{
    ApiKeySource, AuthBinding, CredentialRef, IntegrationId, KeyDelivery, OAuthProfileId,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

mod pool;
mod transactions;

/// The integration id resolve tests run under; test credentials are unbound,
/// so any id satisfies the destination check.
fn test_integration() -> IntegrationId {
    IntegrationId::new("test-integration")
}

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

pub(super) struct TestDir {
    pub path: PathBuf,
}

impl TestDir {
    pub(super) fn new() -> Self {
        let suffix = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aifuel-credentials-test-{}-{}",
            std::process::id(),
            suffix
        ));
        fs::create_dir_all(&path).expect("test dir should be creatable");
        Self { path }
    }

    pub(super) fn store(&self) -> CredentialStore {
        CredentialStore::new(&self.path)
    }

    pub(super) fn write_data_file(&self, contents: &[u8]) {
        fs::write(self.path.join("credentials.json"), contents)
            .expect("data file should be writable");
    }

    pub(super) fn data_file_contents(&self) -> Vec<u8> {
        fs::read(self.path.join("credentials.json")).expect("data file should be readable")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(super) fn reference(name: &str) -> CredentialRef {
    CredentialRef::new(name)
}

pub(super) fn oauth_binding(name: &str) -> AuthBinding {
    AuthBinding::OAuth {
        credential: reference(name),
        profile: OAuthProfileId::new("test-profile"),
    }
}

pub(super) fn api_key_store_binding(name: &str) -> AuthBinding {
    AuthBinding::ApiKey {
        source: ApiKeySource::Store {
            credential: reference(name),
        },
        delivery: KeyDelivery::Bearer,
    }
}

#[test]
fn api_keys_round_trip_through_set_get_and_remove() {
    let dir = TestDir::new();
    let store = dir.store();
    let reference = reference("work-anthropic");

    assert_eq!(store.get(&reference).unwrap(), None);

    store.set_api_key(&reference, "sk-test-123").unwrap();
    assert_eq!(
        store.get(&reference).unwrap(),
        Some(ManagedCredential::api("sk-test-123"))
    );

    assert!(store.remove(&reference).unwrap());
    assert_eq!(store.get(&reference).unwrap(), None);
    assert!(!store.remove(&reference).unwrap());
}

#[test]
fn oauth_grants_round_trip_with_optional_fields() {
    let dir = TestDir::new();
    let store = dir.store();
    let reference = reference("copilot-oauth");

    store
        .set_oauth(
            &reference,
            OAuthTokens {
                access: "access-1".to_owned(),
                refresh: Some("refresh-1".to_owned()),
                expires: Some(4_102_444_800),
                account_id: Some("acct-1".to_owned()),
                destination: None,
            },
        )
        .unwrap();

    assert_eq!(
        store.get(&reference).unwrap(),
        Some(ManagedCredential::OAuth {
            access: "access-1".to_owned(),
            refresh: Some("refresh-1".to_owned()),
            expires: Some(4_102_444_800),
            account_id: Some("acct-1".to_owned()),
            destination: None,
        })
    );
}

#[test]
fn metadata_reports_kind_expiry_and_account_without_material() {
    let dir = TestDir::new();
    let store = dir.store();
    let api = reference("api");
    let oauth = reference("oauth");

    assert_eq!(store.metadata(&api).unwrap(), None);

    store.set_api_key(&api, "secret-key").unwrap();
    store
        .set_oauth(
            &oauth,
            OAuthTokens {
                access: "secret-access".to_owned(),
                refresh: Some("secret-refresh".to_owned()),
                expires: Some(4_102_444_800),
                account_id: Some("acct-1".to_owned()),
                destination: None,
            },
        )
        .unwrap();

    let api_meta = store.metadata(&api).unwrap().unwrap();
    assert_eq!(api_meta.kind, CredentialKind::ApiKey);
    assert_eq!(api_meta.expiry, CredentialExpiry::None);
    assert_eq!(api_meta.account_id, None);

    let oauth_meta = store.metadata(&oauth).unwrap().unwrap();
    assert_eq!(oauth_meta.kind, CredentialKind::OAuth);
    assert_eq!(
        oauth_meta.expiry,
        CredentialExpiry::Valid {
            until: 4_102_444_800
        }
    );
    assert_eq!(oauth_meta.account_id.as_deref(), Some("acct-1"));

    // Rule 9: a metadata read never carries secret material.
    let rendered = format!("{api_meta:?} {oauth_meta:?}");
    assert!(!rendered.contains("secret-key"));
    assert!(!rendered.contains("secret-access"));
    assert!(!rendered.contains("secret-refresh"));
}

#[test]
fn list_returns_each_reference_with_metadata() {
    let dir = TestDir::new();
    let store = dir.store();

    store.set_api_key(&reference("b-key"), "v").unwrap();
    store
        .set_oauth(&reference("a-grant"), OAuthTokens::new("a"))
        .unwrap();

    let listed = store.list().unwrap();
    let names: Vec<&str> = listed.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(names, vec!["a-grant", "b-key"]);
    assert_eq!(listed[0].1.kind, CredentialKind::OAuth);
    assert_eq!(listed[1].1.kind, CredentialKind::ApiKey);
}

#[test]
fn debug_output_redacts_credential_material() {
    let credential = ManagedCredential::OAuth {
        access: "secret-access".to_owned(),
        refresh: Some("secret-refresh".to_owned()),
        expires: Some(123),
        account_id: Some("acct".to_owned()),
        destination: None,
    };
    let rendered = format!("{credential:?}");
    assert!(!rendered.contains("secret-access"));
    assert!(!rendered.contains("secret-refresh"));
    assert!(rendered.contains("acct"));

    let tokens = OAuthTokens {
        access: "secret-access".to_owned(),
        refresh: Some("secret-refresh".to_owned()),
        expires: None,
        account_id: None,
        destination: None,
    };
    let rendered = format!("{tokens:?}");
    assert!(!rendered.contains("secret-access"));
    assert!(!rendered.contains("secret-refresh"));

    let resolved = ResolvedAuth::ApiKey {
        key: "secret-key".to_owned(),
        delivery: KeyDelivery::Bearer,
    };
    assert!(!format!("{resolved:?}").contains("secret-key"));
}

#[test]
fn env_var_is_a_credential_only_when_declared_and_nonempty() {
    let dir = TestDir::new();
    let store = dir.store();
    let var = "AIFUEL_CREDENTIALS_TEST_ENV";
    let binding = AuthBinding::ApiKey {
        source: ApiKeySource::Env {
            var: var.to_owned(),
        },
        delivery: KeyDelivery::Bearer,
    };

    // Unset and empty variables are not credentials.
    unsafe { std::env::remove_var(var) };
    assert!(matches!(
        store.resolve(&binding, &test_integration()),
        Err(CredentialStoreError::EnvVarAbsent { .. })
    ));
    unsafe { std::env::set_var(var, "") };
    assert!(matches!(
        store.resolve(&binding, &test_integration()),
        Err(CredentialStoreError::EnvVarAbsent { .. })
    ));

    unsafe { std::env::set_var(var, "env-key") };
    match store.resolve(&binding, &test_integration()).unwrap() {
        ResolvedAuth::ApiKey { key, delivery } => {
            assert_eq!(key, "env-key");
            assert_eq!(delivery, KeyDelivery::Bearer);
        }
        other => panic!("expected an API key, got {other:?}"),
    }
    unsafe { std::env::remove_var(var) };
}

#[test]
fn resolve_applies_each_binding_kind() {
    let dir = TestDir::new();
    let store = dir.store();

    assert_eq!(
        store
            .resolve(&AuthBinding::None, &test_integration())
            .unwrap(),
        ResolvedAuth::None
    );

    store.set_api_key(&reference("key"), "stored-key").unwrap();
    assert_eq!(
        store
            .resolve(&api_key_store_binding("key"), &test_integration())
            .unwrap(),
        ResolvedAuth::ApiKey {
            key: "stored-key".to_owned(),
            delivery: KeyDelivery::Bearer,
        }
    );

    // A missing credential and a wrong-kind credential are distinct errors.
    assert!(matches!(
        store.resolve(&api_key_store_binding("absent"), &test_integration()),
        Err(CredentialStoreError::CredentialAbsent(_))
    ));
    store
        .set_oauth(&reference("grant"), OAuthTokens::new("a"))
        .unwrap();
    assert!(matches!(
        store.resolve(&api_key_store_binding("grant"), &test_integration()),
        Err(CredentialStoreError::UnexpectedCredentialKind { .. })
    ));
}

#[test]
fn sessions_round_trip_as_single_records_and_replace_on_restore() {
    let dir = TestDir::new();
    let store = dir.store();
    let reference = reference("web:session");

    assert_eq!(store.get(&reference).unwrap(), None);
    store.set_session(&reference, "session-token").unwrap();
    assert_eq!(
        store.get(&reference).unwrap(),
        Some(ManagedCredential::session("session-token"))
    );

    // A refreshed cookie replaces the stale record rather than stacking
    // into a pool member.
    store.set_session(&reference, "Cookie: a=1; b=2").unwrap();
    assert_eq!(
        store.get(&reference).unwrap(),
        Some(ManagedCredential::session("Cookie: a=1; b=2"))
    );
    assert_eq!(store.list().unwrap().len(), 1);
    assert!(!dir.data_file_contents().is_empty());

    // Sessions are stored verbatim; an empty value is not a credential.
    assert!(matches!(
        store.set_session(&reference, ""),
        Err(CredentialStoreError::InvalidMaterial(_))
    ));
    assert!(matches!(
        store.set_session_for(&reference, "", &test_integration()),
        Err(CredentialStoreError::InvalidMaterial(_))
    ));

    // Session metadata reports the kind with no expiry and no key health.
    let meta = store.metadata(&reference).unwrap().unwrap();
    assert_eq!(meta.kind, CredentialKind::Session);
    assert_eq!(meta.expiry, CredentialExpiry::None);
}

#[test]
fn session_serializes_as_its_own_type_and_debug_redacts_material() {
    // Backward compatibility: the record must deserialize as a distinct
    // `"session"` type so older readers refuse it honestly and newer ones
    // keep it out of API-key pools.
    let json: serde_json::Value =
        serde_json::from_slice(&serde_json::to_vec(&ManagedCredential::session("raw")).unwrap())
            .unwrap();
    assert_eq!(json["type"], "session");
    let rendered = format!("{:?}", ManagedCredential::session("raw-secret"));
    assert!(!rendered.contains("raw-secret"));
}

#[test]
fn cookie_delivery_resolves_session_material_never_pool_members() {
    let dir = TestDir::new();
    let store = dir.store();
    let session_ref = reference("web:session");
    let binding = AuthBinding::ApiKey {
        source: ApiKeySource::Store {
            credential: session_ref.clone(),
        },
        delivery: KeyDelivery::Cookie {
            name: "sessionKey".to_owned(),
        },
    };

    store
        .set_session_for(&session_ref, "session-material", &test_integration())
        .unwrap();
    assert_eq!(
        store.resolve(&binding, &test_integration()).unwrap(),
        ResolvedAuth::ApiKey {
            key: "session-material".to_owned(),
            delivery: KeyDelivery::Cookie {
                name: "sessionKey".to_owned()
            },
        }
    );

    // A session record under the pool namespace must not satisfy a Bearer
    // binding, and an API-key record must not satisfy a Cookie binding:
    // sending either across kinds is a kind error, not a silent skip.
    store.set_api_key(&reference("api"), "sk").unwrap();
    let cookie_at_api = AuthBinding::ApiKey {
        source: ApiKeySource::Store {
            credential: reference("api"),
        },
        delivery: KeyDelivery::Cookie {
            name: "sessionKey".to_owned(),
        },
    };
    assert!(matches!(
        store.resolve(&cookie_at_api, &test_integration()),
        Err(CredentialStoreError::UnexpectedCredentialKind { .. })
    ));
    let bearer_at_session = AuthBinding::ApiKey {
        source: ApiKeySource::Store {
            credential: session_ref.clone(),
        },
        delivery: KeyDelivery::Bearer,
    };
    assert!(matches!(
        store.resolve(&bearer_at_session, &test_integration()),
        Err(CredentialStoreError::UnexpectedCredentialKind { .. })
    ));
}

#[test]
fn a_session_resolves_only_for_its_recorded_destination() {
    let dir = TestDir::new();
    let store = dir.store();
    let session_ref = reference("web:session");
    let binding = AuthBinding::ApiKey {
        source: ApiKeySource::Store {
            credential: session_ref.clone(),
        },
        delivery: KeyDelivery::Cookie {
            name: "sessionKey".to_owned(),
        },
    };

    store
        .set_session_for(&session_ref, "s", &IntegrationId::new("claude-web:web"))
        .unwrap();
    assert!(matches!(
        store.resolve(&binding, &IntegrationId::new("other:web")),
        Err(CredentialStoreError::CredentialDestinationMismatch { .. })
    ));
    assert!(
        store
            .resolve(&binding, &IntegrationId::new("claude-web:web"))
            .is_ok()
    );

    // EnvOrStore prefers the stored session; absent, it falls back to the
    // declared variable.
    let var = "AIFUEL_CREDENTIALS_TEST_SESSION_ENV";
    let env_or_store = AuthBinding::ApiKey {
        source: ApiKeySource::EnvOrStore {
            var: var.to_owned(),
            credential: reference("absent-session"),
        },
        delivery: KeyDelivery::Cookie {
            name: "sessionKey".to_owned(),
        },
    };
    unsafe { std::env::set_var(var, "env-session") };
    assert_eq!(
        store.resolve(&env_or_store, &test_integration()).unwrap(),
        ResolvedAuth::ApiKey {
            key: "env-session".to_owned(),
            delivery: KeyDelivery::Cookie {
                name: "sessionKey".to_owned()
            },
        }
    );
    unsafe { std::env::remove_var(var) };
    assert!(matches!(
        store.resolve(&env_or_store, &test_integration()),
        Err(CredentialStoreError::EnvVarAbsent { .. })
    ));
}

#[test]
fn expired_oauth_grants_resolve_with_needs_refresh() {
    let dir = TestDir::new();
    let store = dir.store();

    store
        .set_oauth(
            &reference("expired"),
            OAuthTokens {
                access: "old-access".to_owned(),
                refresh: Some("r".to_owned()),
                expires: Some(1),
                account_id: None,
                destination: None,
            },
        )
        .unwrap();
    store
        .set_oauth(
            &reference("fresh"),
            OAuthTokens {
                access: "live-access".to_owned(),
                refresh: Some("r".to_owned()),
                expires: Some(4_102_444_800),
                account_id: None,
                destination: None,
            },
        )
        .unwrap();
    store
        .set_oauth(&reference("timeless"), OAuthTokens::new("t"))
        .unwrap();

    let assert_flag = |name: &str, expected: bool| match store
        .resolve(&oauth_binding(name), &test_integration())
        .unwrap()
    {
        ResolvedAuth::OAuth {
            access_token,
            needs_refresh,
        } => {
            assert_eq!(needs_refresh, expected);
            assert!(!access_token.is_empty());
        }
        other => panic!("expected OAuth material, got {other:?}"),
    };

    assert_flag("expired", true);
    assert_flag("fresh", false);
    assert_flag("timeless", false);
}
