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
        Vec::new(),
        Vec::new(),
        aifuel_core::OptimizePlan::default(),
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
        Vec::new(),
        Vec::new(),
        aifuel_core::OptimizePlan::default(),
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
        Vec::new(),
        Vec::new(),
        aifuel_core::OptimizePlan::default(),
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
        Vec::new(),
        Vec::new(),
        aifuel_core::OptimizePlan::default(),
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

    let result = IntegrationRegistry::build(
        vec![builtin],
        vec![smuggled],
        Vec::new(),
        Vec::new(),
        aifuel_core::OptimizePlan::default(),
    );

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
        Vec::new(),
        Vec::new(),
        aifuel_core::OptimizePlan::default(),
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
        Vec::new(),
        Vec::new(),
        aifuel_core::OptimizePlan::default(),
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
        Vec::new(),
        Vec::new(),
        aifuel_core::OptimizePlan::default(),
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
    let registry = IntegrationRegistry::build(
        builtin_integrations(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        aifuel_core::OptimizePlan::default(),
    )
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

fn instance(id: &str, base: &str) -> InstanceDescriptor {
    InstanceDescriptor {
        id: IntegrationId::new(id),
        integration: IntegrationId::new(base),
        env: Default::default(),
        credential: None,
    }
}

#[test]
fn instances_join_the_registry_and_serve_their_base() {
    let registry = IntegrationRegistry::build(
        vec![cli_descriptor("claude", "claude")],
        Vec::new(),
        vec![Ok(instance("claude.work", "claude"))],
        Vec::new(),
        aifuel_core::OptimizePlan::default(),
    )
    .expect("build succeeds");

    // The instance resolves by exact id to its base descriptor plus the
    // overlay; the base id still serves itself.
    let (base, overlay) = registry
        .serving(&IntegrationId::new("claude.work"))
        .expect("instance serves");
    assert_eq!(base.id().as_str(), "claude");
    assert_eq!(overlay.expect("overlay").id.as_str(), "claude.work");
    let (base, overlay) = registry
        .serving(&IntegrationId::new("claude"))
        .expect("base serves");
    assert_eq!(base.id().as_str(), "claude");
    assert!(overlay.is_none());
    assert!(registry.serving(&IntegrationId::new("nobody")).is_none());

    // `resolve` never returns an instance: a bare provider selector keeps
    // meaning the base integration so a stored `claude` selection cannot
    // silently start running `claude.work`'s environment.
    assert_eq!(
        registry
            .resolve("claude")
            .expect("base resolves")
            .id()
            .as_str(),
        "claude"
    );
    assert_eq!(
        registry.resolve("claude.work").expect_err("not a selector"),
        ResolveError::Unknown {
            selector: "claude.work".to_owned()
        }
    );
}

#[test]
fn instance_ids_cannot_collide_with_claimed_identities() {
    // An instance reusing a base integration id would shadow it.
    let result = IntegrationRegistry::build(
        vec![cli_descriptor("claude", "claude")],
        Vec::new(),
        vec![Ok(instance("claude", "claude"))],
        Vec::new(),
        aifuel_core::OptimizePlan::default(),
    );
    assert!(matches!(
        result,
        Err(RegistryError::DuplicateInstanceId(ref id)) if id.as_str() == "claude"
    ));

    // Two instances cannot share an id.
    let result = IntegrationRegistry::build(
        vec![cli_descriptor("claude", "claude")],
        Vec::new(),
        vec![
            Ok(instance("claude.work", "claude")),
            Ok(instance("claude.work", "claude")),
        ],
        Vec::new(),
        aifuel_core::OptimizePlan::default(),
    );
    assert!(matches!(
        result,
        Err(RegistryError::DuplicateInstanceId(ref id)) if id.as_str() == "claude.work"
    ));
}

#[test]
fn an_instance_must_name_a_registered_base() {
    let result = IntegrationRegistry::build(
        vec![cli_descriptor("claude", "claude")],
        Vec::new(),
        vec![Ok(instance("ghost.work", "ghost"))],
        Vec::new(),
        aifuel_core::OptimizePlan::default(),
    );
    assert!(matches!(
        result,
        Err(RegistryError::InvalidInstance { ref id, .. }) if id.as_str() == "ghost.work"
    ));
}

#[test]
fn an_instance_cannot_reference_a_builtin_credential() {
    // The same reservation config integrations obey: a built-in-held
    // Credential Reference is bound to that built-in's endpoint, so an
    // instance must never smuggle it into another process environment.
    let mut builtin = cli_descriptor("claude", "claude");
    builtin.integration.execution = ExecutionConfig::Http {
        endpoint: EndpointConfig {
            base_url: "https://api.anthropic.example".to_owned(),
            extra_headers: Default::default(),
            request_timeout_seconds: None,
        },
        protocol: WireApi::AnthropicMessages,
        auth: AuthBinding::ApiKey {
            source: ApiKeySource::Store {
                credential: aifuel_core::CredentialRef::new("builtin-held"),
            },
            delivery: aifuel_core::KeyDelivery::Bearer,
        },
    };
    let mut smuggler = instance("claude.work", "claude");
    smuggler.env.insert(
        "ANTHROPIC_API_KEY".to_owned(),
        crate::integrations::InstanceEnvSource::Credential(aifuel_core::CredentialRef::new(
            "builtin-held",
        )),
    );
    let result = IntegrationRegistry::build(
        vec![builtin],
        Vec::new(),
        vec![Ok(smuggler)],
        Vec::new(),
        aifuel_core::OptimizePlan::default(),
    );
    assert!(matches!(
        result,
        Err(RegistryError::ReservedCredential { .. })
    ));
}

#[test]
fn an_instance_credential_needs_a_slot_to_fill() {
    // `auth: none` declares no credential slot, so binding one is a
    // configuration error - the instance would otherwise claim a binding
    // nothing applies.
    let mut bound = instance("open.inst", "open");
    bound.credential = Some(aifuel_core::CredentialRef::new("inst-key"));
    let result = IntegrationRegistry::build(
        Vec::new(),
        vec![configured_http("open", "open", AuthBinding::None)],
        vec![Ok(bound.clone())],
        Vec::new(),
        aifuel_core::OptimizePlan::default(),
    );
    assert!(matches!(
        result,
        Err(RegistryError::InvalidInstance { ref id, .. }) if id.as_str() == "open.inst"
    ));

    // Over an api-key binding the same instance is valid.
    let result = IntegrationRegistry::build(
        Vec::new(),
        vec![configured_http(
            "open",
            "open",
            AuthBinding::ApiKey {
                source: ApiKeySource::Env {
                    var: "OPEN_KEY".to_owned(),
                },
                delivery: aifuel_core::KeyDelivery::Bearer,
            },
        )],
        vec![Ok(bound)],
        Vec::new(),
        aifuel_core::OptimizePlan::default(),
    );
    assert!(result.is_ok(), "{result:?}");
}

#[test]
fn an_instance_id_must_not_shadow_an_unrelated_selector() {
    // `claude` as an instance id over a different base would steal every
    // bare `claude` selection - the exact-id match would win over the
    // provider-name mapping users already type.
    let result = IntegrationRegistry::build(
        vec![
            cli_descriptor("claude:cli", "claude"),
            cli_descriptor("codex:cli", "codex"),
        ],
        Vec::new(),
        vec![Ok(instance("claude", "codex:cli"))],
        Vec::new(),
        aifuel_core::OptimizePlan::default(),
    );
    assert!(matches!(
        result,
        Err(RegistryError::InvalidInstance { ref id, .. }) if id.as_str() == "claude"
    ));

    // The same id over its own provider's integration is consistent: the
    // selector and the instance lead to the same serving integration.
    let registry = IntegrationRegistry::build(
        vec![cli_descriptor("claude:cli", "claude")],
        Vec::new(),
        vec![Ok(instance("claude", "claude:cli"))],
        Vec::new(),
        aifuel_core::OptimizePlan::default(),
    )
    .expect("a provider-name instance over that provider's integration is consistent");
    let (base, overlay) = registry
        .serving(&IntegrationId::new("claude"))
        .expect("serves");
    assert_eq!(base.id().as_str(), "claude:cli");
    assert_eq!(overlay.expect("overlay").id.as_str(), "claude");
}

#[test]
fn check_instance_guards_adds_after_build() {
    // `aifuel instance add` validates through the same rules `build`
    // applies, so the file never gains an entry the registry would reject.
    let registry = IntegrationRegistry::build(
        vec![cli_descriptor("claude", "claude")],
        Vec::new(),
        vec![Ok(instance("claude.work", "claude"))],
        Vec::new(),
        aifuel_core::OptimizePlan::default(),
    )
    .expect("build succeeds");
    assert!(
        registry
            .check_instance(&instance("claude.personal", "claude"))
            .is_ok()
    );
    assert!(matches!(
        registry.check_instance(&instance("claude.work", "claude")),
        Err(RegistryError::DuplicateInstanceId(_))
    ));
    assert!(matches!(
        registry.check_instance(&instance("ghost.inst", "ghost")),
        Err(RegistryError::InvalidInstance { .. })
    ));
}

fn chain(name: &str, steps: &[(&str, Option<&str>)]) -> ChainDescriptor {
    ChainDescriptor {
        name: name.to_owned(),
        strategy: crate::integrations::ChainStrategy::Priority,
        steps: steps
            .iter()
            .map(|(integration, model)| crate::integrations::ChainStep {
                integration: IntegrationId::new(*integration),
                model: model.map(str::to_owned),
            })
            .collect(),
    }
}

#[test]
fn chains_join_the_registry_and_steps_resolve_through_serving() {
    // A chain step may name a base integration or an instance selector;
    // the instance step reports its base descriptor for routing.
    let registry = IntegrationRegistry::build(
        vec![cli_descriptor("claude", "claude")],
        Vec::new(),
        vec![Ok(instance("claude.work", "claude"))],
        vec![Ok(chain(
            "main",
            &[("claude", None), ("claude.work", Some("opus"))],
        ))],
        aifuel_core::OptimizePlan::default(),
    )
    .expect("a chain over registered identities builds");

    let chain = registry.chain("main").expect("the chain registers");
    assert_eq!(chain.steps.len(), 2);
    assert_eq!(chain.steps[1].model.as_deref(), Some("opus"));
    assert!(registry.chain("ghost").is_none());
    assert_eq!(registry.chains().count(), 1);
    let (base, overlay) = registry
        .serving(&chain.steps[1].integration)
        .expect("the instance step serves");
    assert_eq!(base.id().as_str(), "claude");
    assert!(overlay.is_some());
}

#[test]
fn a_chain_step_must_name_a_registered_identity() {
    // A step pointing nowhere is a load-time error, not a runtime skip:
    // a typo in the file must surface when the config is read.
    let result = IntegrationRegistry::build(
        vec![cli_descriptor("claude", "claude")],
        Vec::new(),
        Vec::new(),
        vec![Ok(chain("main", &[("ghost", None)]))],
        aifuel_core::OptimizePlan::default(),
    );
    assert!(matches!(
        result,
        Err(RegistryError::InvalidChain { ref name, .. }) if name == "main"
    ));
}

#[test]
fn the_registry_reports_the_file_optimizer_plan() {
    let mut plan = aifuel_core::OptimizePlan::default();
    plan.rtk = aifuel_core::RtkLevel::Ultra;
    let registry = IntegrationRegistry::build(
        vec![cli_descriptor("claude", "claude")],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        plan,
    )
    .expect("build succeeds");
    assert_eq!(registry.optimizer().rtk, aifuel_core::RtkLevel::Ultra);
}
