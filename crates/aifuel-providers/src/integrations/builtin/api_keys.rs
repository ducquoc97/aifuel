//! The compiled API-key Provider Integrations.
//!
//! Every entry is a billed (or free-tier) endpoint bound to
//! `AuthBinding::ApiKey` with `ApiKeySource::EnvOrStore`: the provider's
//! documented environment variable works as-is, and `aifuel auth set-key
//! <integration>` stores a managed Credential Reference that then takes
//! precedence. No key material ever appears here - only the variable name
//! and the managed Credential Reference are declared.
//!
//! The provider set and base URLs are ported from OmniRoute's provider
//! catalog (`release/v3.8.52`, MIT licensed) and cross-checked against each
//! provider's own API documentation; free-tier notes live in
//! `crate::catalog` keyed by provider id. A provider
//! only lands here when its documented base URL serves a compiled Wire
//! Api - `WireApi::OpenAiChat` (the `POST {base_url}/chat/completions`
//! shape) for the OpenAI-compatible entries, `WireApi::AnthropicMessages`
//! for Anthropic's native API. `collector` names a compiled Monitoring
//! Collection Contract and is set only for providers that document a
//! quota, usage, or balance endpoint; an inference protocol alone never
//! implies one.

use crate::integrations::{EvidenceSource, IntegrationDescriptor};
use aifuel_core::{
    ApiKeySource, AuthBinding, CollectorId, CredentialRef, EndpointConfig, ExecutionConfig,
    Integration, IntegrationId, KeyDelivery, MonitoringConfig, ProviderId, WireApi,
};
use std::collections::BTreeMap;

/// One declarative row in the API-key provider table.
struct ApiKeyProvider {
    /// The Integration Id, `<provider>:api-key`.
    id: &'static str,
    /// The catalog provider id the integration belongs to.
    provider: &'static str,
    /// The display name surfaced in `aifuel auth list`.
    name: &'static str,
    /// The documented base URL the declared Wire Api serves.
    base_url: &'static str,
    /// The provider's conventional environment variable.
    env_var: &'static str,
    /// The compiled collector id, when the provider documents a quota,
    /// usage, or balance endpoint.
    collector: Option<&'static str>,
    /// The Wire Api the documented base URL serves.
    protocol: WireApi,
    /// The named header carrying the key when the provider does not use
    /// `Authorization: Bearer` (Anthropic's `x-api-key`).
    key_header: Option<&'static str>,
}

const API_KEY_PROVIDERS: &[ApiKeyProvider] = &[
    ApiKeyProvider {
        id: "openai:api-key",
        provider: "openai",
        name: "OpenAI (API key)",
        base_url: "https://api.openai.com/v1",
        env_var: "OPENAI_API_KEY",
        collector: None,
        protocol: WireApi::OpenAiChat,
        key_header: None,
    },
    ApiKeyProvider {
        id: "openrouter:api-key",
        provider: "openrouter",
        name: "OpenRouter (API key)",
        base_url: "https://openrouter.ai/api/v1",
        env_var: "OPENROUTER_API_KEY",
        // OpenRouter reports the key-scoped credit allowance on `/key`.
        collector: Some(crate::openrouter::OPENROUTER_KEY_COLLECTOR),
        protocol: WireApi::OpenAiChat,
        key_header: None,
    },
    ApiKeyProvider {
        id: "anthropic:api-key",
        provider: "anthropic",
        name: "Anthropic (API key)",
        // The native API origin; the wire layer appends `/v1/messages`.
        base_url: "https://api.anthropic.com",
        env_var: "ANTHROPIC_API_KEY",
        // Anthropic documents no quota or balance endpoint.
        collector: None,
        protocol: WireApi::AnthropicMessages,
        // Anthropic keys authenticate on `x-api-key`, not Bearer.
        key_header: Some("x-api-key"),
    },
    ApiKeyProvider {
        id: "cerebras:api-key",
        provider: "cerebras",
        name: "Cerebras (API key)",
        base_url: "https://api.cerebras.ai/v1",
        env_var: "CEREBRAS_API_KEY",
        collector: None,
        protocol: WireApi::OpenAiChat,
        key_header: None,
    },
    ApiKeyProvider {
        id: "cohere:api-key",
        provider: "cohere",
        name: "Cohere (API key)",
        base_url: "https://api.cohere.ai/compatibility/v1",
        env_var: "COHERE_API_KEY",
        collector: None,
        protocol: WireApi::OpenAiChat,
        key_header: None,
    },
    ApiKeyProvider {
        id: "deepinfra:api-key",
        provider: "deepinfra",
        name: "DeepInfra (API key)",
        base_url: "https://api.deepinfra.com/v1/openai",
        env_var: "DEEPINFRA_API_KEY",
        collector: None,
        protocol: WireApi::OpenAiChat,
        key_header: None,
    },
    ApiKeyProvider {
        id: "deepseek:api-key",
        provider: "deepseek",
        name: "DeepSeek (API key)",
        base_url: "https://api.deepseek.com",
        env_var: "DEEPSEEK_API_KEY",
        // DeepSeek reports the prepaid credit balance on `/user/balance`.
        collector: Some(crate::deepseek::DEEPSEEK_BALANCE_COLLECTOR),
        protocol: WireApi::OpenAiChat,
        key_header: None,
    },
    ApiKeyProvider {
        id: "fireworks:api-key",
        provider: "fireworks",
        name: "Fireworks AI (API key)",
        base_url: "https://api.fireworks.ai/inference/v1",
        env_var: "FIREWORKS_API_KEY",
        collector: None,
        protocol: WireApi::OpenAiChat,
        key_header: None,
    },
    ApiKeyProvider {
        id: "groq:api-key",
        provider: "groq",
        name: "Groq (API key)",
        base_url: "https://api.groq.com/openai/v1",
        env_var: "GROQ_API_KEY",
        collector: None,
        protocol: WireApi::OpenAiChat,
        key_header: None,
    },
    ApiKeyProvider {
        id: "huggingface:api-key",
        provider: "huggingface",
        name: "Hugging Face (API key)",
        base_url: "https://router.huggingface.co/v1",
        env_var: "HF_TOKEN",
        collector: None,
        protocol: WireApi::OpenAiChat,
        key_header: None,
    },
    ApiKeyProvider {
        id: "mistral:api-key",
        provider: "mistral",
        name: "Mistral (API key)",
        base_url: "https://api.mistral.ai/v1",
        env_var: "MISTRAL_API_KEY",
        collector: None,
        protocol: WireApi::OpenAiChat,
        key_header: None,
    },
    ApiKeyProvider {
        id: "moonshot:api-key",
        provider: "moonshot",
        name: "Moonshot AI (API key)",
        base_url: "https://api.moonshot.ai/v1",
        env_var: "MOONSHOT_API_KEY",
        collector: None,
        protocol: WireApi::OpenAiChat,
        key_header: None,
    },
    ApiKeyProvider {
        id: "nvidia:api-key",
        provider: "nvidia",
        name: "NVIDIA NIM (API key)",
        base_url: "https://integrate.api.nvidia.com/v1",
        env_var: "NVIDIA_API_KEY",
        collector: None,
        protocol: WireApi::OpenAiChat,
        key_header: None,
    },
    ApiKeyProvider {
        id: "perplexity:api-key",
        provider: "perplexity",
        name: "Perplexity (API key)",
        base_url: "https://api.perplexity.ai",
        env_var: "PERPLEXITY_API_KEY",
        collector: None,
        protocol: WireApi::OpenAiChat,
        key_header: None,
    },
    ApiKeyProvider {
        id: "siliconflow:api-key",
        provider: "siliconflow",
        name: "SiliconFlow (API key)",
        base_url: "https://api.siliconflow.com/v1",
        env_var: "SILICONFLOW_API_KEY",
        // SiliconFlow reports the wallet balance on `/v1/user/info`.
        collector: Some(crate::siliconflow::SILICONFLOW_BALANCE_COLLECTOR),
        protocol: WireApi::OpenAiChat,
        key_header: None,
    },
    ApiKeyProvider {
        id: "together:api-key",
        provider: "together",
        name: "Together AI (API key)",
        base_url: "https://api.together.xyz/v1",
        env_var: "TOGETHER_API_KEY",
        collector: None,
        protocol: WireApi::OpenAiChat,
        key_header: None,
    },
    ApiKeyProvider {
        id: "xai:api-key",
        provider: "xai",
        name: "xAI (API key)",
        base_url: "https://api.x.ai/v1",
        env_var: "XAI_API_KEY",
        collector: None,
        protocol: WireApi::OpenAiChat,
        key_header: None,
    },
    ApiKeyProvider {
        id: "zai:api-key",
        provider: "zai",
        name: "Z.AI (API key)",
        // The GLM Coding Plan endpoint; the coding-plan quota monitor at
        // `/api/monitor/usage/quota/limit` is scoped to this surface.
        base_url: "https://api.z.ai/api/coding/paas/v4",
        env_var: "ZAI_API_KEY",
        collector: Some(crate::zai::ZAI_QUOTA_COLLECTOR),
        protocol: WireApi::OpenAiChat,
        key_header: None,
    },
];

/// The compiled API-key integrations in table order. Rows declaring a
/// collector get a Monitoring Collection Contract that reuses the execution
/// binding - no dedicated monitoring credential is required.
pub(super) fn integrations() -> Vec<IntegrationDescriptor> {
    API_KEY_PROVIDERS
        .iter()
        .map(|spec| {
            let delivery = match spec.key_header {
                Some(name) => KeyDelivery::Header {
                    name: name.to_owned(),
                },
                None => KeyDelivery::Bearer,
            };
            let mut descriptor = api_key_endpoint(
                spec.id,
                spec.provider,
                spec.name,
                spec.base_url,
                spec.protocol,
                spec.env_var,
                delivery,
                BTreeMap::new(),
            );
            if let Some(collector) = spec.collector {
                descriptor.integration.monitoring = Some(MonitoringConfig {
                    collector: CollectorId::new(collector),
                    credential: None,
                    endpoint: None,
                });
            }
            descriptor
        })
        .collect()
}

/// A billed API-key endpoint. The `EnvOrStore` binding names the
/// conventional environment variable and reserves the Credential Reference
/// `aifuel auth set-key` writes to; a stored managed credential takes
/// precedence over the variable. Evidence covers both sources.
#[allow(clippy::too_many_arguments)]
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
    fn every_entry_uses_its_documented_binding() {
        // The provider set was ported only where the documented base URL
        // speaks a compiled Wire Api and a conventional env var exists;
        // the table must keep both properties true for every row, and a
        // row must never declare a protocol no engine serves.
        let descriptors = integrations();
        assert_eq!(descriptors.len(), API_KEY_PROVIDERS.len());
        for spec in API_KEY_PROVIDERS {
            let descriptor = descriptors
                .iter()
                .find(|d| d.integration.id.as_str() == spec.id)
                .unwrap_or_else(|| panic!("{} is a builtin", spec.id));
            assert_eq!(descriptor.integration.provider.as_str(), spec.provider);
            assert_eq!(descriptor.integration.name, spec.name);
            let ExecutionConfig::Http {
                endpoint,
                protocol,
                auth,
            } = &descriptor.integration.execution
            else {
                panic!("{} must be an Http integration", spec.id);
            };
            assert_eq!(endpoint.base_url, spec.base_url);
            assert_eq!(*protocol, spec.protocol);
            assert!(
                crate::wire::serves(*protocol),
                "{} declares an unserveable Wire Api",
                spec.id
            );
            match auth {
                AuthBinding::ApiKey {
                    source: ApiKeySource::EnvOrStore { var, credential },
                    delivery,
                } => {
                    assert_eq!(var, spec.env_var);
                    // The managed Credential Reference a builtin reserves
                    // equals its own Integration Identity.
                    assert_eq!(credential.as_str(), spec.id);
                    let expected = match spec.key_header {
                        Some(name) => KeyDelivery::Header {
                            name: name.to_owned(),
                        },
                        None => KeyDelivery::Bearer,
                    };
                    assert_eq!(
                        *delivery, expected,
                        "{} must deliver the key the documented way",
                        spec.id
                    );
                }
                other => panic!(
                    "{} must bind an env-or-store API key, got {other:?}",
                    spec.id
                ),
            }
            assert_eq!(
                descriptor.sources,
                vec![
                    EvidenceSource::EnvVar(spec.env_var.to_owned()),
                    EvidenceSource::ManagedEntry(CredentialRef::new(spec.id)),
                ]
            );
        }
    }

    #[test]
    fn declared_collectors_are_compiled_and_every_id_is_unique() {
        // A collector name that resolves to nothing would register an
        // integration whose monitoring can never run; a duplicated id would
        // make selection ambiguous.
        let mut ids = std::collections::BTreeSet::new();
        for spec in API_KEY_PROVIDERS {
            assert!(ids.insert(spec.id), "{} is duplicated", spec.id);
            if let Some(collector) = spec.collector {
                assert!(
                    crate::quota::compiled_collector_ids().contains(&collector),
                    "{collector} is not a compiled collector"
                );
            }
        }
    }

    #[test]
    fn monitored_integrations_bind_the_documented_collector() {
        let descriptors = integrations();
        for (id, collector) in [
            (
                "openrouter:api-key",
                crate::openrouter::OPENROUTER_KEY_COLLECTOR,
            ),
            (
                "deepseek:api-key",
                crate::deepseek::DEEPSEEK_BALANCE_COLLECTOR,
            ),
            (
                "siliconflow:api-key",
                crate::siliconflow::SILICONFLOW_BALANCE_COLLECTOR,
            ),
            ("zai:api-key", crate::zai::ZAI_QUOTA_COLLECTOR),
        ] {
            let descriptor = descriptors
                .iter()
                .find(|d| d.integration.id.as_str() == id)
                .unwrap_or_else(|| panic!("{id} is a builtin"));
            let monitoring = descriptor
                .integration
                .monitoring
                .as_ref()
                .unwrap_or_else(|| panic!("{id} declares a monitoring contract"));
            assert_eq!(monitoring.collector.as_str(), collector);
            // No dedicated monitoring credential - the execution binding is
            // reused.
            assert!(monitoring.credential.is_none());
        }
    }
}
