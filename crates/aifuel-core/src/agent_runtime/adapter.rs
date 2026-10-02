//! The in-process boundary between the runtime facade and one provider
//! adapter. One adapter wraps one provider protocol - stream formats,
//! JSON-RPC servers, HTTP APIs - never terminal output.

use crate::{
    AccessMode, AgentEventKind, AgentRuntimeError, ApprovalDecision, CheckpointId, Integration,
    ModelDescriptor, ModelSelection, RequestId, RunId, SessionId, UserInput,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Honest capability declarations for one provider adapter.
///
/// An adapter that cannot surface a feature declares it `false`; hosts hide
/// the affordance rather than fake it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterCapabilities {
    /// Live `message.delta` and `message.completed` streams.
    pub streaming: bool,
    /// Provider-side continuation of an interrupted session.
    pub resume: bool,
    /// Typed Approval Requests and answers.
    pub approvals: bool,
    /// Per-run Checkpoints on hidden git refs.
    pub checkpoints: bool,
    /// Model-specific effort selection.
    pub effort: bool,
    /// Image attachments on `UserInput`.
    pub images: bool,
    /// `todos.updated` task-list events.
    pub todos: bool,
    /// Exact AI Fuel Gateway tool names on `session.create`: an adapter
    /// declares `true` only when it can enforce the requested snapshot, and
    /// the runtime refuses tools the adapter cannot enforce.
    #[serde(default)]
    pub external_tools: bool,
}

/// Options for [`AgentAdapter::start`], mirroring `session.create`.
#[derive(Clone, PartialEq, Serialize)]
pub struct StartOptions {
    pub cwd: PathBuf,
    pub selection: ModelSelection,
    /// The access policy the Host Application declared for the session. The
    /// runtime enforces it but does not invent authorization.
    pub access: AccessMode,
    /// The persisted provider resume cursor when startup reconcile
    /// reattaches a session that survived a runtime restart. `None` opens a
    /// fresh provider session. Adapters that do not declare `resume` ignore
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_cursor: Option<String>,
    /// The exact AI Fuel Gateway tool names the host asked the session to
    /// enforce. Empty means none requested; adapters that do not declare
    /// `external_tools` never see this populated - the facade refuses the
    /// session first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external_tools: Vec<String>,
    /// The process environment overlay the selected Provider Integration
    /// instance resolved, applied to the provider process at spawn. Values
    /// can hold Credential Store material, so the map is never serialized,
    /// persisted, or printed - `Debug` reports variable names only.
    #[serde(skip)]
    pub env: std::collections::BTreeMap<String, String>,
}

impl std::fmt::Debug for StartOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StartOptions")
            .field("cwd", &self.cwd)
            .field("selection", &self.selection)
            .field("access", &self.access)
            .field("resume_cursor", &self.resume_cursor)
            .field("external_tools", &self.external_tools)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// An adapter-owned handle to one live Agent Session.
///
/// The adapter keys its internal provider state by `session_id`; the handle
/// itself carries no secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentSessionHandle {
    pub session_id: SessionId,
    /// The provider-native session identity or resume cursor, when the
    /// provider protocol exposes one. Persisted so a runtime restart can
    /// attempt provider-side continuation where `resume` is declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session: Option<String>,
}

/// The pull stream of typed event payloads an adapter emits for one session.
///
/// `next` blocks until the adapter produces a payload or the stream closes.
/// The runtime stamps `session_id`, `seq`, and `ts` onto each payload before
/// writing the Session Event Log, so adapters never invent sequence numbers.
/// A `std::sync::mpsc::Receiver` satisfies `Iterator`, so an adapter may box
/// a channel receiver directly.
pub type AgentEventStream = Box<dyn Iterator<Item = AgentEventKind> + Send>;

/// Provider-specific implementation behind the runtime contract.
///
/// One adapter wraps one provider protocol. It serves exactly the
/// integrations it is compiled for and never falls back to another model or
/// credential. Implementations own any provider process or connection they
/// start, and never emit credential material in events.
pub trait AgentAdapter: Send + Sync {
    /// Honest declarations of what this adapter can surface.
    fn capabilities(&self) -> AdapterCapabilities;

    /// Resolve one selection to a merged descriptor before use.
    ///
    /// `session.create` and `model.select` resolve first; a selection that
    /// is not `ready` fails with `invalid_selection`, never a silent
    /// fallback to another model or credential.
    fn resolve(&self, selection: &ModelSelection) -> Result<ModelDescriptor, AgentRuntimeError>;

    /// List the integration's models, merging the Advertised Model catalog
    /// with Account Entitlement and Execution Availability for its account.
    fn list_models(
        &self,
        integration: &Integration,
    ) -> Result<Vec<ModelDescriptor>, AgentRuntimeError>;

    /// Start a provider session and return its handle.
    fn start(
        &self,
        integration: &Integration,
        options: StartOptions,
    ) -> Result<AgentSessionHandle, AgentRuntimeError>;

    /// Send user input as one Agent Run and return its id.
    fn send(
        &self,
        handle: &AgentSessionHandle,
        input: UserInput,
    ) -> Result<RunId, AgentRuntimeError>;

    /// Request cancellation of one in-flight Agent Run.
    fn cancel(&self, handle: &AgentSessionHandle, run: RunId) -> Result<(), AgentRuntimeError>;

    /// Answer one pending Approval Request. A second answer loses to the
    /// first with `already_resolved`.
    fn answer(
        &self,
        handle: &AgentSessionHandle,
        request: RequestId,
        decision: ApprovalDecision,
    ) -> Result<(), AgentRuntimeError>;

    /// The live event payload stream for this session.
    fn events(&self, handle: &AgentSessionHandle) -> AgentEventStream;

    /// Record a hidden git ref for one completed workspace-mutating run.
    fn checkpoint(
        &self,
        handle: &AgentSessionHandle,
        run: RunId,
    ) -> Result<CheckpointId, AgentRuntimeError>;

    /// Reset the workspace to one recorded Checkpoint. Invoked only through
    /// the explicit `checkpoint.restore` command because it discards working
    /// state.
    fn restore_checkpoint(
        &self,
        handle: &AgentSessionHandle,
        checkpoint: CheckpointId,
    ) -> Result<(), AgentRuntimeError>;

    /// Stop the provider session and release the handle.
    fn stop(&self, handle: AgentSessionHandle) -> Result<(), AgentRuntimeError>;
}
