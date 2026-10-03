use super::*;

/// A store rooted at a fresh temporary directory, mirroring
/// `route_planner`'s `test_store` pattern.
fn test_store(name: &str) -> KeyStore {
    let dir =
        std::env::temp_dir().join(format!("aifuel-gw-keys-test-{}-{name}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("stale test dir removable");
    }
    std::fs::create_dir_all(&dir).expect("test dir creatable");
    KeyStore::new(&dir)
}

#[test]
fn an_empty_store_authorizes_anonymously_until_a_key_lands() {
    // The transitional posture: before the first key exists every
    // request is anonymous so an unkeyed setup keeps working; the
    // moment a key lands, unauthenticated requests must fail or
    // keying a client would silently lock the owner out.
    let store = test_store("anonymous");
    assert_eq!(store.authorize(None).expect("open store"), "anonymous");
    assert_eq!(
        store
            .authorize(Some("aifuel-gw-ignored"))
            .expect("open store ignores any presented key"),
        "anonymous"
    );
    store.create("laptop", None).expect("create");
    assert_eq!(
        store.authorize(None).unwrap_err(),
        "missing api key",
        "with a key on file, an absent bearer must now reject"
    );
}

#[test]
fn created_keys_authorize_until_revoked() {
    // The whole lifecycle: issue, match by digest, stamp use, reject
    // after revocation - without ever storing the raw key.
    let store = test_store("round-trip");
    let (summary, raw) = store.create("laptop", None).expect("create");
    assert!(
        raw.starts_with(KEY_PREFIX) && raw.len() == KEY_PREFIX.len() + 32,
        "raw key keeps the aifuel-gw-<32 hex> contract"
    );
    assert_eq!(store.authorize(Some(&raw)).expect("valid key"), "laptop");
    let listed = store.list().expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, summary.id);
    assert!(
        listed[0].last_used_at.is_some(),
        "a successful authorize stamps last_used_at"
    );
    store.revoke(&summary.id).expect("revoke");
    assert_eq!(
        store.authorize(Some(&raw)).unwrap_err(),
        "invalid api key",
        "a revoked key must reject like an unknown one"
    );
    assert_eq!(
        store.authorize(Some("aifuel-gw-unknown")).unwrap_err(),
        "invalid api key",
        "an unknown key rejects with the contract message"
    );
}

#[test]
fn the_store_never_persists_key_material() {
    // Credential material is never serialized: the file holds only the
    // digest and the display prefix, and KeySummary drops even those.
    let store = test_store("no-material");
    let (summary, raw) = store.create("laptop", None).expect("create");
    let on_disk = std::fs::read_to_string(&store.path).expect("store file readable");
    assert!(
        !on_disk.contains(&raw),
        "the raw key must never reach the store file"
    );
    assert!(
        on_disk.contains(&sha256_hex(raw.as_bytes())),
        "the stored digest is the sha256 of the full key"
    );
    assert!(on_disk.contains(&summary.prefix));
    let listed = serde_json::to_value(&summary).expect("summary serializes");
    let text = listed.to_string();
    assert!(!text.contains(&raw) && !text.contains("sha256"));
}

#[test]
fn a_malformed_store_errors_instead_of_resetting() {
    // A hand-edited or torn file must surface, not silently wipe the
    // key set - and never panic the request thread.
    let store = test_store("malformed");
    std::fs::write(&store.path, b"not json").expect("seed malformed file");
    assert!(store.list().is_err());
    assert!(store.authorize(None).is_err());
    std::fs::write(&store.path, br#"{"version":2,"keys":[]}"#).expect("seed future version");
    assert!(
        store.list().is_err(),
        "a newer schema version reports rather than silently adopting"
    );
}

#[test]
fn permits_honors_the_stored_allowlist() {
    // The allowlist is recorded now and reported through the admin
    // api; when #109 wires enforcement, this predicate is the check.
    let store = test_store("permits");
    let (scoped, _) = store
        .create("scoped", Some(vec!["auto".to_owned()]))
        .expect("create scoped");
    let (open, _) = store.create("full", None).expect("create open");
    assert!(store.permits(&scoped.name, "auto"));
    assert!(!store.permits(&scoped.name, "codex"));
    assert!(store.permits(&open.name, "codex"));
    let listed = store.list().expect("list");
    assert_eq!(listed[0].models, Some(vec!["auto".to_owned()]));
    // The empty store is the open transitional posture.
    let empty = test_store("permits-empty");
    assert!(empty.permits("anonymous", "anything"));
}

#[test]
fn update_replaces_and_clears_the_allowlist() {
    // `update` moves only `models`: the row keeps its name, digest, and
    // history, and `permits` tracks whichever list is current - the admin
    // api's allowlist edit must take effect without reissuing the key.
    let store = test_store("update");
    let (summary, _) = store
        .create("scoped", Some(vec!["auto".to_owned()]))
        .expect("create scoped");
    assert!(store.permits("scoped", "auto"));
    assert!(!store.permits("scoped", "codex"));

    store
        .update(&summary.id, Some(vec!["codex".to_owned()]))
        .expect("replace the allowlist");
    assert!(
        !store.permits("scoped", "auto"),
        "a replaced allowlist drops the old members"
    );
    assert!(store.permits("scoped", "codex"));
    let listed = store.list().expect("list");
    assert_eq!(listed[0].models, Some(vec!["codex".to_owned()]));

    store
        .update(&summary.id, None)
        .expect("clear the allowlist");
    assert!(
        store.permits("scoped", "auto") && store.permits("scoped", "anything"),
        "a cleared allowlist restores the every-model posture"
    );

    assert!(
        store.update("k_missing", None).is_err(),
        "an unknown id reports instead of silently no-op'ing"
    );
}

#[test]
fn the_permits_gate_combines_authorize_and_the_allowlist() {
    // `require_permits` is `authorize` then `permits` over the default
    // store; a `tiny_http::Request` cannot be built in a unit test, so
    // drive the same pair store-level: the identity authorize resolves is
    // the identity the allowlist checks, and a rejected bearer means the
    // gate errors before `permits` is ever consulted.
    let store = test_store("require-permits");
    let (_, raw) = store
        .create("scoped", Some(vec!["auto".to_owned()]))
        .expect("create scoped");

    let identity = store.authorize(Some(&raw)).expect("valid key");
    assert_eq!(identity, "scoped");
    assert!(store.permits(&identity, "auto"));
    assert!(
        !store.permits(&identity, "gpt-5"),
        "a valid key for a disallowed model is the 403 case"
    );
    assert!(
        store.authorize(Some("aifuel-gw-bogus")).is_err(),
        "an unknown bearer is the gate's first failure mode"
    );
}

#[test]
fn bearer_parsing_accepts_the_scheme_case_insensitively() {
    let header = |value: &str| {
        tiny_http::Header::from_bytes("Authorization", value).expect("header constructs")
    };
    assert_eq!(
        bearer_token(&[header("Bearer aifuel-gw-x")]),
        Some("aifuel-gw-x")
    );
    assert_eq!(
        bearer_token(&[header("bearer   aifuel-gw-x  ")]),
        Some("aifuel-gw-x")
    );
    assert_eq!(bearer_token(&[header("Basic aifuel-gw-x")]), None);
    assert_eq!(bearer_token(&[]), None);
    assert_eq!(bearer_token(&[header("Bearer")]), None);
}

#[test]
fn a_summary_carries_every_admin_field_but_no_material() {
    let summary = KeySummary {
        id: "k_1".to_owned(),
        name: "laptop".to_owned(),
        prefix: "aifuel-gw-abcdef12".to_owned(),
        created_at: 1,
        models: Some(vec!["auto".to_owned()]),
        revoked: false,
        last_used_at: Some(2),
    };
    let value = serde_json::to_value(&summary).expect("serializes");
    assert_eq!(value["id"], "k_1");
    assert_eq!(value["name"], "laptop");
    assert_eq!(value["prefix"], "aifuel-gw-abcdef12");
    assert_eq!(value["models"], serde_json::json!(["auto"]));
    assert_eq!(value["last_used_at"], 2);
    assert!(value.get("sha256").is_none());
    assert!(value.get("key").is_none());
}
