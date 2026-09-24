//! Provider-owned evidence for listing compiled Agent Integrations.

use crate::ProviderKey;
use crate::status::CapabilityState;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCapability {
    ModelCatalog,
    Streaming,
    ReadOnly,
    WorkspaceWrite,
    ExternalMcpTools,
    Resume,
    Effort,
    AccountSelection,
    OrdinaryInput,
    PermissionApproval,
    StructuredOutput,
}

impl AgentCapability {
    pub const ALL: [Self; 11] = [
        Self::ModelCatalog,
        Self::Streaming,
        Self::ReadOnly,
        Self::WorkspaceWrite,
        Self::ExternalMcpTools,
        Self::Resume,
        Self::Effort,
        Self::AccountSelection,
        Self::OrdinaryInput,
        Self::PermissionApproval,
        Self::StructuredOutput,
    ];

    fn display_name(self) -> &'static str {
        match self {
            Self::ModelCatalog => "provider model catalog discovery",
            Self::Streaming => "live answer streaming",
            Self::ReadOnly => "read-only effect enforcement",
            Self::WorkspaceWrite => "workspace-write effect enforcement",
            Self::ExternalMcpTools => "exact external MCP tool routing",
            Self::Resume => "native session resume",
            Self::Effort => "model-specific effort selection",
            Self::AccountSelection => "provider account selection",
            Self::OrdinaryInput => "ordinary-input forwarding",
            Self::PermissionApproval => "local permission approval forwarding",
            Self::StructuredOutput => "structured output mode",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPresenceState {
    Present,
    Absent,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentPresenceEvidence {
    pub state: AgentPresenceState,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentVersionEvidence {
    pub version: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAuthenticationState {
    Authenticated,
    Unauthenticated,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentAuthenticationEvidence {
    pub state: AgentAuthenticationState,
    pub reason: String,
}

/// Provider-owned instructions for a person to install, authenticate, and
/// verify the native integration. These are guidance only; listing never
/// executes the commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AgentSetupGuidance {
    pub install: &'static str,
    pub login: &'static str,
    pub check: &'static str,
    pub documentation_url: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentCapabilityEvidence {
    pub state: CapabilityState,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentCapabilityAssessment {
    /// The capability declared by AI Fuel's compiled adapter. This is not a
    /// claim that the native integration currently enforces it.
    pub declared: AgentCapabilityEvidence,
    /// Evidence for the installed version and active provider context.
    pub current: AgentCapabilityEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentIntegrationInfo {
    pub provider: ProviderKey,
    pub integration: String,
    pub native_presence: AgentPresenceEvidence,
    pub native_version: AgentVersionEvidence,
    pub native_authentication: AgentAuthenticationEvidence,
    pub setup_guidance: Option<AgentSetupGuidance>,
    pub capabilities: BTreeMap<AgentCapability, AgentCapabilityAssessment>,
}

impl AgentIntegrationInfo {
    pub fn unknown(provider: ProviderKey, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        let native_presence = AgentPresenceEvidence {
            state: AgentPresenceState::Unknown,
            reason: reason.clone(),
        };
        let native_version = AgentVersionEvidence {
            version: None,
            reason: reason.clone(),
        };
        let native_authentication = AgentAuthenticationEvidence {
            state: AgentAuthenticationState::Unknown,
            reason: "authentication is not inspected during listing to avoid reading local credentials or starting an auth flow; use provider setup guidance for manual login and checks".to_owned(),
        };
        let capabilities = AgentCapability::ALL
            .into_iter()
            .map(|capability| {
                let declared = AgentCapabilityEvidence {
                    state: CapabilityState::Unknown,
                    reason: format!(
                        "{} declaration is not available from the registered adapter",
                        capability.display_name()
                    ),
                };
                let current = AgentCapabilityEvidence {
                    state: CapabilityState::Unknown,
                    reason: format!(
                        "{} is unknown because native version, account, or permission evidence was not established",
                        capability.display_name()
                    ),
                };
                (
                    capability,
                    AgentCapabilityAssessment { declared, current },
                )
            })
            .collect();
        Self {
            provider,
            integration: "compiled".to_owned(),
            native_presence,
            native_version,
            native_authentication,
            setup_guidance: None,
            capabilities,
        }
    }

    /// Build an integration listing from a provider-owned native inspection
    /// and explicit adapter declarations. Declarations never promote runtime
    /// evidence to supported.
    pub fn from_inspection(
        provider: ProviderKey,
        native_presence: AgentPresenceEvidence,
        native_version: AgentVersionEvidence,
        native_authentication: AgentAuthenticationEvidence,
        declared_capabilities: impl IntoIterator<Item = (AgentCapability, AgentCapabilityEvidence)>,
    ) -> Self {
        let declared_capabilities = declared_capabilities
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        let capabilities = AgentCapability::ALL
            .into_iter()
            .map(|capability| {
                let declared = declared_capabilities
                    .get(&capability)
                    .cloned()
                    .unwrap_or_else(|| AgentCapabilityEvidence {
                        state: CapabilityState::Unknown,
                        reason: format!(
                            "{} is not declared by the provider adapter",
                            capability.display_name()
                        ),
                    });
                let current = match (native_presence.state, declared.state) {
                    (_, CapabilityState::Unsupported) => AgentCapabilityEvidence {
                        state: CapabilityState::Unsupported,
                        reason: format!(
                            "the compiled adapter does not expose {}",
                            capability.display_name()
                        ),
                    },
                    (AgentPresenceState::Absent, _) => AgentCapabilityEvidence {
                        state: CapabilityState::Unsupported,
                        reason: format!(
                            "{} is unavailable because the native executable is absent",
                            capability.display_name()
                        ),
                    },
                    _ => AgentCapabilityEvidence {
                        state: CapabilityState::Unknown,
                        reason: current_capability_reason(capability),
                    },
                };
                (capability, AgentCapabilityAssessment { declared, current })
            })
            .collect();
        Self {
            provider,
            integration: "compiled".to_owned(),
            native_presence,
            native_version,
            native_authentication,
            setup_guidance: None,
            capabilities,
        }
    }

    /// Attach provider-owned user guidance without changing inspected state.
    pub fn with_setup_guidance(mut self, setup_guidance: Option<AgentSetupGuidance>) -> Self {
        self.setup_guidance = setup_guidance;
        self
    }
}

fn current_capability_reason(capability: AgentCapability) -> String {
    match capability {
        AgentCapability::ModelCatalog => {
            "current catalog, account entitlement, and installed-version evidence was not refreshed"
                .to_owned()
        }
        AgentCapability::Streaming => {
            "the installed version's live streaming behavior was not observed".to_owned()
        }
        AgentCapability::ReadOnly => {
            "no current-version denied-effect test established read-only enforcement".to_owned()
        }
        AgentCapability::WorkspaceWrite => {
            "no current-version workspace-boundary test established write enforcement".to_owned()
        }
        AgentCapability::ExternalMcpTools => {
            "the current connected server and tool inventory was not observed".to_owned()
        }
        AgentCapability::Resume => {
            "the installed version's same-provider resume behavior was not observed".to_owned()
        }
        AgentCapability::Effort => {
            "current model-specific effort support and effective effort were not observed"
                .to_owned()
        }
        AgentCapability::AccountSelection => {
            "provider account context was not inspected; no login or credential probe was run"
                .to_owned()
        }
        AgentCapability::OrdinaryInput => {
            "ordinary-input forwarding was not exercised for the installed version".to_owned()
        }
        AgentCapability::PermissionApproval => {
            "permission request and enforcement behavior was not observed for the active account"
                .to_owned()
        }
        AgentCapability::StructuredOutput => {
            "structured output behavior was not observed for the installed version".to_owned()
        }
    }
}
