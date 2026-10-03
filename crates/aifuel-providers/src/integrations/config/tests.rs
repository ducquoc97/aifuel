use super::*;
use crate::EvidenceSource;
use crate::integrations::IntegrationOrigin;
use aifuel_core::{ApiKeySource, AuthBinding, ExecutionConfig, WireApi};
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
            "aifuel-providers-config-test-{}-{}",
            std::process::id(),
            suffix
        ));
        fs::create_dir_all(&path).expect("test dir should be creatable");
        Self { path }
    }

    fn write(&self, contents: &str) -> PathBuf {
        let path = self.path.join("providers.json");
        fs::write(&path, contents).expect("config should be writable");
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn invalid_reason(result: &Result<IntegrationDescriptor, ConfigError>) -> &str {
    match result {
        Err(ConfigError::InvalidIntegration { reason, .. }) => reason,
        other => panic!("expected InvalidIntegration, got {other:?}"),
    }
}

#[test]
fn an_absent_file_is_an_empty_config_not_an_error() {
    let dir = TestDir::new();
    let config = ProvidersConfig::load(dir.path.join("providers.json"))
        .expect("missing file should load as empty");
    assert!(config.entries().is_empty());
}

#[test]
fn a_malformed_file_is_an_error_never_silently_empty() {
    let dir = TestDir::new();
    let path = dir.write("{ not json");
    let result = ProvidersConfig::load(path);
    assert!(matches!(result, Err(ConfigError::InvalidFile { .. })));

    let path = dir.write(r#"{"schema_version": 1, "integrations": "nope"}"#);
    let result = ProvidersConfig::load(path);
    assert!(matches!(result, Err(ConfigError::InvalidFile { .. })));
}

#[test]
fn an_unknown_schema_version_is_rejected() {
    let dir = TestDir::new();
    let path = dir.write(r#"{"schema_version": 99, "integrations": []}"#);
    assert!(matches!(
        ProvidersConfig::load(path),
        Err(ConfigError::UnknownSchemaVersion { found: 99 })
    ));
}

#[test]
fn a_valid_file_produces_descriptors() {
    let dir = TestDir::new();
    let path = dir.write(
        r#"{
                "schema_version": 1,
                "integrations": [
                    {
                        "id": "work-openai",
                        "provider_id": "openai",
                        "endpoint": { "base_url": "https://api.openai.com/v1" },
                        "wire_api": "openai-chat",
                        "auth": { "kind": "api-key-env", "var": "WORK_OPENAI_KEY" }
                    },
                    {
                        "id": "local-proxy",
                        "provider_id": "litellm",
                        "name": "LiteLLM proxy",
                        "endpoint": {
                            "base_url": "http://localhost:4000/v1",
                            "headers": { "x-tenant": "a" },
                            "request_timeout_seconds": 30
                        },
                        "wire_api": "openai-chat",
                        "auth": { "kind": "none" }
                    }
                ]
            }"#,
    );
    let config = ProvidersConfig::load(path).expect("valid file should load");
    let entries = config.entries();
    assert_eq!(entries.len(), 2);

    let first = entries[0].as_ref().expect("first entry should be valid");
    assert_eq!(first.integration.id.as_str(), "work-openai");
    assert_eq!(first.integration.provider.as_str(), "openai");
    assert_eq!(first.integration.name, "work-openai");
    assert_eq!(first.origin, IntegrationOrigin::Configured);
    match &first.integration.execution {
        ExecutionConfig::Http {
            endpoint,
            protocol,
            auth,
        } => {
            assert_eq!(endpoint.base_url, "https://api.openai.com/v1");
            assert_eq!(*protocol, WireApi::OpenAiChat);
            assert_eq!(
                *auth,
                AuthBinding::ApiKey {
                    source: ApiKeySource::Env {
                        var: "WORK_OPENAI_KEY".to_owned()
                    },
                    delivery: KeyDelivery::Bearer,
                }
            );
        }
        other => panic!("expected Http execution, got {other:?}"),
    }
    // Evidence: the config entry itself plus the declared env var.
    assert_eq!(
        first.sources,
        vec![
            EvidenceSource::ConfiguredEndpoint {
                marker_directories: Vec::new()
            },
            EvidenceSource::EnvVar("WORK_OPENAI_KEY".to_owned()),
        ]
    );

    let second = entries[1].as_ref().expect("second entry should be valid");
    assert_eq!(second.integration.name, "LiteLLM proxy");
    assert_eq!(second.origin, IntegrationOrigin::Configured);
}

#[test]
fn per_entry_failures_do_not_lose_sibling_entries() {
    let dir = TestDir::new();
    let path = dir.write(
        r#"{
                "schema_version": 1,
                "integrations": [
                    {
                        "id": "good",
                        "provider_id": "openai",
                        "endpoint": { "base_url": "https://api.openai.com/v1" },
                        "wire_api": "openai-chat",
                        "auth": { "kind": "none" }
                    },
                    {
                        "id": "bad",
                        "provider_id": "openai",
                        "endpoint": { "base_url": "ftp://nope" },
                        "wire_api": "openai-chat",
                        "auth": { "kind": "none" }
                    }
                ]
            }"#,
    );
    let config = ProvidersConfig::load(path).expect("file itself is well-formed");
    assert!(config.entries()[0].is_ok());
    let error = config.entries()[1].as_ref().expect_err("entry must fail");
    assert!(matches!(
        error,
        ConfigError::InvalidIntegration { index: 1, .. }
    ));
}

#[test]
fn validation_rejects_bad_inputs_at_the_load_boundary() {
    let cases = [
        // id / provider_id
        (
            r#"{"id": "", "provider_id": "p", "endpoint": {"base_url": "http://x"}, "wire_api": "openai-chat", "auth": {"kind": "none"}}"#,
            "id must not be empty",
        ),
        (
            r#"{"id": "i", "provider_id": "  ", "endpoint": {"base_url": "http://x"}, "wire_api": "openai-chat", "auth": {"kind": "none"}}"#,
            "provider_id",
        ),
        // base_url must be http(s)
        (
            r#"{"id": "i", "provider_id": "p", "endpoint": {"base_url": "not a url"}, "wire_api": "openai-chat", "auth": {"kind": "none"}}"#,
            "not a URL",
        ),
        (
            r#"{"id": "i", "provider_id": "p", "endpoint": {"base_url": "ftp://x"}, "wire_api": "openai-chat", "auth": {"kind": "none"}}"#,
            "http or https",
        ),
        // unknown wire_api; a native ollama-chat is intentionally not compiled
        (
            r#"{"id": "i", "provider_id": "p", "endpoint": {"base_url": "http://x"}, "wire_api": "ollama-chat", "auth": {"kind": "none"}}"#,
            "wire_api 'ollama-chat' is unknown",
        ),
        // auth kind consistency
        (
            r#"{"id": "i", "provider_id": "p", "endpoint": {"base_url": "http://x"}, "wire_api": "openai-chat", "auth": {"kind": "bearer"}}"#,
            "auth kind 'bearer' is unknown",
        ),
        (
            r#"{"id": "i", "provider_id": "p", "endpoint": {"base_url": "http://x"}, "wire_api": "openai-chat", "auth": {"kind": "api-key-env"}}"#,
            "requires a non-empty var",
        ),
        (
            r#"{"id": "i", "provider_id": "p", "endpoint": {"base_url": "http://x"}, "wire_api": "openai-chat", "auth": {"kind": "api-key-ref"}}"#,
            "requires a non-empty credential",
        ),
        (
            r#"{"id": "i", "provider_id": "p", "endpoint": {"base_url": "http://x"}, "wire_api": "openai-chat", "auth": {"kind": "oauth-ref", "credential": "c"}}"#,
            "requires a non-empty profile",
        ),
        (
            r#"{"id": "i", "provider_id": "p", "endpoint": {"base_url": "http://x"}, "wire_api": "openai-chat", "auth": {"kind": "none", "credential": "c"}}"#,
            "accepts no var",
        ),
        (
            r#"{"id": "i", "provider_id": "p", "endpoint": {"base_url": "http://x"}, "wire_api": "openai-chat", "auth": {"kind": "api-key-env", "var": "V", "credential": "c"}}"#,
            "accepts only var",
        ),
    ];
    for (entry, expected) in cases {
        let dir = TestDir::new();
        let path = dir.write(&format!(
            r#"{{"schema_version": 1, "integrations": [{entry}]}}"#
        ));
        let config = ProvidersConfig::load(path).expect("file is well-formed");
        let reason = invalid_reason(&config.entries()[0]);
        assert!(
            reason.contains(expected),
            "expected reason containing {expected:?}, got {reason:?}"
        );
    }
}

#[test]
fn auto_is_reserved_for_routing_not_a_configured_identity() {
    // `--provider auto` is the routing alias; a configured integration or
    // provider_id named `auto` would shadow it, so both are rejected.
    let dir = TestDir::new();
    for entry in [
        r#"{"id": "auto", "provider_id": "p", "endpoint": {"base_url": "http://x"}, "wire_api": "openai-chat", "auth": {"kind": "none"}}"#,
        r#"{"id": "i", "provider_id": "auto", "endpoint": {"base_url": "http://x"}, "wire_api": "openai-chat", "auth": {"kind": "none"}}"#,
    ] {
        let path = dir.write(&format!(
            r#"{{"schema_version": 1, "integrations": [{entry}]}}"#
        ));
        let config = ProvidersConfig::load(path).expect("file is well-formed");
        let reason = invalid_reason(&config.entries()[0]);
        assert!(
            reason.contains("reserved for automatic Provider routing"),
            "got {reason:?}"
        );
    }
}

#[test]
fn custom_headers_cannot_override_managed_auth_headers() {
    // Authorization is what a bearer delivery applies; letting config
    // set it too would let a header smuggle a different credential than
    // the managed binding.
    let dir = TestDir::new();
    let path = dir.write(
        r#"{
                "schema_version": 1,
                "integrations": [{
                    "id": "i",
                    "provider_id": "p",
                    "endpoint": {
                        "base_url": "http://x",
                        "headers": { "Authorization": "Bearer smuggled" }
                    },
                    "wire_api": "openai-chat",
                    "auth": { "kind": "api-key-env", "var": "V" }
                }]
            }"#,
    );
    let config = ProvidersConfig::load(path).expect("file is well-formed");
    let reason = invalid_reason(&config.entries()[0]);
    assert!(reason.contains("Authorization"), "got {reason:?}");

    // A non-colliding custom header is fine.
    let path = dir.write(
            r#"{
                "schema_version": 1,
                "integrations": [{
                    "id": "i",
                    "provider_id": "p",
                    "endpoint": {
                        "base_url": "http://x",
                        "headers": { "x-tenant": "ok" }
                    },
                    "wire_api": "openai-chat",
                    "auth": { "kind": "api-key-env", "var": "V", "delivery": {"header": {"name": "x-api-key"}} }
                }]
            }"#,
        );
    let config = ProvidersConfig::load(path).expect("file is well-formed");
    let entry = config.entries()[0].as_ref().expect("should be valid");
    match &entry.integration.execution {
        ExecutionConfig::Http { auth, .. } => assert_eq!(
            *auth,
            AuthBinding::ApiKey {
                source: ApiKeySource::Env {
                    var: "V".to_owned()
                },
                delivery: KeyDelivery::Header {
                    name: "x-api-key".to_owned()
                },
            }
        ),
        _ => unreachable!(),
    }
}

#[test]
fn instance_entries_decode_alongside_integrations() {
    let dir = TestDir::new();
    let path = dir.write(
        r#"{
            "schema_version": 1,
            "integrations": [],
            "instances": {
                "claude.work": {
                    "integration": "claude",
                    "env": {
                        "CLAUDE_CONFIG_DIR": "/cfg",
                        "ANTHROPIC_API_KEY": { "credential": "work-key" }
                    },
                    "credential": "work-key"
                },
                "claude.personal": { "integration": "claude" }
            }
        }"#,
    );
    let config = ProvidersConfig::load(path).expect("file is well-formed");
    assert!(config.entries().is_empty());
    let instances = config.instances();
    assert_eq!(instances.len(), 2);
    // `instances` stores the map as a `BTreeMap`, so entries surface in
    // sorted id order: personal before work.
    let personal = instances[0].as_ref().expect("personal instance is valid");
    assert_eq!(personal.id.as_str(), "claude.personal");
    assert!(personal.env.is_empty());
    assert!(personal.credential.is_none());
    let work = instances[1].as_ref().expect("work instance is valid");
    assert_eq!(work.id.as_str(), "claude.work");
    assert_eq!(
        work.env["ANTHROPIC_API_KEY"],
        crate::integrations::InstanceEnvSource::Credential(aifuel_core::CredentialRef::new(
            "work-key"
        ))
    );
}

#[test]
fn one_bad_instance_reports_its_own_error() {
    // A malformed instance entry reports `InvalidInstance` keyed by its map
    // id while a sibling entry still decodes - the same per-entry isolation
    // `integrations` entries get.
    let dir = TestDir::new();
    let path = dir.write(
        r#"{
            "schema_version": 1,
            "instances": {
                "good": { "integration": "claude" },
                "bad": { "env": { "X": "y" } }
            }
        }"#,
    );
    let config = ProvidersConfig::load(path).expect("file is well-formed");
    let instances = config.instances();
    assert_eq!(instances.len(), 2);
    assert!(matches!(
        instances[0],
        Err(ConfigError::InvalidInstance { ref id, .. }) if id == "bad"
    ));
    assert!(instances[1].is_ok());
}

#[test]
fn chains_and_optimizer_decode_alongside_integrations() {
    // The full documented providers.json shape: named chains plus the
    // file-level optimizer plan, both spellings of a level.
    let dir = TestDir::new();
    let path = dir.write(
        r#"{
            "schema_version": 1,
            "integrations": [],
            "chains": {
                "main": {
                    "strategy": "priority",
                    "steps": [
                        { "integration": "claude" },
                        { "integration": "glm:api-key", "model": "glm-4.7" }
                    ]
                }
            },
            "optimizer": {
                "stack": ["rtk", "caveman"],
                "rtk": "standard",
                "caveman": { "level": "full" }
            }
        }"#,
    );
    let config = ProvidersConfig::load(path).expect("file is well-formed");
    let chains = config.chains();
    assert_eq!(chains.len(), 1);
    let main = chains[0].as_ref().expect("main chain is valid");
    assert_eq!(main.name, "main");
    assert_eq!(main.steps.len(), 2);
    assert_eq!(main.steps[1].model.as_deref(), Some("glm-4.7"));
    assert_eq!(config.optimizer().rtk, aifuel_core::RtkLevel::Standard);
    assert_eq!(config.optimizer().caveman, aifuel_core::CavemanLevel::Full);
}

#[test]
fn one_bad_chain_reports_its_own_error() {
    // Same per-entry isolation as instances: a malformed chain reports
    // `InvalidChain` keyed by its map name while siblings still decode.
    let dir = TestDir::new();
    let path = dir.write(
        r#"{
            "schema_version": 1,
            "chains": {
                "good": { "strategy": "priority", "steps": [{ "integration": "claude" }] },
                "bad": { "strategy": "weighted", "steps": [{ "integration": "claude" }] }
            }
        }"#,
    );
    let config = ProvidersConfig::load(path).expect("file is well-formed");
    let chains = config.chains();
    assert_eq!(chains.len(), 2);
    assert!(matches!(
        chains[0],
        Err(ConfigError::InvalidChain { ref name, .. }) if name == "bad"
    ));
    assert!(chains[1].is_ok());
}

#[test]
fn a_malformed_optimizer_is_a_file_error_not_a_silent_default() {
    // Optimization changes model-visible bytes, so a misspelled level can
    // never fall back to "off" - the operator must fix the file.
    let dir = TestDir::new();
    let path = dir.write(
        r#"{
            "schema_version": 1,
            "optimizer": { "rtk": "supersonic" }
        }"#,
    );
    assert!(matches!(
        ProvidersConfig::load(path),
        Err(ConfigError::InvalidFile { .. })
    ));
}

#[test]
fn an_absent_optimizer_is_the_inert_plan() {
    let dir = TestDir::new();
    let path = dir.write(r#"{ "schema_version": 1 }"#);
    let config = ProvidersConfig::load(path).expect("file is well-formed");
    assert!(!config.optimizer().is_active());
}
