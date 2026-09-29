use super::*;
use crate::integrations::builtin::builtin_integrations;
use aifuel_core::{ApiKeySource, AuthBinding, EndpointConfig, WireApi};

fn cli_descriptor(id: &str, provider: &str) -> IntegrationDescriptor {
    IntegrationDescriptor::builtin(
        Integration {
            id: IntegrationId::new(id),
            provider: ProviderId::new(provider),
            name: id.to_owned(),
            execution: ExecutionConfig::Cli {
                adapter: aifuel_core::CliAdapterId::new(provider),
            },
            monitoring: None,
        },
        Vec::new(),
    )
}

fn configured_http(
    id: &str,
    provider: &str,
    auth: AuthBinding,
) -> Result<IntegrationDescriptor, ConfigError> {
    Ok(IntegrationDescriptor::configured(
        Integration {
            id: IntegrationId::new(id),
            provider: ProviderId::new(provider),
            name: id.to_owned(),
            execution: ExecutionConfig::Http {
                endpoint: EndpointConfig {
                    base_url: "https://example.test/v1".to_owned(),
                    extra_headers: Default::default(),
                    request_timeout_seconds: None,
                },
                protocol: WireApi::OpenAiChat,
                auth,
            },
            monitoring: None,
        },
        Vec::new(),
    ))
}

#[test]
fn duplicate_builtin_ids_are_rejected() {
    let result = IntegrationRegistry::build(
        vec![
            cli_descriptor("claude:cli", "claude"),
            cli_descriptor("claude:cli", "claude"),
        ],
        Vec::new(),
    );
    assert!(matches!(
        result,
        Err(RegistryError::DuplicateIntegrationId(ref id)) if id.as_str() == "claude:cli"
    ));
}

#[test]
fn config_cannot_shadow_a_builtin_id() {
    // A reserved built-in id must never silently resolve to a config
    // definition - which definition wins would be ambiguous.
    let result = IntegrationRegistry::build(
        vec![cli_descriptor("claude:cli", "claude")],
        vec![configured_http("claude:cli", "claude", AuthBinding::None)],
    );
    assert!(matches!(
        result,
        Err(RegistryError::ReservedIntegrationId(ref id)) if id.as_str() == "claude:cli"
    ));
}

#[test]
fn duplicate_config_ids_are_rejected() {
    let result = IntegrationRegistry::build(
        Vec::new(),
        vec![
            configured_http("mine", "openai", AuthBinding::None),
            configured_http("mine", "openai", AuthBinding::None),
        ],
    );
    assert!(matches!(
        result,
        Err(RegistryError::DuplicateIntegrationId(ref id)) if id.as_str() == "mine"
    ));
}

#[test]
fn a_config_entry_error_fails_the_build() {
    let result = IntegrationRegistry::build(
        vec![cli_descriptor("claude:cli", "claude")],
        vec![Err(ConfigError::InvalidIntegration {
            index: 0,
            id: Some("broken".to_owned()),
            reason: "auth kind 'bogus' is unknown".to_owned(),
        })],
    );
    assert!(matches!(result, Err(RegistryError::Config(_))));
}

#[test]
fn config_cannot_reference_a_builtin_held_credential() {
    // The trust boundary: a config endpoint pointing at attacker.example
    // must not attach a managed credential a built-in holds, because the
    // binding would send that credential to a different host.
    let builtin = IntegrationDescriptor::builtin(
        Integration {
            id: IntegrationId::new("copilot:oauth"),
            provider: ProviderId::new("copilot"),
            name: "Copilot OAuth".to_owned(),
            execution: ExecutionConfig::Http {
                endpoint: EndpointConfig {
                    base_url: "https://api.githubcopilot.com".to_owned(),
                    extra_headers: Default::default(),
                    request_timeout_seconds: None,
                },
                protocol: WireApi::OpenAiChat,
                auth: AuthBinding::OAuth {
                    credential: CredentialRef::new("copilot-oauth"),
                    profile: aifuel_core::OAuthProfileId::new("copilot-device"),
                },
            },
            monitoring: None,
        },
        Vec::new(),
    );
    let smuggled = configured_http(
        "evil",
        "openai",
        AuthBinding::OAuth {
            credential: CredentialRef::new("copilot-oauth"),
            profile: aifuel_core::OAuthProfileId::new("copilot-device"),
        },
    );

    let result = IntegrationRegistry::build(vec![builtin], vec![smuggled]);

    assert!(matches!(
        result,
        Err(RegistryError::ReservedCredential {
            ref integration,
            ref credential,
        }) if integration.as_str() == "evil" && credential.as_str() == "copilot-oauth"
    ));
}

#[test]
fn resolve_prefers_an_exact_integration_id() {
    let registry = IntegrationRegistry::build(
        vec![
            cli_descriptor("claude:cli", "claude"),
            // A second integration whose provider is itself spelled like
            // the first integration's id: the exact id still wins.
            cli_descriptor("mirror", "claude:cli"),
        ],
        Vec::new(),
    )
    .expect("build should succeed");

    let resolved = registry.resolve("claude:cli").expect("exact id resolves");
    assert_eq!(resolved.id().as_str(), "claude:cli");
    assert_eq!(resolved.provider().as_str(), "claude");
}

#[test]
fn a_bare_provider_resolves_only_when_unambiguous() {
    let registry = IntegrationRegistry::build(
        vec![
            cli_descriptor("claude:cli", "claude"),
            cli_descriptor("codex:cli", "codex"),
        ],
        vec![configured_http(
            "work-codex",
            "codex",
            AuthBinding::ApiKey {
                source: ApiKeySource::Env {
                    var: "OPENAI_API_KEY".to_owned(),
                },
                delivery: aifuel_core::KeyDelivery::Bearer,
            },
        )],
    )
    .expect("build should succeed");

    // Unique provider -> resolves.
    assert_eq!(
        registry
            .resolve("claude")
            .expect("unique provider")
            .id()
            .as_str(),
        "claude:cli"
    );

    // Ambiguous provider -> error naming every candidate, never a guess.
    let error = registry.resolve("codex").expect_err("ambiguous provider");
    assert_eq!(
        error,
        ResolveError::Ambiguous {
            provider: ProviderId::new("codex"),
            candidates: vec![
                IntegrationId::new("codex:cli"),
                IntegrationId::new("work-codex"),
            ],
        }
    );

    // Nothing matches -> unknown.
    assert_eq!(
        registry
            .resolve("nonexistent")
            .expect_err("unknown selector"),
        ResolveError::Unknown {
            selector: "nonexistent".to_owned()
        }
    );
}

#[test]
fn list_preserves_builtin_catalog_order_then_config_file_order() {
    // BTreeMap would sort ids alphabetically; the spec's deterministic
    // order is catalog order then file order, so `list` must not sort.
    let registry = IntegrationRegistry::build(
        vec![
            cli_descriptor("zeta:cli", "zeta"),
            cli_descriptor("alpha:cli", "alpha"),
        ],
        vec![
            configured_http("omega", "omega", AuthBinding::None),
            configured_http("beta", "beta", AuthBinding::None),
        ],
    )
    .expect("build should succeed");

    let order: Vec<&str> = registry.list().map(|d| d.id().as_str()).collect();
    assert_eq!(order, ["zeta:cli", "alpha:cli", "omega", "beta"]);
    assert_eq!(registry.configured_ids().len(), 2);
    assert_eq!(registry.len(), 4);
    assert!(!registry.is_empty());
    assert_eq!(
        registry.get(&IntegrationId::new("omega")).unwrap().origin,
        IntegrationOrigin::Configured
    );
}

#[test]
fn the_real_builtins_build_and_resolve() {
    let registry = IntegrationRegistry::build(builtin_integrations(), Vec::new())
        .expect("builtins never collide");

    // CLI integration ids equal the catalog provider ids, so stored
    // legacy selections resolve by exact match.
    assert_eq!(
        registry.resolve("claude").expect("claude").id().as_str(),
        "claude"
    );
    assert_eq!(
        registry.resolve("ollama").expect("ollama").id().as_str(),
        "ollama:local"
    );
    assert_eq!(
        registry.resolve("openai").expect("openai").id().as_str(),
        "openai:api-key"
    );
}
