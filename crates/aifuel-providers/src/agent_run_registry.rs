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
    use aifuel_core::{AgentCapability, CapabilityState, ProviderKey};

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

    #[test]
    fn every_compiled_integration_declares_each_capability_with_a_reason() {
        for adapter in agent_run_adapters() {
            let declarations = adapter.declared_agent_capabilities();
            assert_eq!(declarations.len(), AgentCapability::ALL.len());
            for capability in AgentCapability::ALL {
                let declaration = declarations
                    .get(&capability)
                    .expect("all capabilities have an independent declaration");
                assert!(
                    !declaration.reason.is_empty(),
                    "{} has an empty reason for {:?}",
                    adapter.provider(),
                    capability
                );
            }
        }

        let codex = agent_run_adapters()
            .iter()
            .find(|adapter| adapter.provider() == ProviderKey::Codex)
            .expect("Codex is compiled");
        let declarations = codex.declared_agent_capabilities();
        assert_eq!(
            declarations[&AgentCapability::ModelCatalog].state,
            CapabilityState::Supported
        );
        assert_eq!(
            declarations[&AgentCapability::Streaming].state,
            CapabilityState::Supported
        );
        assert_eq!(
            declarations[&AgentCapability::WorkspaceWrite].state,
            CapabilityState::Supported
        );
    }

    #[test]
    fn every_compiled_integration_has_provider_owned_setup_guidance() {
        let expected = [
            (
                ProviderKey::Claude,
                "npm install -g @anthropic-ai/claude-code",
                "Run `claude` and complete the browser sign-in prompt. If `ANTHROPIC_API_KEY` is configured, approve it when prompted.",
                "Run `claude --version` to check the install; `claude doctor` gives read-only install and settings diagnostics. AI Fuel separately runs `claude auth status` with a bounded timeout and discards its output.",
                "https://code.claude.com/docs/en/getting-started",
            ),
            (
                ProviderKey::Codex,
                "npm install -g @openai/codex",
                "Run `codex login` and complete the browser sign-in flow.",
                "Run `codex --version` to check the install. To inspect local sign-in state, run `codex login status` yourself; AI Fuel does not run auth commands.",
                "https://developers.openai.com/codex/auth",
            ),
            (
                ProviderKey::Copilot,
                "npm install -g @github/copilot",
                "Start `copilot`, then enter `/login` in its interactive UI.",
                "Run `copilot --version` to check the installed version; AI Fuel does not inspect Copilot sign-in state.",
                "https://docs.github.com/en/copilot/get-started/cli-quickstart",
            ),
            (
                ProviderKey::Gemini,
                "npm install -g @google/gemini-cli",
                "Start `gemini` and choose a documented sign-in method, such as Sign in with Google.",
                "Run `gemini --version` to check the install. Start `gemini` and complete the interactive auth selection to verify account access; AI Fuel does not inspect local credentials.",
                "https://geminicli.com/docs/get-started/",
            ),
            (
                ProviderKey::Antigravity,
                "macOS/Linux: `curl -fsSL https://antigravity.google/cli/install.sh | bash`; Windows PowerShell: `irm https://antigravity.google/cli/install.ps1 | iex`.",
                "Run `agy` interactively; follow first-launch sign-in, which may open a browser.",
                "No non-interactive version or authentication status command is documented. Start `agy` manually to check setup; AI Fuel does not launch it.",
                "https://antigravity.google/docs/cli/install",
            ),
        ];

        for (provider, install, login, check, documentation_url) in expected {
            let adapter = agent_run_adapters()
                .iter()
                .find(|adapter| adapter.provider() == provider)
                .expect("every built-in provider is registered");
            let guidance = adapter
                .setup_guidance()
                .unwrap_or_else(|| panic!("{provider} has no setup guidance"));
            assert_eq!(guidance.install, install, "{provider} install guidance");
            assert_eq!(guidance.login, login, "{provider} login guidance");
            assert_eq!(guidance.check, check, "{provider} check guidance");
            assert_eq!(
                guidance.documentation_url, documentation_url,
                "{provider} official setup source"
            );
        }
    }
}
