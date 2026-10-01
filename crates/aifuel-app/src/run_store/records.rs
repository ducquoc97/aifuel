//! Row payloads exchanged between the run manager and the store, and the
//! projections rebuilt from persisted rows.

use aifuel_core::{
    AgentEvent, Effort, IntegrationId, ManagedRun, ManagedRunResult, ProviderId,
    RUN_MANAGEMENT_SCHEMA_VERSION, RunEvent, RunState, RunStatus, Seq, SessionId, SessionStatus,
    TokenUsage,
};
use std::path::PathBuf;

/// Metadata recorded when an Agent Run is accepted. The prompt is
/// intentionally absent; prompts are content and are never persisted.
pub(crate) struct StartedRun {
    pub run_id: String,
    /// The upstream provider the selected integration executes against.
    pub provider: ProviderId,
    /// The configured integration the run was routed through.
    pub integration: IntegrationId,
    pub created_at: f64,
    pub working_directory: Option<String>,
    pub requested_model: Option<String>,
    pub requested_effort: Option<String>,
    pub external_tools: Option<Vec<String>>,
    pub output_format: Option<String>,
    pub access: Option<String>,
    pub timeout_seconds: Option<u64>,
    /// The account context the request asked for, as opposed to the account
    /// the provider reported at completion.
    pub requested_account: Option<String>,
    pub resume: Option<String>,
    /// Native integration version probed at run start, when the adapter's
    /// version probe reported one.
    pub integration_version: Option<String>,
    /// Host platform label the run started under.
    pub platform: String,
}

/// The terminal metadata written once when a run completes.
pub(crate) struct CompletedRun {
    pub state: RunState,
    pub status: Option<RunStatus>,
    pub completed_at: f64,
    pub effective_model: Option<String>,
    pub effective_effort: Option<String>,
    pub session_id: Option<String>,
    pub local_session_id: Option<String>,
    pub exit_code: Option<i32>,
    /// Stable failure category such as `provider_failed`, `agent_unavailable`,
    /// `invalid_request`, or `owner_exited`, when the run did not succeed.
    pub closed_reason: Option<String>,
    /// The account context reported by the provider, when it reports one.
    pub reported_account: Option<String>,
    /// Token accounting the provider reported, when the wire protocol
    /// returned it.
    pub usage: Option<TokenUsage>,
    /// Whether the run's content payloads were persisted to the content
    /// store. Owner-held memory content does not count: after the owner
    /// exits, only persisted payloads remain available.
    pub content_available: bool,
    pub output_bytes: usize,
    pub diagnostics_bytes: usize,
    pub output_truncated: bool,
    pub diagnostics_truncated: bool,
}

/// One persisted terminal run row.
pub(crate) struct StoredRun {
    pub run_id: String,
    /// Process id of the owner that wrote the row. Used to decide whether the
    /// row is orphaned history (owner exited) or another live owner's record,
    /// which stays hidden from this connection.
    pub owner_pid: i64,
    /// The upstream provider the selected integration executed against.
    pub provider: ProviderId,
    /// The configured integration the run was routed through.
    pub integration: IntegrationId,
    pub state: RunState,
    pub status: Option<RunStatus>,
    pub requested_model: Option<String>,
    pub requested_effort: Option<String>,
    pub effective_model: Option<String>,
    pub effective_effort: Option<String>,
    pub external_tools: Option<Vec<String>>,
    pub session_id: Option<String>,
    pub local_session_id: Option<String>,
    pub exit_code: Option<i32>,
    pub closed_reason: Option<String>,
    /// Account context the provider reported at completion.
    pub reported_account: Option<String>,
    /// Token accounting the provider reported at completion.
    pub usage: Option<TokenUsage>,
    /// Native integration version the run started under.
    pub integration_version: Option<String>,
    /// Host platform label the run started under.
    pub platform: Option<String>,
    pub created_at: f64,
    pub completed_at: Option<f64>,
    pub content_available: bool,
    pub output_bytes: usize,
    pub diagnostics_bytes: usize,
    pub output_truncated: bool,
    pub diagnostics_truncated: bool,
}

impl StoredRun {
    /// Rebuild the public metadata snapshot. Pending inputs died with their
    /// owner and are never resurrected from history.
    pub fn managed_run(&self) -> ManagedRun {
        ManagedRun {
            schema_version: RUN_MANAGEMENT_SCHEMA_VERSION,
            run_id: self.run_id.clone(),
            state: self.state,
            integration: self.integration.clone(),
            provider: self.provider.clone(),
            requested_model: self.requested_model.clone(),
            requested_effort: self.requested_effort.clone(),
            external_tools: self.external_tools.clone(),
            created_at: self.created_at,
            completed_at: self.completed_at,
            integration_version: self.integration_version.clone(),
            platform: self.platform.clone(),
            content_available: self.content_available,
            pending_input: None,
        }
    }

    /// Rebuild the public result projection without content payloads; the
    /// caller overlays retained content the same way it does for live runs.
    pub fn result(&self) -> ManagedRunResult {
        ManagedRunResult {
            schema_version: RUN_MANAGEMENT_SCHEMA_VERSION,
            run_id: self.run_id.clone(),
            state: self.state,
            integration: self.integration.clone(),
            provider: self.provider.clone(),
            requested_model: self.requested_model.clone(),
            requested_effort: self.requested_effort.clone(),
            effective_model: self.effective_model.clone(),
            effective_effort: self.effective_effort.clone(),
            local_session_id: self.local_session_id.clone(),
            session_id: self.session_id.clone(),
            status: self.status,
            closed_reason: self.closed_reason.clone(),
            account_id: self.reported_account.clone(),
            exit_code: self.exit_code,
            output: None,
            error: None,
            diagnostics: None,
            usage: self.usage.clone(),
            content_available: self.content_available,
            output_truncated: self.output_truncated,
            diagnostics_truncated: self.diagnostics_truncated,
            output_bytes: self.output_bytes,
            diagnostics_bytes: self.diagnostics_bytes,
        }
    }
}

/// A page of persisted events plus the stream's highest sequence.
pub(crate) struct StoredEventPage {
    pub events: Vec<RunEvent>,
    pub latest_sequence: u64,
    pub has_more: bool,
}

/// One persisted Agent Session row: the read-model projection the Session
/// Event Log maintains beside the raw `session_events` sequence.
pub struct StoredAgentSession {
    pub session_id: SessionId,
    /// The Provider Integration the session is bound to.
    pub integration: IntegrationId,
    /// The session's current model selection. `None` means no selection has
    /// been recorded yet; it does not claim a provider default.
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub cwd: PathBuf,
    pub status: SessionStatus,
    /// The provider resume cursor persisted where the adapter supports
    /// resume, read back by the startup reconcile.
    pub resume_cursor: Option<String>,
    /// The process-instance owner id that created the row, or `None` for
    /// rows written before owner scoping. Internal ownership marker; the
    /// wire payload never carries it.
    pub owner: Option<String>,
    /// The exact AI Fuel Gateway tool list `session.create` declared,
    /// persisted so a startup resume redeclares the same enforcement.
    /// Empty means no tool restriction was asked for.
    pub external_tools: Vec<String>,
}

/// One bounded page from a session's event log plus the log's head.
pub struct ReplayPage {
    pub events: Vec<AgentEvent>,
    /// The newest sequence the Session Event Log assigned for the session.
    pub head_seq: Seq,
    /// Events past `after_seq` remained beyond the page bounds. Per the
    /// contract's replay-to-snapshot fallback, the subscribe path answers a
    /// truncated replay with a fresh `SessionSnapshot` instead.
    pub truncated: bool,
}
