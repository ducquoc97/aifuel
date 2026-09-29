//! The built-in Provider Integrations.
//!
//! Six CLI integrations wrap the compiled CLI adapters (`ExecutionConfig::Cli`
//! - the provider CLI owns its credential, so no auth binding exists). Their
//! evidence sources are the same provider-owned credential markers the
//! catalog registry inspects; the marker paths are repeated here because the
//! catalog definitions do not expose them.
//!
//! The HTTP builtins cover the spec's P1 matrix: local OpenAI-compatible
//! servers (`ollama:local`, `lmstudio:local`) with explicit `AuthBinding::None`,
//! and billed API-key integrations for OpenAI and OpenRouter. An API-key
//! builtin binds `EnvOrStore`: the conventional environment variable works
//! as-is, and `aifuel auth set-key <integration>` stores a managed credential
//! that then takes precedence. No key material ever appears here - only the
//! variable name and the managed Credential Reference are declared.

use super::evidence::EvidenceSource;
use super::registry::IntegrationDescriptor;
use aifuel_core::{
    ApiKeySource, AuthBinding, CliAdapterId, CollectorId, CredentialRef, EndpointConfig,
    ExecutionConfig, Integration, IntegrationId, KeyDelivery, MonitoringConfig, ProviderId,
    ProviderKey, WireApi,
};
use std::collections::BTreeMap;

const OLLAMA_BASE_URL: &str = "http://localhost:11434/v1";
const LMSTUDIO_BASE_URL: &str = "http://localhost:1234/v1";
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// The built-in integration descriptors: the six CLI integrations in pinned
/// catalog order, then the local-server and API-key HTTP integrations.
///
/// CLI integration ids equal the catalog provider ids (`claude`, `codex`,
/// ...) so they match what the compiled CLI adapters report and what stored
/// legacy selections already name. HTTP builtins carry a distinguishing
/// suffix (`ollama:local`, `openai:api-key`) because their provider ids may
/// later gain sibling integrations.
pub fn builtin_integrations() -> Vec<IntegrationDescriptor> {
    vec![
        cli(
            ProviderKey::Claude,
            vec![EvidenceSource::File(".claude/.credentials.json".to_owned())],
        ),
        cli(
            ProviderKey::Codex,
            vec![EvidenceSource::File(".codex/auth.json".to_owned())],
        ),
        cli(
            ProviderKey::Copilot,
            vec![EvidenceSource::File(".copilot/config.json".to_owned())],
        ),
        cli(
            ProviderKey::Gemini,
            vec![EvidenceSource::File(".gemini/oauth_creds.json".to_owned())],
        ),
        cli(
            ProviderKey::Antigravity,
            vec![
                EvidenceSource::Directory(".gemini/antigravity".to_owned()),
                EvidenceSource::Directory(".gemini/antigravity-cli".to_owned()),
            ],
        ),
        cli(
            ProviderKey::Devin,
            vec![
                EvidenceSource::File(".local/share/devin/credentials.toml".to_owned()),
                EvidenceSource::File(
                    "Library/Application Support/devin/credentials.toml".to_owned(),
                ),
                EvidenceSource::File("AppData/Roaming/devin/credentials.toml".to_owned()),
            ],
        ),
        local_endpoint(
            "ollama:local",
            "ollama",
            "Ollama (local)",
            OLLAMA_BASE_URL,
            vec![".ollama"],
        ),
        local_endpoint(
            "lmstudio:local",
            "lmstudio",
            "LM Studio (local)",
            LMSTUDIO_BASE_URL,
            vec![".lmstudio"],
        ),
        // The P1 engine set serves WireApi::OpenAiChat only, so no Anthropic
        // Messages builtin ships yet: a registered-but-never-executable
        // integration is worse than an absent one.
        api_key_endpoint(
            "openai:api-key",
            "openai",
            "OpenAI (API key)",
            OPENAI_BASE_URL,
            WireApi::OpenAiChat,
            "OPENAI_API_KEY",
            KeyDelivery::Bearer,
            BTreeMap::new(),
        ),
        {
            let mut openrouter = api_key_endpoint(
                "openrouter:api-key",
                "openrouter",
                "OpenRouter (API key)",
                OPENROUTER_BASE_URL,
                WireApi::OpenAiChat,
                "OPENROUTER_API_KEY",
                KeyDelivery::Bearer,
                BTreeMap::new(),
            );
            // The one managed collector in the P1 matrix: OpenRouter reports
            // the key-scoped credit allowance on `/key`. No dedicated
            // monitoring credential - the execution binding is reused.
            openrouter.integration.monitoring = Some(MonitoringConfig {
                collector: CollectorId::new(crate::openrouter::OPENROUTER_KEY_COLLECTOR),
                credential: None,
                endpoint: None,
            });
            openrouter
        },
    ]
}

/// A CLI integration over a compiled adapter. The provider CLI owns its
/// credential; `Cli` carries no endpoint or auth fields. `marker sources`
/// are the provider-owned credential markers shared with catalog discovery.
fn cli(key: ProviderKey, sources: Vec<EvidenceSource>) -> IntegrationDescriptor {
    IntegrationDescriptor::builtin(
        Integration {
            id: IntegrationId::from(key),
            provider: ProviderId::from(key),
            name: key.display_name().to_owned(),
            execution: ExecutionConfig::Cli {
                adapter: CliAdapterId::new(key.as_str()),
            },
            monitoring: None,
        },
        sources,
    )
}

/// An unauthenticated local endpoint speaking the OpenAI chat Wire Api -
/// Ollama, LM Studio, or an equivalent. `auth` is explicit `None`: a local
/// address does not imply unauthenticated, the binding declares it.
fn local_endpoint(
    id: &str,
    provider: &str,
    name: &str,
    base_url: &str,
    marker_directories: Vec<&str>,
) -> IntegrationDescriptor {
    IntegrationDescriptor::builtin(
        Integration {
            id: IntegrationId::new(id),
            provider: ProviderId::new(provider),
            name: name.to_owned(),
            execution: ExecutionConfig::Http {
                endpoint: EndpointConfig {
                    base_url: base_url.to_owned(),
                    extra_headers: BTreeMap::new(),
                    request_timeout_seconds: None,
                },
                protocol: WireApi::OpenAiChat,
                auth: AuthBinding::None,
            },
            monitoring: None,
        },
        vec![EvidenceSource::ConfiguredEndpoint {
            marker_directories: marker_directories.into_iter().map(str::to_owned).collect(),
        }],
    )
}

/// A billed API-key endpoint. The `EnvOrStore` binding names the
/// conventional environment variable and reserves the Credential Reference
/// `aifuel auth set-key` writes to; a stored managed credential takes
/// precedence over the variable. Evidence covers both sources.
fn api_key_endpoint(
    id: &str,
    provider: &str,
    name: &str,
    base_url: &str,
    protocol: WireApi,
    env_var: &str,
    delivery: KeyDelivery,
    extra_headers: BTreeMap<String, String>,
) -> IntegrationDescriptor {
    IntegrationDescriptor::builtin(
        Integration {
            id: IntegrationId::new(id),
            provider: ProviderId::new(provider),
            name: name.to_owned(),
            execution: ExecutionConfig::Http {
                endpoint: EndpointConfig {
                    base_url: base_url.to_owned(),
                    extra_headers,
                    request_timeout_seconds: None,
                },
                protocol,
                auth: AuthBinding::ApiKey {
                    source: ApiKeySource::EnvOrStore {
                        var: env_var.to_owned(),
                        credential: CredentialRef::new(id),
                    },
                    delivery,
                },
            },
            monitoring: None,
        },
        vec![
            EvidenceSource::EnvVar(env_var.to_owned()),
            EvidenceSource::ManagedEntry(CredentialRef::new(id)),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_six_cli_integrations_appear_in_catalog_order() {
        let descriptors = builtin_integrations();
        let cli: Vec<(&str, &str)> = descriptors
            .iter()
            .filter(|d| matches!(d.integration.execution, ExecutionConfig::Cli { .. }))
            .map(|d| (d.integration.id.as_str(), d.integration.provider.as_str()))
            .collect();
        assert_eq!(
            cli,
            [
                ("claude", "claude"),
                ("codex", "codex"),
                ("copilot", "copilot"),
                ("gemini", "gemini"),
                ("antigravity", "antigravity"),
                ("devin", "devin"),
            ]
        );
    }

    #[test]
    fn cli_integrations_bind_compiled_adapters_and_no_credentials() {
        // A Cli execution owns no endpoint or credential: the provider CLI
        // owns auth. The adapter id equals the catalog provider key so the
        // compiled adapter factory can map back to a ProviderKey.
        for descriptor in builtin_integrations() {
            let ExecutionConfig::Cli { adapter } = &descriptor.integration.execution else {
                continue;
            };
            assert_eq!(adapter.as_str(), descriptor.integration.provider.as_str());
        }
    }

    #[test]
    fn local_endpoints_speak_openai_chat_with_explicit_no_auth() {
        let descriptors = builtin_integrations();
        for (id, base_url, marker) in [
            ("ollama:local", "http://localhost:11434/v1", ".ollama"),
            ("lmstudio:local", "http://localhost:1234/v1", ".lmstudio"),
        ] {
            let descriptor = descriptors
                .iter()
                .find(|d| d.integration.id.as_str() == id)
                .unwrap_or_else(|| panic!("{id} is a builtin"));
            let ExecutionConfig::Http {
                endpoint,
                protocol,
                auth,
            } = &descriptor.integration.execution
            else {
                panic!("{id} must be an Http integration");
            };
            assert_eq!(endpoint.base_url, base_url);
            assert_eq!(*protocol, WireApi::OpenAiChat);
            assert_eq!(
                *auth,
                AuthBinding::None,
                "local implies unauthenticated is wrong - the binding must be explicit"
            );
            assert_eq!(
                descriptor.sources,
                vec![EvidenceSource::ConfiguredEndpoint {
                    marker_directories: vec![marker.to_owned()],
                }]
            );
        }
    }

    #[test]
    fn api_key_integrations_bind_env_or_store_never_key_material() {
        let descriptors = builtin_integrations();
        for (id, provider, base_url, protocol, var) in [
            (
                "openai:api-key",
                "openai",
                "https://api.openai.com/v1",
                WireApi::OpenAiChat,
                "OPENAI_API_KEY",
            ),
            (
                "openrouter:api-key",
                "openrouter",
                "https://openrouter.ai/api/v1",
                WireApi::OpenAiChat,
                "OPENROUTER_API_KEY",
            ),
        ] {
            let descriptor = descriptors
                .iter()
                .find(|d| d.integration.id.as_str() == id)
                .unwrap_or_else(|| panic!("{id} is a builtin"));
            assert_eq!(descriptor.integration.provider.as_str(), provider);
            let ExecutionConfig::Http {
                endpoint,
                protocol: found_protocol,
                auth,
            } = &descriptor.integration.execution
            else {
                panic!("{id} must be an Http integration");
            };
            assert_eq!(endpoint.base_url, base_url);
            assert_eq!(*found_protocol, protocol);
            match auth {
                AuthBinding::ApiKey {
                    source:
                        ApiKeySource::EnvOrStore {
                            var: found,
                            credential,
                        },
                    ..
                } => {
                    assert_eq!(found, var);
                    // The managed Credential Reference a builtin reserves
                    // equals its own Integration Identity.
                    assert_eq!(credential.as_str(), id);
                }
                other => panic!("{id} must bind an env-or-store API key, got {other:?}"),
            }
            assert_eq!(
                descriptor.sources,
                vec![
                    EvidenceSource::EnvVar(var.to_owned()),
                    EvidenceSource::ManagedEntry(CredentialRef::new(id)),
                ]
            );
        }
    }

    #[test]
    fn no_builtin_declares_a_wire_protocol_without_an_engine() {
        // P1 serves openai_chat only; a builtin on another protocol would be
        // registered but never executable, which is worse than absent.
        for descriptor in builtin_integrations() {
            if let ExecutionConfig::Http { protocol, .. } = &descriptor.integration.execution {
                assert_eq!(
                    *protocol,
                    WireApi::OpenAiChat,
                    "builtin {} declares an unserveable Wire Api",
                    descriptor.integration.id
                );
            }
        }
    }

    #[test]
    fn every_builtin_provider_resolves_to_exactly_one_integration() {
        // A bare provider id is a billing-sensitive selector: it must be
        // unambiguous across the whole builtin set, or resolve() would have
        // to error on ordinary invocations.
        let descriptors = builtin_integrations();
        let mut by_provider: BTreeMap<&str, usize> = BTreeMap::new();
        for descriptor in &descriptors {
            *by_provider
                .entry(descriptor.integration.provider.as_str())
                .or_default() += 1;
        }
        for (provider, count) in by_provider {
            assert_eq!(
                count, 1,
                "provider '{provider}' is ambiguous among builtins"
            );
        }
    }
}
