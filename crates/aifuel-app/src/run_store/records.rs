//! Row payloads exchanged between the run manager and the store, and the
//! projections rebuilt from persisted rows.

use aifuel_core::{
    ManagedRun, ManagedRunResult, ProviderKey, RUN_MANAGEMENT_SCHEMA_VERSION, RunEvent, RunState,
    RunStatus,
};

/// Metadata recorded when an Agent Run is accepted. The prompt is
/// intentionally absent; prompts are content and are never persisted.
pub(crate) struct StartedRun {
    pub run_id: String,
    pub provider: ProviderKey,
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
    pub provider: ProviderKey,
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
            provider: self.provider,
            requested_model: self.requested_model.clone(),
            requested_effort: self.requested_effort.clone(),
            external_tools: self.external_tools.clone(),
            created_at: self.created_at,
            completed_at: self.completed_at,
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
            provider: self.provider,
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
