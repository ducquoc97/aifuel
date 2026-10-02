use std::fs;
use std::process::Output;

use crate::support::{TestDirectory, ai_fuel_config_dir};

/// Run `aifuel auth …` against an isolated home so the real credential
/// store and environment stay untouched. `OPENAI_API_KEY` is stripped:
/// `openai:api-key` reads it via `EnvOrStore` and a leaked host value
/// would flip the reported source.
fn aifuel(directory: &TestDirectory, args: &[&str]) -> Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_aifuel"));
    command
        .args(args)
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"));
    for var in ["OPENAI_API_KEY", "OPENROUTER_API_KEY"] {
        command.env_remove(var);
    }
    command.output().expect("aifuel should start")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn set_key_appends_pool_members_and_list_reports_per_key_health() {
    let directory = TestDirectory::new("auth-pool");
    let secrets = ["sk-one-secret", "sk-two-secret"];

    // The first key takes the binding's Credential Reference; the second
    // appends `…/2`.
    let first = aifuel(
        &directory,
        &["auth", "set-key", "openai:api-key", "--key", secrets[0]],
    );
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(
        stdout(&first)
            .contains("Stored API key as credential openai:api-key bound to openai:api-key"),
        "{}",
        stdout(&first)
    );

    let second = aifuel(
        &directory,
        &["auth", "set-key", "openai:api-key", "--key", secrets[1]],
    );
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(
        stdout(&second)
            .contains("Added API key to the openai:api-key pool as credential openai:api-key/2"),
        "{}",
        stdout(&second)
    );

    // Both members exist on disk as one pool under the binding reference.
    let store_file = ai_fuel_config_dir(directory.path()).join("credentials.json");
    let stored: serde_json::Value =
        serde_json::from_slice(&fs::read(store_file).expect("credentials.json exists"))
            .expect("credentials.json parses");
    for reference in ["openai:api-key", "openai:api-key/2"] {
        assert_eq!(
            stored["credentials"][reference]["type"], "api",
            "{reference} should be stored as an API credential"
        );
    }

    // `auth list` reports the pool rollup and one line per member, and
    // never prints the key material.
    let list = aifuel(&directory, &["auth", "list"]);
    assert!(list.status.success(), "{}", stderr(&list));
    let listed = stdout(&list);
    assert!(
        listed.contains("key pool openai:api-key (2 keys: 2 healthy)"),
        "{listed}"
    );
    for reference in ["openai:api-key", "openai:api-key/2"] {
        assert!(
            listed.contains(reference),
            "{reference} missing in {listed}"
        );
    }
    for secret in secrets {
        assert!(!listed.contains(secret), "key material leaked in {listed}");
    }

    // The JSON view carries the same per-member health, machine-readable.
    let json = aifuel(&directory, &["auth", "list", "--json"]);
    assert!(json.status.success(), "{}", stderr(&json));
    let value: serde_json::Value =
        serde_json::from_slice(&json.stdout).expect("auth list --json emits JSON");
    let members: Vec<&serde_json::Value> = value["credentials"]
        .as_array()
        .expect("credentials is an array")
        .iter()
        .filter(|entry| {
            entry["credential"]
                .as_str()
                .is_some_and(|name| name.starts_with("openai:api-key"))
        })
        .collect();
    assert_eq!(members.len(), 2, "{value}");
    assert!(
        members
            .iter()
            .all(|entry| entry["health"] == "healthy" && entry["kind"] == "api_key"),
        "{value}"
    );
    assert_eq!(members[1]["pool"], "openai:api-key", "{value}");
    for secret in secrets {
        assert!(
            !stdout(&json).contains(secret),
            "key material leaked in JSON output"
        );
    }
}

#[test]
fn remove_deletes_one_member_and_warns_only_when_the_pool_empties() {
    let directory = TestDirectory::new("auth-remove");
    for key in ["key-a", "key-b"] {
        let output = aifuel(
            &directory,
            &["auth", "set-key", "openai:api-key", "--key", key],
        );
        assert!(output.status.success(), "{}", stderr(&output));
    }

    // Removing a member keeps the pool - no auth warning, a retention note.
    let member = aifuel(&directory, &["auth", "remove", "openai:api-key/2"]);
    assert!(member.status.success(), "{}", stderr(&member));
    assert!(
        stdout(&member).contains("retains 1 key(s)"),
        "{}",
        stdout(&member)
    );
    assert!(!stderr(&member).contains("fail authentication"));

    // Removing the last member strands the binding, which is the warning.
    let last = aifuel(&directory, &["auth", "remove", "openai:api-key"]);
    assert!(last.status.success(), "{}", stderr(&last));
    assert!(
        stderr(&last).contains("pool now holds no keys"),
        "{}",
        stderr(&last)
    );

    // Nothing left: the integration reports the credential absent again.
    let list = aifuel(&directory, &["auth", "list"]);
    assert!(
        stdout(&list).contains("managed credential openai:api-key (absent)"),
        "{}",
        stdout(&list)
    );
}

#[test]
fn a_raw_credential_reference_overwrites_its_exact_slot() {
    let directory = TestDirectory::new("auth-raw-ref");

    // A target that names no integration stores a plain credential.
    let store = aifuel(
        &directory,
        &["auth", "set-key", "scratch:key", "--key", "one"],
    );
    assert!(store.status.success(), "{}", stderr(&store));
    assert!(stdout(&store).contains("Stored API key as credential scratch:key."));

    // Repeating the command on the same reference overwrites, never appends.
    let store = aifuel(
        &directory,
        &["auth", "set-key", "scratch:key", "--key", "two"],
    );
    assert!(store.status.success(), "{}", stderr(&store));
    let store_file = ai_fuel_config_dir(directory.path()).join("credentials.json");
    let stored: serde_json::Value =
        serde_json::from_slice(&fs::read(store_file).expect("credentials.json exists"))
            .expect("credentials.json parses");
    assert_eq!(stored["credentials"]["scratch:key"]["key"], "two");
    assert!(stored["credentials"]["scratch:key/2"].is_null());

    // Re-storing identical material on an integration clears its failure
    // state instead of appending a duplicate member.
    let pool = aifuel(
        &directory,
        &["auth", "set-key", "openai:api-key", "--key", "same"],
    );
    assert!(pool.status.success(), "{}", stderr(&pool));
    let pool = aifuel(
        &directory,
        &["auth", "set-key", "openai:api-key", "--key", "same"],
    );
    assert!(pool.status.success(), "{}", stderr(&pool));
    assert!(
        stdout(&pool).contains("already stores that key"),
        "{}",
        stdout(&pool)
    );
    let listed = stdout(&aifuel(&directory, &["auth", "list"]));
    assert!(
        listed.contains("managed credential openai:api-key (healthy)"),
        "a single deduplicated member must not read as a pool: {listed}"
    );
}
