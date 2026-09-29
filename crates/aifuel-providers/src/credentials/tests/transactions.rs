//! Tests for the transaction rules: lock serialization, atomic replacement,
//! corruption handling, schema versioning, and refresh-token preservation.

use super::{TestDir, reference};
use crate::credentials::*;
use aifuel_core::CredentialRef;
use std::fs;
use std::sync::Barrier;

#[test]
fn a_malformed_store_errors_and_is_never_overwritten() {
    let dir = TestDir::new();
    let store = dir.store();
    dir.write_data_file(b"{ not json");

    assert!(matches!(
        store.get(&reference("a")),
        Err(CredentialStoreError::Corrupt { .. })
    ));
    assert!(matches!(
        store.list(),
        Err(CredentialStoreError::Corrupt { .. })
    ));
    assert!(matches!(
        store.set_api_key(&reference("a"), "key"),
        Err(CredentialStoreError::Corrupt { .. })
    ));

    // The malformed file is left byte-for-byte intact.
    assert_eq!(dir.data_file_contents(), b"{ not json");
}

#[test]
fn an_unknown_schema_version_is_rejected() {
    let dir = TestDir::new();
    let store = dir.store();
    dir.write_data_file(br#"{"schema_version": 99, "credentials": {}}"#);

    assert!(matches!(
        store.get(&reference("a")),
        Err(CredentialStoreError::UnknownSchemaVersion { found: 99 })
    ));
    assert!(matches!(
        store.set_api_key(&reference("a"), "key"),
        Err(CredentialStoreError::UnknownSchemaVersion { found: 99 })
    ));
    assert_eq!(
        dir.data_file_contents(),
        br#"{"schema_version": 99, "credentials": {}}"#
    );
}

#[test]
fn concurrent_mutations_serialize_on_the_sidecar_lock() {
    const THREADS: usize = 8;
    let dir = TestDir::new();
    let store = dir.store();
    let barrier = Barrier::new(THREADS);

    std::thread::scope(|scope| {
        for index in 0..THREADS {
            let store = &store;
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                store
                    .set_api_key(
                        &reference(&format!("key-{index}")),
                        &format!("value-{index}"),
                    )
                    .expect("set should succeed under the lock");
            });
        }
    });

    assert_eq!(store.list().unwrap().len(), THREADS);
    for index in 0..THREADS {
        assert_eq!(
            store.get(&reference(&format!("key-{index}"))).unwrap(),
            Some(ManagedCredential::api(format!("value-{index}")))
        );
    }
}

#[test]
fn a_rewrite_omitting_refresh_preserves_the_stored_grant() {
    let dir = TestDir::new();
    let store = dir.store();
    let reference = reference("grant");

    store
        .set_oauth(
            &reference,
            OAuthTokens {
                access: "access-1".to_owned(),
                refresh: Some("refresh-1".to_owned()),
                expires: Some(100),
                account_id: None,
                destination: None,
            },
        )
        .unwrap();

    // A refresh response without a replacement grant preserves the stored
    // one, whether written through set_oauth or update().
    store
        .set_oauth(&reference, OAuthTokens::new("access-2"))
        .unwrap();
    assert_eq!(
        store.get(&reference).unwrap(),
        Some(ManagedCredential::OAuth {
            access: "access-2".to_owned(),
            refresh: Some("refresh-1".to_owned()),
            expires: None,
            account_id: None,
            destination: None,
        })
    );

    store
        .update(|credentials| {
            credentials.insert(
                reference.clone(),
                ManagedCredential::OAuth {
                    access: "access-3".to_owned(),
                    refresh: None,
                    expires: Some(200),
                    account_id: None,
                    destination: None,
                },
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(
        store.get(&reference).unwrap(),
        Some(ManagedCredential::OAuth {
            access: "access-3".to_owned(),
            refresh: Some("refresh-1".to_owned()),
            expires: Some(200),
            account_id: None,
            destination: None,
        })
    );
}

#[test]
fn remove_drops_the_grant_and_nothing_resurrects_it() {
    let dir = TestDir::new();
    let store = dir.store();
    let reference = reference("grant");

    store
        .set_oauth(
            &reference,
            OAuthTokens {
                access: "access-1".to_owned(),
                refresh: Some("refresh-1".to_owned()),
                expires: None,
                account_id: None,
                destination: None,
            },
        )
        .unwrap();
    store.remove(&reference).unwrap();

    // After removal the refresh guard has nothing to preserve.
    store
        .set_oauth(&reference, OAuthTokens::new("access-2"))
        .unwrap();
    assert_eq!(
        store.get(&reference).unwrap(),
        Some(ManagedCredential::OAuth {
            access: "access-2".to_owned(),
            refresh: None,
            expires: None,
            account_id: None,
            destination: None,
        })
    );
}

#[test]
fn the_lock_is_a_stable_sidecar_next_to_the_data_file() {
    let dir = TestDir::new();
    let store = dir.store();

    store.set_api_key(&reference("a"), "key").unwrap();

    let lock_path = dir.path.join("credentials.json.lock");
    assert_ne!(lock_path, store.path());
    assert!(lock_path.exists());

    // The data file itself stays a valid JSON document; the lock is never
    // taken on it.
    let parsed: serde_json::Value =
        serde_json::from_slice(&dir.data_file_contents()).expect("data file should parse");
    assert_eq!(parsed["schema_version"], 1);
    assert_eq!(parsed["credentials"]["a"]["type"], "api");
    assert_eq!(parsed["credentials"]["a"]["key"], "key");
}

#[cfg(unix)]
#[test]
fn the_data_file_is_created_mode_0600() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TestDir::new();
    let store = dir.store();
    store.set_api_key(&reference("a"), "key").unwrap();

    let mode = fs::metadata(store.path())
        .expect("data file should be inspectable")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn the_stored_record_matches_the_auth_json_shape() {
    let dir = TestDir::new();
    let store = dir.store();

    store
        .set_oauth(
            &reference("copilot-oauth"),
            OAuthTokens {
                access: "a".to_owned(),
                refresh: Some("r".to_owned()),
                expires: Some(1_735_689_600),
                account_id: Some("acct".to_owned()),
                destination: None,
            },
        )
        .unwrap();

    let parsed: serde_json::Value =
        serde_json::from_slice(&dir.data_file_contents()).expect("data file should parse");
    assert_eq!(
        parsed["credentials"]["copilot-oauth"],
        serde_json::json!({
            "type": "oauth",
            "access": "a",
            "refresh": "r",
            "expires": 1_735_689_600,
            "account_id": "acct"
        })
    );
}

#[test]
fn empty_material_is_not_a_credential() {
    let dir = TestDir::new();
    let store = dir.store();

    assert!(matches!(
        store.set_api_key(&reference("a"), ""),
        Err(CredentialStoreError::InvalidMaterial(_))
    ));
    assert!(matches!(
        store.set_oauth(&reference("b"), OAuthTokens::new("")),
        Err(CredentialStoreError::InvalidMaterial(_))
    ));
    assert_eq!(store.list().unwrap().len(), 0);
}

#[test]
fn update_exposes_the_decoded_map_for_locked_rechecks() {
    let dir = TestDir::new();
    let store = dir.store();
    let reference: CredentialRef = reference("grant");
    store.set_oauth(&reference, OAuthTokens::new("a")).unwrap();

    // The closure sees the rereaded store and its return value flows out.
    let present = store
        .update(|credentials| Ok(credentials.contains_key(&reference)))
        .unwrap();
    assert!(present);
}

#[test]
fn a_failed_mutation_persists_nothing() {
    let dir = TestDir::new();
    let store = dir.store();
    let reference = reference("grant");
    store
        .set_oauth(&reference, OAuthTokens::new("original"))
        .unwrap();

    // Spec transaction rule: a mutate error aborts before persist, so the
    // insertion the closure made never reaches the file.
    let outcome: Result<(), CredentialStoreError> = store.update(|credentials| {
        credentials.insert(
            reference.clone(),
            ManagedCredential::oauth(OAuthTokens::new("replaced")),
        );
        Err(CredentialStoreError::LockedTimeout)
    });
    assert!(matches!(outcome, Err(CredentialStoreError::LockedTimeout)));
    assert_eq!(
        store.get(&reference).unwrap(),
        Some(ManagedCredential::oauth(OAuthTokens::new("original")))
    );
}
