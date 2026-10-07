use aifuel_core::{CapabilityState, CatalogPlatformStatus, CatalogProviderStatus};

/// Free-tier capability notes per catalog provider id, ported from
/// OmniRoute's audited provider catalog (`release/v3.8.52`, MIT licensed):
/// the `hasFree`/`freeNote` fields in `src/shared/constants/providers/**`
/// and `docs/reference/FREE_TIERS.md`. Membership means the provider
/// documents a free tier usable through an API-key or native credential;
/// absence means none is documented, not a guarantee none exists.
const FREE_TIER_NOTES: &[(&str, &str)] = &[
    (
        "openrouter",
        "Free models at $0/token with :free suffix - 20 RPM / 200 RPD",
    ),
    (
        "groq",
        "Free plan: per-model caps of 200K tokens/day per chat model",
    ),
    (
        "mistral",
        "Free Experiment tier: rate-limited access to all models, no card required",
    ),
    (
        "deepseek",
        "5M free tokens on signup (one-time, no card required)",
    ),
    (
        "fireworks",
        "$1 free starter credits on signup for API testing",
    ),
    (
        "cerebras",
        "One-time $5 signup credit (30-day validity); a payment method is required",
    ),
    (
        "cohere",
        "Free Trial: 1,000 API calls/month for testing, no card required",
    ),
    (
        "siliconflow",
        "$1 free credits plus permanently free $0 models",
    ),
    ("nvidia", "Free dev access: ~40 RPM, 70+ models"),
    ("huggingface", "Free inference credits (~$0.10/month cap)"),
    (
        "deepinfra",
        "Free signup credits for API testing and model exploration",
    ),
];

/// The free-tier note recorded for a catalog provider id, when the imported
/// catalog data documents one.
pub fn free_tier_note(provider: &str) -> Option<&'static str> {
    FREE_TIER_NOTES
        .iter()
        .find(|(id, _)| *id == provider)
        .map(|(_, note)| *note)
}

pub const PINNED_PROVIDER_IDS: &[&str] = &[
    "codex",
    "openai",
    "azureopenai",
    "claude",
    "clinepass",
    "cursor",
    "opencode",
    "opencodego",
    "alibaba",
    "alibabatokenplan",
    "qwencloud",
    "factory",
    "fireworks",
    "antigravity",
    "copilot",
    "devin",
    "zai",
    "minimax",
    "manus",
    "kimi",
    "kilo",
    "kiro",
    "vertexai",
    "augment",
    "jetbrains",
    "moonshot",
    "amp",
    "t3chat",
    "ollama",
    "synthetic",
    "openrouter",
    "elevenlabs",
    "warp",
    "windsurf",
    "zed",
    "perplexity",
    "mimo",
    "doubao",
    "sakana",
    "abacus",
    "mistral",
    "deepseek",
    "deepinfra",
    "codebuff",
    "crof",
    "venice",
    "commandcode",
    "qoder",
    "stepfun",
    "bedrock",
    "grok",
    "groq",
    "llmproxy",
    "litellm",
    "deepgram",
    "poe",
    "chutes",
    "neuralwatt",
    "clawrouter",
    "longcat",
    "sub2api",
    "wayfinder",
    "zenmux",
    "aiand",
    "zoommate",
    "xai",
    "notion",
    "ibmbob",
    "cerebras",
    "cohere",
    "huggingface",
    "nvidia",
    "siliconflow",
    "together",
    "anthropic",
];

pub(crate) fn statuses() -> Vec<CatalogProviderStatus> {
    PINNED_PROVIDER_IDS
        .iter()
        .map(|id| {
            let (monitoring, agent_execution) = match *id {
                "claude" | "codex" | "copilot" | "devin" => {
                    (CapabilityState::Supported, CapabilityState::Supported)
                }
                "antigravity" => (CapabilityState::Supported, CapabilityState::Unsupported),
                _ => (CapabilityState::Unsupported, CapabilityState::Unsupported),
            };
            let implemented = !matches!(monitoring, CapabilityState::Unsupported);
            let platform_monitoring = if implemented {
                CapabilityState::Unknown
            } else {
                CapabilityState::Unsupported
            };
            let platform_agent_execution = if agent_execution == CapabilityState::Supported {
                CapabilityState::Unknown
            } else {
                agent_execution
            };
            let platforms = ["macos", "linux", "windows"]
                .into_iter()
                .map(|platform| CatalogPlatformStatus {
                    platform: platform.to_owned(),
                    monitoring: platform_monitoring,
                    agent_execution: platform_agent_execution,
                    reason: if implemented {
                        "Rust adapter is implemented, but this platform has not passed live acceptance in this build".to_owned()
                    } else {
                        "No Rust provider adapter is implemented; capability remains explicitly unsupported".to_owned()
                    },
                })
                .collect();
            let free_note = free_tier_note(id).map(str::to_owned);
            CatalogProviderStatus {
                id: (*id).to_owned(),
                monitoring,
                agent_execution,
                evidence: if implemented {
                    "Rust provider adapter and fixture coverage".to_owned()
                } else {
                    "Pinned provider inventory only; no Rust adapter".to_owned()
                },
                platforms,
                has_free: free_note.is_some(),
                free_note,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_catalog_preserves_all_provider_ids() {
        assert_eq!(PINNED_PROVIDER_IDS.len(), 75);
        assert_eq!(PINNED_PROVIDER_IDS.first(), Some(&"codex"));
        assert_eq!(PINNED_PROVIDER_IDS.last(), Some(&"anthropic"));
        assert!(!PINNED_PROVIDER_IDS.contains(&"gemini"));
        assert!(PINNED_PROVIDER_IDS.contains(&"antigravity"));
    }
}
