use aifuel_core::{CapabilityState, CatalogPlatformStatus, CatalogProviderStatus};

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
    "gemini",
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
];

pub(crate) fn statuses() -> Vec<CatalogProviderStatus> {
    PINNED_PROVIDER_IDS
        .iter()
        .map(|id| {
            let (monitoring, agent_execution) = match *id {
                "claude" | "codex" | "copilot" | "gemini" | "devin" => {
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
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_catalog_preserves_all_provider_ids() {
        assert_eq!(PINNED_PROVIDER_IDS.len(), 69);
        assert_eq!(PINNED_PROVIDER_IDS.first(), Some(&"codex"));
        assert_eq!(PINNED_PROVIDER_IDS.last(), Some(&"ibmbob"));
    }
}
