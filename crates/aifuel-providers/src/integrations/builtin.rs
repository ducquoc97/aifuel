//! The built-in Provider Integrations.
//!
//! Five CLI integrations wrap the compiled CLI adapters (`ExecutionConfig::Cli`
//! - the provider CLI owns its credential, so no auth binding exists). Their
//! evidence sources are the same provider-owned credential markers the
//! catalog registry inspects; the marker paths are repeated here because the
//! catalog definitions do not expose them.
//!
//! The `opencode` integration also carries `ExecutionConfig::Cli`, but its
//! adapter is the reusable runtime `OpenCodeAdapter`, not a compiled CLI
//! adapter: OpenCode executes through its headless `opencode serve` HTTP
//! API. Its evidence source is OpenCode's documented auth store,
//! `~/.local/share/opencode/auth.json`. The `cursor` integration is the
//! same shape over the generic `AcpAdapter`: `cursor-agent acp` speaks the
//! Agent Client Protocol, and its evidence sources are the documented
//! `~/.cursor/cli-config.json` plus the `CURSOR_API_KEY`/`CURSOR_AUTH_TOKEN`
//! environment variables.
//!
//! The `<provider>:oauth` integrations also carry `ExecutionConfig::Cli`,
//! bound to the compiled `*_oauth` direct HTTP adapters: they run prompt
//! completion over the provider-owned subscription API surfaces using the
//! provider's own local credential files - no endpoint or auth binding is
//! configured because the adapter, not the credential store, reads the
//! file. Their ids carry the `:oauth` suffix so a selection can never
//! alias the provider's CLI integration: `codex` and `codex:oauth` are
//! distinct billing surfaces and bare `codex` always resolves to the CLI.
//!
//! The HTTP builtins cover local OpenAI-compatible servers (`ollama:local`,
//! `lmstudio:local`) with explicit `AuthBinding::None`, plus the
//! API-key provider catalog in [`api_keys`]. An API-key
//! builtin binds `EnvOrStore`: the conventional environment variable works
//! as-is, and `aifuel auth set-key <integration>` stores a managed credential
//! that then takes precedence. No key material ever appears here - only the
//! variable name and the managed Credential Reference are declared.

mod api_keys;
mod web;

use super::evidence::EvidenceSource;
use super::registry::IntegrationDescriptor;
use aifuel_core::{
    AuthBinding, CliAdapterId, CredentialRef, EndpointConfig, ExecutionConfig, Integration,
    IntegrationId, ProviderId, ProviderKey, WireApi,
};
use std::collections::BTreeMap;

const OLLAMA_BASE_URL: &str = "http://localhost:11434/v1";
const LMSTUDIO_BASE_URL: &str = "http://localhost:1234/v1";

/// The built-in integration descriptors: the five compiled-adapter CLI
/// integrations in pinned catalog order, the OpenCode runtime adapter
/// integration, the compiled `*:oauth` direct HTTP adapters, then the
/// local-server and API-key HTTP integrations.
///
/// CLI integration ids equal the catalog provider ids (`claude`, `codex`,
/// ...) so they match what the compiled CLI adapters report and what stored
/// legacy selections already name; `opencode` follows the same convention
/// for its runtime adapter. The OAuth and HTTP builtins carry a
/// distinguishing suffix (`codex:oauth`, `ollama:local`, `openai:api-key`)
/// because their provider ids have sibling integrations and a bare
/// provider id must keep resolving to the canonical one.
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
        agent_cli(
            "opencode",
            "OpenCode",
            // `opencode auth login` persists provider credentials in the
            // documented data-dir auth store, shared by the TUI and the
            // headless server.
            vec![EvidenceSource::File(
                ".local/share/opencode/auth.json".to_owned(),
            )],
        ),
        // The `cursor` integration is served by the generic AcpAdapter:
        // `cursor-agent acp` is its documented ACP invocation, and the
        // adapter's spawn argv is the only Cursor-specific fact.
        agent_cli(
            "cursor",
            "Cursor",
            vec![
                EvidenceSource::File(".cursor/cli-config.json".to_owned()),
                EvidenceSource::EnvVar("CURSOR_API_KEY".to_owned()),
                EvidenceSource::EnvVar("CURSOR_AUTH_TOKEN".to_owned()),
            ],
        ),
        // The compiled OAuth direct-HTTP adapters: subscription-backed
        // execution without the provider CLI. `devin:oauth` is registered
        // for honest listing (unavailable) since no documented direct
        // inference path exists for its local credential.
        oauth_cli(
            ProviderKey::Codex,
            "Codex (ChatGPT OAuth)",
            vec![
                // A managed grant minted by `aifuel auth login codex` is
                // present evidence too, winning over the CLI's file.
                EvidenceSource::ManagedEntry(CredentialRef::new("codex:oauth")),
                EvidenceSource::File(".codex/auth.json".to_owned()),
            ],
        ),
        oauth_cli(
            ProviderKey::Copilot,
            "GitHub Copilot (OAuth)",
            vec![
                EvidenceSource::ManagedEntry(CredentialRef::new("copilot:oauth")),
                EvidenceSource::File(".copilot/config.json".to_owned()),
                EvidenceSource::File(".config/github-copilot/hosts.json".to_owned()),
                EvidenceSource::File(".config/github-copilot/apps.json".to_owned()),
            ],
        ),
        oauth_cli(
            ProviderKey::Devin,
            "Devin (OAuth)",
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
        // TypeSafe AI's System One decision surface: an API-key HTTP
        // endpoint like the `api_keys` catalog, but the declared Wire Api
        // is `Decisions` - a structured `state` + `questions` surface the
        // gateway forwards to, never an Agent Run engine.
        decisions_endpoint(
            "typesafe:api-key",
            "typesafe",
            "TypeSafe AI (API key)",
            "https://api.typesafe.ai/v1",
            "TYPESAFE_API_KEY",
        ),
    ]
    .into_iter()
    // The API-key catalog: OpenAI-compatible endpoints plus Anthropic's
    // native Messages API. Every row must declare a served Wire Api - a
    // registered-but-never-executable integration is worse than absent.
    .chain(api_keys::integrations())
    // The `*:web` browser-session integrations: monitoring-only by design,
    // documented as the exception to the served-protocol rule.
    .chain(web::integrations())
    .collect()
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

/// A compiled OAuth HTTP adapter bound as a `Cli` execution, like `cli`
/// but with the distinguishing `<provider>:oauth` id. The adapter owns the
/// provider's local credential file - `Cli` carries no endpoint or auth
/// fields for it - and the id's suffix keeps it from ever aliasing the
/// provider's CLI integration.
fn oauth_cli(key: ProviderKey, name: &str, sources: Vec<EvidenceSource>) -> IntegrationDescriptor {
    let id = format!("{}:oauth", key.as_str());
    IntegrationDescriptor::builtin(
        Integration {
            id: IntegrationId::new(&id),
            provider: ProviderId::from(key),
            name: name.to_owned(),
            execution: ExecutionConfig::Cli {
                adapter: CliAdapterId::new(&id),
            },
            monitoring: None,
        },
        sources,
    )
}

/// A CLI integration over a runtime `AgentAdapter`. Same `Cli` execution
/// binding as `cli`, but for providers outside the closed `ProviderKey`
/// catalog: the adapter id still equals the provider id so execution
/// resolution and evidence cannot disagree.
fn agent_cli(id: &str, name: &str, sources: Vec<EvidenceSource>) -> IntegrationDescriptor {
    IntegrationDescriptor::builtin(
        Integration {
            id: IntegrationId::new(id),
            provider: ProviderId::new(id),
            name: name.to_owned(),
            execution: ExecutionConfig::Cli {
                adapter: CliAdapterId::new(id),
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

/// An API-key endpoint speaking `WireApi::Decisions` - a structured
/// decision surface, not a chat engine. The binding is the API-key
/// catalog's `EnvOrStore` shape, but the integration lives beside the
/// locals here rather than in `api_keys`: that table asserts every row
/// serves a compiled run surface, and a decision endpoint never does.
fn decisions_endpoint(
    id: &str,
    provider: &str,
    name: &str,
    base_url: &str,
    env_var: &str,
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
                protocol: WireApi::Decisions,
                auth: AuthBinding::ApiKey {
                    source: aifuel_core::ApiKeySource::EnvOrStore {
                        var: env_var.to_owned(),
                        credential: aifuel_core::CredentialRef::new(id),
                    },
                    delivery: aifuel_core::KeyDelivery::Bearer,
                },
            },
            monitoring: None,
        },
        vec![
            EvidenceSource::EnvVar(env_var.to_owned()),
            EvidenceSource::ManagedEntry(aifuel_core::CredentialRef::new(id)),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cli_integrations_appear_in_catalog_order() {
        // The five compiled-adapter integrations in pinned catalog order,
        // then the runtime-adapter integrations (`opencode`, `cursor`),
        // then the compiled OAuth direct HTTP adapters.
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
                ("antigravity", "antigravity"),
                ("devin", "devin"),
                ("opencode", "opencode"),
                ("cursor", "cursor"),
                ("codex:oauth", "codex"),
                ("copilot:oauth", "copilot"),
                ("devin:oauth", "devin"),
            ]
        );
    }

    #[test]
    fn cli_integrations_bind_compiled_adapters_and_no_credentials() {
        // A Cli execution owns no endpoint or credential: the provider owns
        // auth. The adapter id equals the integration id, so an OAuth
        // adapter cannot alias a bare CLI integration and execution
        // resolution cannot disagree with evidence.
        for descriptor in builtin_integrations() {
            let ExecutionConfig::Cli { adapter } = &descriptor.integration.execution else {
                continue;
            };
            assert_eq!(adapter.as_str(), descriptor.integration.id.as_str());
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
    fn no_builtin_declares_a_wire_protocol_without_an_engine() {
        // A builtin on an unserved protocol would be registered but never
        // executable, which is worse than absent. Two documented
        // exceptions: the `*:web` set, browser-session integrations that
        // exist to carry their Monitoring Collection Contract; and the
        // `WireApi::Decisions` endpoints, which the gateway's `/v1/decisions`
        // forward serves without an Agent Run engine.
        const FORWARD_ONLY: &[&str] = &["claude-web:web", "typesafe:api-key"];
        for descriptor in builtin_integrations() {
            if let ExecutionConfig::Http { protocol, .. } = &descriptor.integration.execution {
                assert!(
                    crate::wire::serves(*protocol)
                        || FORWARD_ONLY.contains(&descriptor.integration.id.as_str()),
                    "builtin {} declares an unserveable Wire Api",
                    descriptor.integration.id
                );
            }
        }
    }

    #[test]
    fn every_builtin_provider_resolves_unambiguously() {
        // A bare provider id is a billing-sensitive selector: exact-match
        // resolution wins over candidates, so a provider may gain `*:oauth`
        // siblings only while exactly one integration keeps the bare id.
        let descriptors = builtin_integrations();
        let mut by_provider: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for descriptor in &descriptors {
            by_provider
                .entry(descriptor.integration.provider.as_str())
                .or_default()
                .push(descriptor.integration.id.as_str());
        }
        for (provider, ids) in by_provider {
            if ids.len() > 1 {
                assert!(
                    ids.contains(&provider),
                    "provider '{provider}' has {ids:?} but none owns the bare id, \
                     so a bare selection would be ambiguous"
                );
            }
        }
    }
}
