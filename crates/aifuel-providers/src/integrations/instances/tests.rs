use super::*;
use aifuel_core::{KeyDelivery, OAuthProfileId};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new() -> Self {
        let suffix = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aifuel-instances-test-{}-{}",
            std::process::id(),
            suffix
        ));
        std::fs::create_dir_all(&path).expect("test dir should be creatable");
        Self { path }
    }

    fn store(&self) -> CredentialStore {
        CredentialStore::new(&self.path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn descriptor(
    id: &str,
    integration: &str,
    env: &[(&str, InstanceEnvSource)],
    credential: Option<&str>,
) -> InstanceDescriptor {
    InstanceDescriptor {
        id: IntegrationId::new(id),
        integration: IntegrationId::new(integration),
        env: env
            .iter()
            .map(|(name, source)| (name.to_string(), source.clone()))
            .collect(),
        credential: credential.map(CredentialRef::new),
    }
}

#[test]
fn instance_entries_decode_env_sources_and_credential() {
    let raw = serde_json::json!({
        "integration": "claude",
        "env": {
            "CLAUDE_CONFIG_DIR": "/home/me/.claude-work",
            "ANTHROPIC_API_KEY": { "credential": "claude-work-key" }
        },
        "credential": "claude-work-key"
    });
    let instance = build_instance("claude.work", raw).expect("valid instance");
    assert_eq!(instance.id.as_str(), "claude.work");
    assert_eq!(instance.integration.as_str(), "claude");
    assert_eq!(
        instance.env["CLAUDE_CONFIG_DIR"],
        InstanceEnvSource::Literal("/home/me/.claude-work".to_owned())
    );
    assert_eq!(
        instance.env["ANTHROPIC_API_KEY"],
        InstanceEnvSource::Credential(CredentialRef::new("claude-work-key"))
    );
    assert_eq!(
        instance.credential,
        Some(CredentialRef::new("claude-work-key"))
    );
}

#[test]
fn instance_entries_reject_bad_shapes_without_material_leaks() {
    // `integration` is the only required field; it must be non-empty.
    for raw in [
        serde_json::json!({}),
        serde_json::json!({ "integration": "" }),
        serde_json::json!({ "integration": "   " }),
    ] {
        assert!(matches!(
            build_instance("claude.work", raw),
            Err(ConfigError::InvalidInstance { .. })
        ));
    }
    // An empty credential reference would name nothing resolvable.
    let raw = serde_json::json!({
        "integration": "claude",
        "env": { "API_KEY": { "credential": " " } }
    });
    let error = build_instance("claude.work", raw).expect_err("empty credential ref");
    let rendered = error.to_string();
    assert!(rendered.contains("claude.work"));

    // Env names must be real variable names: the spawn path feeds them to
    // `Command::envs`, which panics on `=` or NUL.
    let raw = serde_json::json!({
        "integration": "claude",
        "env": { "BAD=NAME": "value" }
    });
    assert!(matches!(
        build_instance("claude.work", raw),
        Err(ConfigError::InvalidInstance { .. })
    ));
}

#[test]
fn resolve_env_returns_literals_and_credential_material() {
    let dir = TestDir::new();
    let store = dir.store();
    store
        .set_api_key(&CredentialRef::new("work-key"), "sk-work-secret")
        .expect("credential stores");

    let instance = descriptor(
        "claude.work",
        "claude",
        &[
            (
                "CLAUDE_CONFIG_DIR",
                InstanceEnvSource::Literal("/cfg".to_owned()),
            ),
            (
                "ANTHROPIC_API_KEY",
                InstanceEnvSource::Credential(CredentialRef::new("work-key")),
            ),
        ],
        None,
    );
    let env = instance.resolve_env(&store).expect("env resolves");
    assert_eq!(env["CLAUDE_CONFIG_DIR"], "/cfg");
    assert_eq!(env["ANTHROPIC_API_KEY"], "sk-work-secret");
}

#[test]
fn resolve_env_fails_before_spawn_on_credential_errors() {
    let dir = TestDir::new();
    let store = dir.store();

    // A missing credential is an explicit failure, not an empty var.
    let instance = descriptor(
        "claude.work",
        "claude",
        &[(
            "ANTHROPIC_API_KEY",
            InstanceEnvSource::Credential(CredentialRef::new("absent")),
        )],
        None,
    );
    assert!(matches!(
        instance.resolve_env(&store),
        Err(CredentialStoreError::CredentialAbsent(_))
    ));

    // A credential bound to another destination refuses to resolve for this
    // instance: a key recorded for `claude.personal` must never land in
    // `claude.work`'s provider process.
    store
        .set_api_key_for(
            &CredentialRef::new("personal-key"),
            "sk-personal",
            &IntegrationId::new("claude.personal"),
        )
        .expect("credential stores");
    let instance = descriptor(
        "claude.work",
        "claude",
        &[(
            "ANTHROPIC_API_KEY",
            InstanceEnvSource::Credential(CredentialRef::new("personal-key")),
        )],
        None,
    );
    assert!(matches!(
        instance.resolve_env(&store),
        Err(CredentialStoreError::CredentialDestinationMismatch { .. })
    ));

    // An OAuth credential cannot fill an env var: wrong kind is an error.
    store
        .set_oauth(&CredentialRef::new("grant"), crate::OAuthTokens::new("a"))
        .expect("grant stores");
    let instance = descriptor(
        "claude.work",
        "claude",
        &[(
            "ANTHROPIC_API_KEY",
            InstanceEnvSource::Credential(CredentialRef::new("grant")),
        )],
        None,
    );
    assert!(matches!(
        instance.resolve_env(&store),
        Err(CredentialStoreError::UnexpectedCredentialKind { .. })
    ));
}

#[test]
fn bound_auth_rebinds_only_the_credential_slot() {
    let instance = descriptor("work-openai.inst", "work-openai", &[], Some("inst-key"));

    // Store -> Store: the delivery survives, only the reference changes.
    let binding = AuthBinding::ApiKey {
        source: ApiKeySource::Store {
            credential: CredentialRef::new("base-key"),
        },
        delivery: KeyDelivery::Header {
            name: "x-api-key".to_owned(),
        },
    };
    assert_eq!(
        instance.bound_auth(&binding).expect("store rebinds"),
        AuthBinding::ApiKey {
            source: ApiKeySource::Store {
                credential: CredentialRef::new("inst-key"),
            },
            delivery: KeyDelivery::Header {
                name: "x-api-key".to_owned(),
            },
        }
    );

    // Env -> EnvOrStore: a literal env var the instance sets still wins, and
    // the bound credential is the fallback.
    let binding = AuthBinding::ApiKey {
        source: ApiKeySource::Env {
            var: "API_KEY".to_owned(),
        },
        delivery: KeyDelivery::Bearer,
    };
    assert_eq!(
        instance.bound_auth(&binding).expect("env rebinds"),
        AuthBinding::ApiKey {
            source: ApiKeySource::EnvOrStore {
                var: "API_KEY".to_owned(),
                credential: CredentialRef::new("inst-key"),
            },
            delivery: KeyDelivery::Bearer,
        }
    );

    // OAuth -> OAuth: the profile survives, the credential ref changes.
    let binding = AuthBinding::OAuth {
        credential: CredentialRef::new("base-grant"),
        profile: OAuthProfileId::new("profile"),
    };
    assert_eq!(
        instance.bound_auth(&binding).expect("oauth rebinds"),
        AuthBinding::OAuth {
            credential: CredentialRef::new("inst-key"),
            profile: OAuthProfileId::new("profile"),
        }
    );

    // `auth: none` has no credential slot: the binding is contradictory.
    assert!(instance.bound_auth(&AuthBinding::None).is_err());

    // No declared `credential` leaves the base binding untouched.
    let unbound = descriptor("work-openai.inst", "work-openai", &[], None);
    assert_eq!(
        unbound.bound_auth(&binding).expect("nothing to rebind"),
        binding
    );
}

#[test]
fn debug_never_prints_literal_values_or_material() {
    let instance = descriptor(
        "claude.work",
        "claude",
        &[
            (
                "CLAUDE_CONFIG_DIR",
                InstanceEnvSource::Literal("sensitive-literal".to_owned()),
            ),
            (
                "ANTHROPIC_API_KEY",
                InstanceEnvSource::Credential(CredentialRef::new("work-key")),
            ),
        ],
        Some("work-key"),
    );
    let rendered = format!("{instance:?}");
    // The literal text is user-owned configuration that may carry secrets;
    // the Credential Reference is safe to name, the value never appears.
    assert!(!rendered.contains("sensitive-literal"), "{rendered}");
    assert!(rendered.contains("work-key"), "{rendered}");
}

#[test]
fn edit_instances_mutates_only_the_instances_map() {
    let dir = TestDir::new();
    let path = dir.path.join("providers.json");
    std::fs::write(
        &path,
        r#"{
            "schema_version": 1,
            "integrations": [{"id": "keep-me"}],
            "extensions": {"pin": true}
        }"#,
    )
    .expect("config writes");

    edit_instances(&path, |instances| {
        instances.insert(
            "claude.work".to_owned(),
            serde_json::json!({"integration": "claude"}),
        );
        Ok(())
    })
    .expect("edit applies");

    let file: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("config reads"))
            .expect("config parses");
    // The sibling keys survive untouched and the lock file stays a sibling.
    assert_eq!(file["integrations"][0]["id"], "keep-me");
    assert_eq!(file["extensions"]["pin"], true);
    assert_eq!(file["instances"]["claude.work"]["integration"], "claude");

    edit_instances(&path, |instances| {
        instances.remove("claude.work");
        Ok(())
    })
    .expect("remove applies");
    let file: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("config reads"))
            .expect("config parses");
    assert!(file["instances"].as_object().unwrap().is_empty());
}

#[test]
fn edit_instances_refuses_a_newer_schema_version() {
    let dir = TestDir::new();
    let path = dir.path.join("providers.json");
    std::fs::write(&path, r#"{"schema_version": 99, "instances": {}}"#).expect("config writes");
    let result = edit_instances(&path, |_| Ok(()));
    assert!(matches!(
        result,
        Err(ConfigError::UnknownSchemaVersion { found: 99 })
    ));
}
