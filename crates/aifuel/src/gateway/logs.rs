//! In-memory request log for the `/v1` surface. Stub: the implementing
//! agent fills in the bounded store and the `Entry` contract.

/// One completed `/v1` request for the dashboard log view.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Entry {
    pub(crate) ts_unix: u64,
    pub(crate) model: String,
    pub(crate) integration: Option<String>,
    pub(crate) status: u16,
    pub(crate) stream: bool,
    pub(crate) usage: Option<serde_json::Value>,
    pub(crate) error: Option<String>,
}

/// Record one completed request. Calls are added at each terminal outcome
/// in `chat`, `messages`, `responses`, and `completions`.
pub(crate) fn record(_entry: Entry) {}
