//! RAII guards for one legacy run's runtime borrow: the provider session
//! the run opened, and the scratch directory a prompt-only run used.

use super::{ConsumerId, RuntimeExecutionAdapter, SessionId};
use std::path::PathBuf;

/// Closes the runtime session on every `run` exit path: no provider session
/// outlives the legacy run that created it.
pub(super) struct SessionGuard<'a> {
    pub(super) adapter: &'a RuntimeExecutionAdapter,
    pub(super) consumer: &'a ConsumerId,
    pub(super) session_id: SessionId,
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        self.adapter.close_session(&self.session_id, self.consumer);
    }
}

/// Removes the scratch directory a prompt-only run borrowed.
pub(super) struct ScratchDir(pub Option<PathBuf>);

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}
