use super::helpers::{decode_cursor, encode_cursor, event_size};
use super::workers::shutdown_inner;
use super::*;

impl RunManager {
    /// Return current metadata for an owner-local run.
    pub fn get_run(&self, run_id: &str) -> Result<ManagedRun, RunManagementError> {
        self.record(run_id).map(|record| record.snapshot())
    }

    /// Read a non-consuming, bounded page of ordered events.
    pub fn read_events(
        &self,
        run_id: &str,
        cursor: Option<&str>,
        page_bytes: Option<usize>,
    ) -> Result<RunEvents, RunManagementError> {
        let record = self.record(run_id)?;
        let page_bytes = page_bytes.unwrap_or(DEFAULT_EVENT_PAGE_BYTES);
        if page_bytes == 0 || page_bytes > MAX_EVENT_PAGE_BYTES {
            return Err(RunManagementError::invalid_request(format!(
                "event page must be between 1 and {MAX_EVENT_PAGE_BYTES} bytes"
            )));
        }
        let cursor_sequence = cursor
            .map(|cursor| decode_cursor(run_id, cursor))
            .transpose()?;
        let metadata = record.metadata.lock().expect("run metadata mutex");
        let terminal = metadata.state.is_terminal();
        drop(metadata);

        let events = record.events.lock().expect("run events mutex");
        let earliest = events.events.front().map(|event| event.sequence);
        let latest = events.events.back().map(|event| event.sequence);
        if let Some(sequence) = cursor_sequence
            && latest.is_some_and(|latest| sequence > latest)
        {
            return Err(RunManagementError::invalid_cursor(
                "event cursor is newer than the retained event stream",
            ));
        }
        let gap = events.gap
            || cursor_sequence.is_some_and(|sequence| {
                earliest.is_some_and(|first| sequence.saturating_add(1) < first)
            });
        let after = cursor_sequence.unwrap_or(0);
        let mut selected = Vec::new();
        let mut used = 0usize;
        for event in events.events.iter().filter(|event| event.sequence > after) {
            let event_bytes = event_size(event);
            if !selected.is_empty() && used.saturating_add(event_bytes) > page_bytes {
                break;
            }
            selected.push(event.clone());
            used = used.saturating_add(event_bytes);
        }
        let next_cursor = selected.last().and_then(|last| {
            events
                .events
                .iter()
                .any(|event| event.sequence > last.sequence)
                .then(|| encode_cursor(run_id, last.sequence))
        });
        Ok(RunEvents {
            events: selected,
            next_cursor,
            gap,
            terminal,
        })
    }

    /// Return a non-consuming bounded result projection. Active runs have no
    /// provider status yet, but their state and metadata remain inspectable.
    pub fn get_result(&self, run_id: &str) -> Result<ManagedRunResult, RunManagementError> {
        let record = self.record(run_id)?;
        let mut result = record.result_snapshot();
        if let Some(store) = self
            .inner
            .content_store
            .lock()
            .expect("content store mutex")
            .as_ref()
            && let Ok(Some(content)) = store.load(run_id)
        {
            result.output = content.output;
            result.error = content.error;
            result.diagnostics = content.diagnostics;
            result.content_available = true;
        }
        Ok(result)
    }
}

impl RunManager {
    /// Request cancellation. A terminal result is never rewritten.
    pub fn cancel_run(&self, run_id: &str) -> Result<ManagedRun, RunManagementError> {
        let record = self.record(run_id)?;
        let should_cancel = {
            let mut metadata = record.metadata.lock().expect("run metadata mutex");
            if metadata.state.is_terminal() {
                false
            } else {
                metadata.state = RunState::Cancelling;
                true
            }
        };
        if should_cancel {
            #[cfg(any(unix, windows))]
            let pending = record
                .pending_input
                .lock()
                .expect("pending input mutex")
                .take();
            #[cfg(not(any(unix, windows)))]
            let _ = record
                .pending_input
                .lock()
                .expect("pending input mutex")
                .take();
            #[cfg(any(unix, windows))]
            if let Some(pending) =
                pending.filter(|pending| pending.kind == RunInputKind::Permission)
            {
                self.remove_local_approval_pending(&pending.run_id, &pending.input_id);
            }
            record.cancellation.cancel();
            record.interaction_changed.notify_all();
            self.push_event(
                &record,
                RunEventKind::StateChanged,
                Some("cancelling".to_owned()),
            );
        }
        Ok(record.snapshot())
    }

    /// Cancel active runs and join every owned worker before returning.
    pub fn shutdown(&self) {
        shutdown_inner(&self.inner);
    }
}
