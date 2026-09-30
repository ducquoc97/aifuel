//! Declared capabilities for wire-protocol adapters.
//!
//! Mirrors `agent_execution::capabilities`: every `AgentCapability` is
//! declared with a reason, and declarations are honest - a wire adapter
//! claims only what the direct HTTP path genuinely does.

use aifuel_core::{
    AccessMode, AgentCapability, AgentCapabilityEvidence, AgentRunError, CapabilityState,
    IntegrationId, OutputFormat, RunRequest,
};
use std::collections::BTreeMap;

/// Capability pre-rejection. Everything the wire path cannot enforce is
/// refused here, before any request is built or sent.
pub(crate) fn reject_unsupported(
    integration: &IntegrationId,
    request: &RunRequest,
) -> Result<(), AgentRunError> {
    if matches!(
        request.access,
        AccessMode::WorkspaceWrite | AccessMode::Full
    ) {
        return Err(AgentRunError::InvalidRequest(format!(
            "{integration} cannot enforce {} access; wire \
             execution performs no provider-side tools",
            request.access.as_str()
        )));
    }
    if request.external_tools.is_some() {
        return Err(AgentRunError::InvalidRequest(format!(
            "{integration} cannot enforce an exact external MCP tool selection"
        )));
    }
    if request.effort.is_some() {
        return Err(AgentRunError::InvalidRequest(format!(
            "{integration} cannot report a verified effort setting"
        )));
    }
    if request.resume.is_some() {
        return Err(AgentRunError::InvalidRequest(format!(
            "{integration} does not support explicit session continuation"
        )));
    }
    if request.account.is_some() {
        return Err(AgentRunError::InvalidRequest(format!(
            "{integration} does not expose provider account selection"
        )));
    }
    if request.output == OutputFormat::Jsonl {
        return Err(AgentRunError::InvalidRequest(format!(
            "{integration} cannot provide verified JSONL output"
        )));
    }
    if request
        .model
        .as_deref()
        .is_none_or(|model| model.trim().is_empty())
    {
        return Err(AgentRunError::InvalidRequest(format!(
            "{integration} requires a model: the openai_chat Wire Api has no \
             provider-side default"
        )));
    }
    Ok(())
}

/// The wire adapter's capability declarations: prompt completion through
/// the Wire Api is genuinely supported, deltas stream to the owner, and
/// read-only access is honestly enforceable because AI Fuel executes no
/// provider-side tools. Everything else a run request can demand is
/// declared Unsupported rather than claimed.
pub(crate) fn declared_capabilities() -> BTreeMap<AgentCapability, AgentCapabilityEvidence> {
    [
        (
            AgentCapability::PromptCompletion,
            true,
            "direct prompt completion through the wire protocol",
        ),
        (
            AgentCapability::Streaming,
            true,
            "live answer streaming through server-sent events",
        ),
        (
            AgentCapability::ReadOnly,
            true,
            "read-only enforcement, since wire execution runs no provider-side tools",
        ),
        (
            AgentCapability::WorkspaceWrite,
            false,
            "workspace-write enforcement",
        ),
        (
            AgentCapability::ExternalMcpTools,
            false,
            "exact external MCP tool routing",
        ),
        (AgentCapability::Resume, false, "native session resume"),
        (
            AgentCapability::Effort,
            false,
            "model-specific effort selection",
        ),
        (
            AgentCapability::AccountSelection,
            false,
            "provider account selection",
        ),
        (
            AgentCapability::OrdinaryInput,
            false,
            "ordinary-input forwarding",
        ),
        (
            AgentCapability::PermissionApproval,
            false,
            "permission approval forwarding",
        ),
        (
            AgentCapability::StructuredOutput,
            false,
            "structured output mode",
        ),
        (
            AgentCapability::ModelCatalog,
            false,
            "provider model-catalog discovery",
        ),
    ]
    .into_iter()
    .map(|(capability, supported, name)| (capability, declared_flag(supported, name)))
    .collect()
}

fn declared_flag(supported: bool, capability: &str) -> AgentCapabilityEvidence {
    let (state, reason) = if supported {
        (
            CapabilityState::Supported,
            format!(
                "the compiled wire adapter declares {capability}; this is not live enforcement proof"
            ),
        )
    } else {
        (
            CapabilityState::Unsupported,
            format!("the compiled wire adapter does not expose {capability}"),
        )
    };
    AgentCapabilityEvidence { state, reason }
}
