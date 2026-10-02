//! Tests for Key Pools: member addressing under a binding's Credential
//! Reference, health-state transitions the execution path records, and
//! backward compatibility with single-key store files written before pools
//! existed.

use super::{TestDir, api_key_store_binding, reference};
use crate::credentials::*;
use aifuel_core::{ApiKeySource, AuthBinding, IntegrationId, KeyDelivery};
use std::time::Duration;

fn integration() -> IntegrationId {
    IntegrationId::new("openai:api-key")
}

fn env_or_store_binding() -> AuthBinding {
    AuthBinding::ApiKey {
        source: ApiKeySource::EnvOrStore {
            var: "AIFUEL_POOL_TEST_ENV".to_owned(),
            credential: reference("openai:api-key"),
        },
        delivery: KeyDelivery::Bearer,
    }
}

#[test]
fn set_key_appends_members_to_the_pool() {
    let dir = TestDir::new();
    let store = dir.store();
    let root = reference("openai:api-key");
    let integration = integration();

    // The first key takes the base reference - identical to the pre-pool
    // single-credential record shape - and later keys take `root/N`.
    let (member, created) = store
        .add_pool_api_key(&root, "key-a", &integration)
        .unwrap();
    assert_eq!(member, root);
    assert!(created);

    let (member, created) = store
        .add_pool_api_key(&root, "key-b", &integration)
        .unwrap();
    assert_eq!(member.as_str(), "openai:api-key/2");
    assert!(created);

    let pool = store.api_key_pool(&root, &integration).unwrap();
    let refs: Vec<&str> = pool
        .iter()
        .map(|member| member.reference.as_ref().unwrap().as_str())
        .collect();
    assert_eq!(refs, ["openai:api-key", "openai:api-key/2"]);
    let keys: Vec<&str> = pool.iter().map(|member| member.key.as_str()).collect();
    assert_eq!(keys, ["key-a", "key-b"]);

    // Removing the base frees its slot for the next append.
    store.remove(&root).unwrap();
    let (member, _) = store
        .add_pool_api_key(&root, "key-c", &integration)
        .unwrap();
    assert_eq!(member, root);
    let keys: Vec<String> = store
        .api_key_pool(&root, &integration)
        .unwrap()
        .iter()
        .map(|member| member.key.clone())
        .collect();
    assert_eq!(keys, ["key-c", "key-b"]);
}

#[test]
fn re_storing_identical_material_revives_the_member() {
    let dir = TestDir::new();
    let store = dir.store();
    let root = reference("openai:api-key");
    let integration = integration();

    store
        .add_pool_api_key(&root, "key-a", &integration)
        .unwrap();
    store.mark_key_invalid(&root).unwrap();
    assert!(matches!(
        store.metadata(&root).unwrap().unwrap().key_health,
        Some(KeyHealth::Invalid { .. })
    ));

    // Re-storing the same key is the user's "this key is good again" claim:
    // no duplicate member, the recorded failure state clears.
    let (member, created) = store
        .add_pool_api_key(&root, "key-a", &integration)
        .unwrap();
    assert_eq!(member, root);
    assert!(!created);
    assert!(matches!(
        store.metadata(&root).unwrap().unwrap().key_health,
        Some(KeyHealth::Healthy)
    ));
    assert_eq!(store.list().unwrap().len(), 1);
}

#[test]
fn resolve_picks_the_first_healthy_pool_member() {
    let dir = TestDir::new();
    let store = dir.store();
    let root = reference("openai:api-key");
    let integration = integration();

    store
        .add_pool_api_key(&root, "key-a", &integration)
        .unwrap();
    store
        .add_pool_api_key(&root, "key-b", &integration)
        .unwrap();

    let binding = api_key_store_binding("openai:api-key");
    assert_eq!(
        store.resolve(&binding, &integration).unwrap(),
        ResolvedAuth::ApiKey {
            key: "key-a".to_owned(),
            delivery: KeyDelivery::Bearer,
        }
    );

    // With the first member cooling, single-key resolution - monitoring,
    // diagnostics - gets the next healthy member.
    store.mark_key_cooling(&root, None).unwrap();
    assert_eq!(
        store.resolve(&binding, &integration).unwrap(),
        ResolvedAuth::ApiKey {
            key: "key-b".to_owned(),
            delivery: KeyDelivery::Bearer,
        }
    );
}

#[test]
fn cooldown_persists_and_expires() {
    let dir = TestDir::new();
    let store = dir.store();
    let root = reference("openai:api-key");
    let integration = integration();

    store
        .add_pool_api_key(&root, "key-a", &integration)
        .unwrap();

    // Retry-After wins over backoff and persists in the record.
    store
        .mark_key_cooling(&root, Some(Duration::from_secs(120)))
        .unwrap();
    let pool = store.api_key_pool(&root, &integration).unwrap();
    assert_eq!(pool.len(), 1);
    let state = &pool[0].state;
    assert!(matches!(
        state.health(),
        KeyHealth::Cooling { until } if until > 0
    ));
    assert_eq!(state.cooling_step_seconds, Some(120));

    // An expired cooldown reads healthy again - state stays recorded but
    // produces no effect.
    store
        .update(|credentials| {
            if let Some(ManagedCredential::Api {
                state: Some(state), ..
            }) = credentials.get_mut(&root)
            {
                state.cooling_until = Some(1);
            }
            Ok(())
        })
        .unwrap();
    let pool = store.api_key_pool(&root, &integration).unwrap();
    assert!(pool[0].healthy());
}

#[test]
fn backoff_doubles_per_consecutive_rate_limit() {
    let dir = TestDir::new();
    let store = dir.store();
    let root = reference("openai:api-key");
    let integration = integration();
    store
        .add_pool_api_key(&root, "key-a", &integration)
        .unwrap();

    store.mark_key_cooling(&root, None).unwrap();
    let first = store.api_key_pool(&root, &integration).unwrap()[0]
        .state
        .cooling_step_seconds;
    store.mark_key_cooling(&root, None).unwrap();
    let second = store.api_key_pool(&root, &integration).unwrap()[0]
        .state
        .cooling_step_seconds;
    assert_eq!(first, Some(15));
    assert_eq!(second, Some(30));
}

#[test]
fn terminal_rejection_marks_invalid_and_success_clears_state() {
    let dir = TestDir::new();
    let store = dir.store();
    let root = reference("openai:api-key");
    let integration = integration();
    store
        .add_pool_api_key(&root, "key-a", &integration)
        .unwrap();

    store.mark_key_invalid(&root).unwrap();
    let pool = store.api_key_pool(&root, &integration).unwrap();
    assert!(!pool[0].healthy());
    assert!(matches!(pool[0].state.health(), KeyHealth::Invalid { .. }));

    // The success path clears every recorded failure state.
    store.clear_key_state(&root).unwrap();
    let pool = store.api_key_pool(&root, &integration).unwrap();
    assert!(pool[0].healthy());
    assert_eq!(pool[0].state, ApiKeyState::default());
}

#[test]
fn env_or_store_falls_back_only_when_the_pool_is_empty() {
    let dir = TestDir::new();
    let store = dir.store();
    let binding = env_or_store_binding();
    let integration = integration();
    let var = "AIFUEL_POOL_TEST_ENV";

    unsafe { std::env::remove_var(var) };
    assert!(matches!(
        store.resolve(&binding, &integration),
        Err(CredentialStoreError::EnvVarAbsent { .. })
    ));

    unsafe { std::env::set_var(var, "env-key") };
    assert_eq!(
        store.resolve(&binding, &integration).unwrap(),
        ResolvedAuth::ApiKey {
            key: "env-key".to_owned(),
            delivery: KeyDelivery::Bearer,
        }
    );

    // A stored member wins over the env var, exactly like before pools.
    store
        .add_pool_api_key(&reference("openai:api-key"), "stored-key", &integration)
        .unwrap();
    assert_eq!(
        store.resolve(&binding, &integration).unwrap(),
        ResolvedAuth::ApiKey {
            key: "stored-key".to_owned(),
            delivery: KeyDelivery::Bearer,
        }
    );
    unsafe { std::env::remove_var(var) };
}

#[test]
fn pool_members_share_the_destination_check_and_kind_rules() {
    let dir = TestDir::new();
    let store = dir.store();
    let root = reference("openai:api-key");
    let integration = integration();

    store
        .add_pool_api_key(&root, "key-a", &integration)
        .unwrap();
    // A member written for another destination refuses to resolve here.
    store
        .add_pool_api_key(
            &reference("other:api-key"),
            "foreign",
            &IntegrationId::new("other:api-key"),
        )
        .unwrap();
    let foreign_binding = api_key_store_binding("other:api-key");
    assert!(matches!(
        store.resolve(&foreign_binding, &integration),
        Err(CredentialStoreError::CredentialDestinationMismatch { .. })
    ));

    // A non-API-key record inside the pool namespace is a store
    // inconsistency, not a member to skip silently.
    store
        .set_oauth(&reference("openai:api-key/grant"), OAuthTokens::new("a"))
        .unwrap();
    assert!(matches!(
        store.api_key_pool(&root, &integration),
        Err(CredentialStoreError::UnexpectedCredentialKind { .. })
    ));
}

#[test]
fn an_empty_pool_reports_absent() {
    let dir = TestDir::new();
    let store = dir.store();
    let root = reference("openai:api-key");
    let integration = integration();

    store
        .add_pool_api_key(&root, "key-a", &integration)
        .unwrap();
    store.remove(&root).unwrap();

    assert!(!store.contains_credential(&root).unwrap());
    assert!(matches!(
        store.resolve(&api_key_store_binding("openai:api-key"), &integration),
        Err(CredentialStoreError::CredentialAbsent(_))
    ));
}

#[test]
fn member_only_pool_after_base_removal_resolves() {
    let dir = TestDir::new();
    let store = dir.store();
    let root = reference("openai:api-key");
    let integration = integration();

    store
        .add_pool_api_key(&root, "key-a", &integration)
        .unwrap();
    store
        .add_pool_api_key(&root, "key-b", &integration)
        .unwrap();
    store.remove(&root).unwrap();

    assert!(store.contains_credential(&root).unwrap());
    let pool = store.api_key_pool(&root, &integration).unwrap();
    assert_eq!(pool.len(), 1);
    assert_eq!(pool[0].key, "key-b");
}

#[test]
fn stores_written_before_pools_load_unchanged() {
    let dir = TestDir::new();
    let store = dir.store();
    // The pre-pool record shape: `type` + `key` (+ optional `destination`),
    // no `state` member. It must load, resolve, and report healthy.
    dir.write_data_file(
        br#"{
            "schema_version": 1,
            "credentials": {
                "openai:api-key": {"type": "api", "key": "legacy-key"},
                "work-anthropic": {"type": "api", "key": "other", "destination": "anthropic:api"},
                "grant": {"type": "oauth", "access": "a", "refresh": "r", "expires": 4102444800}
            }
        }"#,
    );

    let pool = store
        .api_key_pool(&reference("openai:api-key"), &integration())
        .unwrap();
    assert_eq!(pool.len(), 1);
    assert_eq!(pool[0].key, "legacy-key");
    assert_eq!(pool[0].state, ApiKeyState::default());
    assert!(pool[0].healthy());

    // metadata reports the key as healthy with no pool state recorded.
    let meta = store
        .metadata(&reference("openai:api-key"))
        .unwrap()
        .unwrap();
    assert_eq!(meta.key_health, Some(KeyHealth::Healthy));
}

#[test]
fn pool_state_never_enters_debug_output() {
    let dir = TestDir::new();
    let store = dir.store();
    let root = reference("openai:api-key");
    store
        .add_pool_api_key(&root, "secret-key", &integration())
        .unwrap();
    store.mark_key_invalid(&root).unwrap();

    let pool = store.api_key_pool(&root, &integration()).unwrap();
    let rendered = format!("{:?}", pool[0]);
    assert!(!rendered.contains("secret-key"));
    assert!(rendered.contains("openai:api-key"));
}
