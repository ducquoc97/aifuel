//! The built-in Provider Integrations.
//!
//! Six CLI integrations wrap the compiled CLI adapters (`ExecutionConfig::Cli`
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
    AuthBinding, CliAdapterId, EndpointConfig, ExecutionConfig, Integration, IntegrationId,
    ProviderId, ProviderKey, WireApi,
};
use std::collections::BTreeMap;

const OLLAMA_BASE_URL: &str = "http://localhost:11434/v1";
const LMSTUDIO_BASE_URL: &str = "http://localhost:1234/v1";

/// The built-in integration descriptors: the six compiled-adapter CLI
/// integrations in pinned catalog order, the OpenCode runtime adapter
/// integration, then the local-server and API-key HTTP integrations.
///
/// CLI integration ids equal the catalog provider ids (`claude`, `codex`,
/// ...) so they match what the compiled CLI adapters report and what stored
/// legacy selections already name; `opencode` follows the same convention
/// for its runtime adapter. HTTP builtins carry a distinguishing
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cli_integrations_appear_in_catalog_order() {
        // The six compiled-adapter integrations in pinned catalog order,
        // then the runtime-adapter integrations (`opencode`).
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
                ("opencode", "opencode"),
                ("cursor", "cursor"),
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
    fn no_builtin_declares_a_wire_protocol_without_an_engine() {
        // A builtin on an unserved protocol would be registered but never
        // executable, which is worse than absent. The documented exception
        // is the `*:web` set: browser-session integrations exist to carry
        // their Monitoring Collection Contract, and the declared protocol
        // names the upstream surface as evidence.
        const MONITORING_ONLY: &[&str] = &["claude-web:web"];
        for descriptor in builtin_integrations() {
            if let ExecutionConfig::Http { protocol, .. } = &descriptor.integration.execution {
                assert!(
                    crate::wire::serves(*protocol)
                        || MONITORING_ONLY.contains(&descriptor.integration.id.as_str()),
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
