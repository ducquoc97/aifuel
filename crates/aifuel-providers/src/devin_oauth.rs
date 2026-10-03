//! The `devin:oauth` integration identity - deliberately not executable.
//!
//! The `devin` CLI credential (`~/.local/share/devin/credentials.toml`)
//! holds a `windsurf_api_key` plus service URLs. That key is scoped to the
//! provider's seat/language-server RPC surface, which has no documented
//! request contract for direct prompt completion. The documented Devin
//! API (`api.devin.ai`) authenticates with separate `cog_` service keys an
//! organization id - neither of which the CLI credential file contains -
//! and exposes session orchestration, not direct inference. Presenting
//! `devin:oauth` as runnable would be a claim the credentials cannot back,
//! so this adapter registers the identity (it shows up honestly in
//! integration listings as unavailable) and fails every run with an
//! explicit explanation instead of silently routing onto the `devin` CLI.

use crate::agent_execution::ExecutionCapabilities;
use crate::oauth_http;
use aifuel_core::{
    AgentExecutionAdapter, AgentIntegrationInfo, AgentPresenceEvidence, AgentPresenceState,
    AgentRunError, AgentRunOutputHandler, AgentSetupGuidance, IntegrationId, ProviderId,
    ProviderKey, RunCancellationToken, RunRequest, RunResult,
};
use std::collections::BTreeMap;

/// Why `devin:oauth` cannot execute, restated to every caller that asks.
const SKIP_REASON: &str = "devin:oauth is not executable: the devin CLI \
     credential (~/.local/share/devin/credentials.toml) holds a \
     windsurf_api_key scoped to the provider's private RPC surface with no \
     documented direct-inference contract, and the documented Devin API \
     (api.devin.ai) needs a separate cog_ service key the CLI credential \
     does not contain; select the `devin` CLI integration instead";

/// The compiled `devin:oauth` adapter.
pub(crate) static ADAPTER: DevinOAuthAdapter = DevinOAuthAdapter;

pub(crate) struct DevinOAuthAdapter;

impl AgentExecutionAdapter for DevinOAuthAdapter {
    fn integration(&self) -> IntegrationId {
        IntegrationId::new("devin:oauth")
    }

    fn provider(&self) -> ProviderId {
        ProviderId::from(ProviderKey::Devin)
    }

    fn setup_guidance(&self) -> Option<AgentSetupGuidance> {
        Some(AgentSetupGuidance {
            install: "macOS/Linux/WSL: `curl -fsSL https://cli.devin.ai/install.sh | bash`; Windows PowerShell: `irm https://static.devin.ai/cli/setup.ps1 | iex`.",
            login: "Run `devin auth login` and complete the browser sign-in prompt.",
            check: "Run `devin --version` to check the install. AI Fuel separately runs `devin auth status` with a bounded timeout and discards its output.",
            documentation_url: "https://docs.devin.ai/cli",
        })
    }

    fn declared_agent_capabilities(
        &self,
    ) -> BTreeMap<aifuel_core::AgentCapability, aifuel_core::AgentCapabilityEvidence> {
        // Nothing is declared: an adapter that can never run must not
        // advertise capabilities it will never exercise. `ReadOnly` is
        // explicitly Unsupported rather than merely unestablished.
        ExecutionCapabilities::new(false, false, false, false)
            .with_unsupported_read_only()
            .evidence()
    }

    fn agent_info(&self) -> AgentIntegrationInfo {
        AgentIntegrationInfo::from_inspection(
            self.provider(),
            self.integration(),
            // `Absent` - not `Unknown` - so `integrations.list` reports the
            // integration as unavailable rather than merely unauthenticated.
            AgentPresenceEvidence {
                state: AgentPresenceState::Absent,
                reason: "no documented direct-inference API exists for the \
                         devin CLI credential; the integration lists as \
                         unavailable, not as a broken sign-in"
                    .to_owned(),
            },
            oauth_http::compiled_version(),
            oauth_http::unprobed_authentication(),
            self.declared_agent_capabilities(),
        )
        .with_setup_guidance(self.setup_guidance())
    }

    fn validate(&self, request: &RunRequest) -> Result<(), AgentRunError> {
        if request.integration != self.integration() {
            return Err(AgentRunError::UnsupportedIntegration(
                request.integration.clone(),
            ));
        }
        Err(AgentRunError::InvalidRequest(SKIP_REASON.to_owned()))
    }

    fn execute(
        &self,
        request: &RunRequest,
        _cancellation: &RunCancellationToken,
    ) -> Result<RunResult, AgentRunError> {
        self.validate(request)?;
        unreachable!("validate always fails");
    }

    fn execute_with_output_handler(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
        _output_handler: &dyn AgentRunOutputHandler,
    ) -> Result<RunResult, AgentRunError> {
        self.execute(request, cancellation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aifuel_core::{AccessMode, OutputFormat};

    fn request() -> RunRequest {
        RunRequest {
            integration: IntegrationId::new("devin:oauth"),
            model: Some("devin".to_owned()),
            effort: None,
            external_tools: None,
            account: None,
            prompt: "say hello".to_owned(),
            output: OutputFormat::Text,
            working_directory: None,
            access: AccessMode::ReadOnly,
            resume: None,
            timeout: None,
            env: Default::default(),
            interaction_handler: None,
        }
    }

    #[test]
    fn the_integration_is_identity_only_and_never_executes() {
        match ADAPTER
            .execute(&request(), &RunCancellationToken::new())
            .err()
            .expect("execution is skipped")
        {
            AgentRunError::InvalidRequest(message) => {
                assert!(message.contains("not executable"), "{message}");
                assert!(message.contains("devin"), "{message}");
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn a_foreign_integration_is_still_rejected_as_foreign() {
        let mut request = request();
        request.integration = IntegrationId::new("devin");
        assert!(matches!(
            ADAPTER.validate(&request),
            Err(AgentRunError::UnsupportedIntegration(_))
        ));
    }

    #[test]
    fn the_listing_reports_unavailable_without_inventing_capabilities() {
        let info = ADAPTER.agent_info();
        assert_eq!(info.native_presence.state, AgentPresenceState::Absent);
        for capability in aifuel_core::AgentCapability::ALL {
            assert_eq!(
                info.capabilities[&capability].declared.state,
                aifuel_core::CapabilityState::Unsupported,
                "{capability:?} must be declared Unsupported by a skipped adapter"
            );
        }
    }
}
