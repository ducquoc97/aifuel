use aifuel_core::AgentExecutionAdapter;

static ADAPTERS: &[&dyn AgentExecutionAdapter] = &[
    &crate::claude::AGENT_RUN_ADAPTER,
    &crate::codex::AGENT_RUN_ADAPTER,
    &crate::copilot::AGENT_RUN_ADAPTER,
    &crate::gemini::AGENT_RUN_ADAPTER,
    &crate::antigravity::AGENT_RUN_ADAPTER,
];

/// Return the compiled execution adapters registered for the built-in providers.
pub fn agent_run_adapters() -> &'static [&'static dyn AgentExecutionAdapter] {
    ADAPTERS
}

#[cfg(test)]
mod tests {
    use super::*;
    use aifuel_core::ProviderKey;

    #[test]
    fn execution_registration_is_independent_of_monitoring_catalog_membership() {
        let providers = agent_run_adapters()
            .iter()
            .map(|adapter| adapter.provider())
            .collect::<Vec<_>>();

        assert_eq!(
            providers,
            vec![
                ProviderKey::Claude,
                ProviderKey::Codex,
                ProviderKey::Copilot,
                ProviderKey::Gemini,
                ProviderKey::Antigravity,
            ]
        );
    }
}
