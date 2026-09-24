use super::*;

pub(super) struct ManagedRunOutputHandler {
    pub(super) record: Weak<RunRecord>,
}

impl std::fmt::Debug for ManagedRunOutputHandler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ManagedRunOutputHandler")
    }
}

impl AgentRunOutputHandler for ManagedRunOutputHandler {
    fn on_output(&self, delta: &str) {
        if let Some(record) = self.record.upgrade() {
            record.capture_output(delta);
        }
    }
}
