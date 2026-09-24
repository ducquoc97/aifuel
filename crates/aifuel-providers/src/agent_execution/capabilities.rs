//! Declared Agent Integration capabilities and their inspection evidence.

use aifuel_core::{AgentCapability, AgentCapabilityEvidence, CapabilityState};
use std::collections::BTreeMap;

pub(crate) struct ExecutionCapabilities {
    pub(super) supports_resume: bool,
    pub(super) supports_account_selection: bool,
    pub(super) supports_workspace_write: bool,
    pub(super) supports_jsonl: bool,
    pub(super) supports_external_tools: bool,
    pub(super) supports_effort: bool,
    supports_model_catalog: bool,
    supports_streaming: bool,
    read_only: CapabilityState,
    supports_ordinary_input: bool,
    supports_permission_approval: bool,
}

impl ExecutionCapabilities {
    pub(crate) const fn new(
        supports_resume: bool,
        supports_account_selection: bool,
        supports_workspace_write: bool,
        supports_jsonl: bool,
    ) -> Self {
        Self {
            supports_resume,
            supports_account_selection,
            supports_workspace_write,
            supports_jsonl,
            supports_external_tools: false,
            supports_effort: false,
            supports_model_catalog: false,
            supports_streaming: false,
            read_only: CapabilityState::Unknown,
            supports_ordinary_input: false,
            supports_permission_approval: false,
        }
    }

    pub(crate) const fn with_external_tools(mut self) -> Self {
        self.supports_external_tools = true;
        self
    }

    pub(crate) const fn with_effort(mut self) -> Self {
        self.supports_effort = true;
        self
    }

    pub(crate) const fn with_model_catalog(mut self) -> Self {
        self.supports_model_catalog = true;
        self
    }

    pub(crate) const fn with_streaming(mut self) -> Self {
        self.supports_streaming = true;
        self
    }

    pub(crate) const fn with_read_only(mut self) -> Self {
        self.read_only = CapabilityState::Supported;
        self
    }

    pub(crate) const fn with_unsupported_read_only(mut self) -> Self {
        self.read_only = CapabilityState::Unsupported;
        self
    }

    pub(super) const fn supports_read_only(&self) -> bool {
        matches!(self.read_only, CapabilityState::Supported)
    }

    pub(crate) const fn with_ordinary_input(mut self) -> Self {
        self.supports_ordinary_input = true;
        self
    }

    pub(crate) const fn with_permission_approval(mut self) -> Self {
        self.supports_permission_approval = true;
        self
    }

    pub(super) fn evidence(&self) -> BTreeMap<AgentCapability, AgentCapabilityEvidence> {
        [
            (
                AgentCapability::ModelCatalog,
                declared_flag(
                    self.supports_model_catalog,
                    "provider model-catalog discovery",
                ),
            ),
            (
                AgentCapability::Streaming,
                declared_flag(self.supports_streaming, "live answer streaming"),
            ),
            (
                AgentCapability::ReadOnly,
                declared_state(self.read_only, "read-only enforcement"),
            ),
            (
                AgentCapability::WorkspaceWrite,
                declared_flag(self.supports_workspace_write, "workspace-write enforcement"),
            ),
            (
                AgentCapability::ExternalMcpTools,
                declared_flag(
                    self.supports_external_tools,
                    "exact external MCP tool routing",
                ),
            ),
            (
                AgentCapability::Resume,
                declared_flag(self.supports_resume, "native session resume"),
            ),
            (
                AgentCapability::Effort,
                declared_flag(self.supports_effort, "model-specific effort selection"),
            ),
            (
                AgentCapability::AccountSelection,
                declared_flag(
                    self.supports_account_selection,
                    "provider account selection",
                ),
            ),
            (
                AgentCapability::OrdinaryInput,
                declared_flag(self.supports_ordinary_input, "ordinary-input forwarding"),
            ),
            (
                AgentCapability::PermissionApproval,
                declared_flag(
                    self.supports_permission_approval,
                    "permission approval forwarding",
                ),
            ),
            (
                AgentCapability::StructuredOutput,
                declared_flag(self.supports_jsonl, "structured output mode"),
            ),
        ]
        .into_iter()
        .collect()
    }
}

fn declared_flag(supported: bool, capability: &str) -> AgentCapabilityEvidence {
    let (state, reason) = if supported {
        (
            CapabilityState::Supported,
            format!(
                "the compiled provider adapter declares {capability}; this is not live enforcement proof"
            ),
        )
    } else {
        (
            CapabilityState::Unsupported,
            format!("the compiled provider adapter does not expose {capability}"),
        )
    };
    AgentCapabilityEvidence { state, reason }
}

fn declared_state(state: CapabilityState, capability: &str) -> AgentCapabilityEvidence {
    let reason = match state {
        CapabilityState::Supported => format!(
            "the compiled provider adapter declares {capability}; this is not live enforcement proof"
        ),
        CapabilityState::Unsupported => {
            format!("the compiled provider adapter does not expose {capability}")
        }
        CapabilityState::Unknown => {
            format!("the compiled provider adapter does not establish {capability}")
        }
    };
    AgentCapabilityEvidence { state, reason }
}
