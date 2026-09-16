use aifuel_core::CatalogProviderStatus;

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
                "claude" | "codex" | "copilot" | "gemini" => ("supported", "supported"),
                "antigravity" => ("supported", "unsupported"),
                _ => ("unsupported", "unsupported"),
            };
            CatalogProviderStatus {
                id: (*id).to_owned(),
                monitoring: monitoring.to_owned(),
                agent_execution: agent_execution.to_owned(),
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
