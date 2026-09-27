use super::helpers::{decode_cursor, encode_cursor, event_size};
use super::workers::shutdown_inner;
use super::*;

impl RunManager {
    /// Return current metadata for an owner-local run, falling back to
    /// persisted terminal history when the owning process no longer holds the
    /// record.
    pub fn get_run(&self, run_id: &str) -> Result<ManagedRun, RunManagementError> {
        match self.record(run_id) {
            Ok(record) => Ok(record.snapshot()),
            Err(error) => self
                .stored_run(run_id)?
                .map(|run| run.managed_run())
                .ok_or(error),
        }
    }

    /// Read a non-consuming, bounded page of ordered events.
    pub fn read_events(
        &self,
        run_id: &str,
        cursor: Option<&str>,
        page_bytes: Option<usize>,
    ) -> Result<RunEvents, RunManagementError> {
        let Ok(record) = self.record(run_id) else {
            return self.read_stored_events(run_id, cursor, page_bytes);
        };
        let page_bytes = checked_page_bytes(page_bytes)?;
        let cursor_sequence = cursor
            .map(|cursor| decode_cursor(run_id, self.inner.cursor_tag, cursor))
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
                .then(|| encode_cursor(run_id, self.inner.cursor_tag, last.sequence))
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
    /// Terminal runs absent from memory are served from persisted history.
    pub fn get_result(&self, run_id: &str) -> Result<ManagedRunResult, RunManagementError> {
        let mut result = match self.record(run_id) {
            Ok(record) => record.result_snapshot(),
            Err(error) => self
                .stored_run(run_id)?
                .map(|run| run.result())
                .ok_or(error)?,
        };
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

    /// Load a terminal run from the attached history store. Non-terminal rows
    /// belong to other live owners and stay invisible to this connection.
    fn stored_run(
        &self,
        run_id: &str,
    ) -> Result<Option<crate::run_store::StoredRun>, RunManagementError> {
        let store = self
            .inner
            .run_store
            .lock()
            .expect("run store mutex")
            .clone();
        let Some(store) = store else {
            return Ok(None);
        };
        store.stored_run(run_id).map_err(|error| {
            RunManagementError::new(
                RunManagementErrorCode::Internal,
                format!("run history store read failed: {error}"),
            )
        })
    }

    /// Read persisted events for a terminal run that is no longer in memory.
    /// Persisted streams are complete, so they never report a sequence gap.
    fn read_stored_events(
        &self,
        run_id: &str,
        cursor: Option<&str>,
        page_bytes: Option<usize>,
    ) -> Result<RunEvents, RunManagementError> {
        let page_bytes = checked_page_bytes(page_bytes)?;
        let store = self
            .inner
            .run_store
            .lock()
            .expect("run store mutex")
            .clone()
            .ok_or_else(|| RunManagementError::run_not_found(run_id))?;
        let cursor_sequence = cursor
            .map(|cursor| decode_cursor(run_id, self.inner.cursor_tag, cursor))
            .transpose()?;
        let after = cursor_sequence.unwrap_or(0);
        let page = store
            .stored_events(run_id, after, page_bytes)
            .map_err(|error| {
                RunManagementError::new(
                    RunManagementErrorCode::Internal,
                    format!("run history store read failed: {error}"),
                )
            })?
            .ok_or_else(|| RunManagementError::run_not_found(run_id))?;
        if let Some(sequence) = cursor_sequence
            && sequence > page.latest_sequence
        {
            return Err(RunManagementError::invalid_cursor(
                "event cursor is newer than the retained event stream",
            ));
        }
        let next_cursor = page
            .events
            .last()
            .filter(|_| page.has_more)
            .map(|last| encode_cursor(run_id, self.inner.cursor_tag, last.sequence));
        Ok(RunEvents {
            events: page.events,
            next_cursor,
            gap: false,
            terminal: true,
        })
    }
}

fn checked_page_bytes(page_bytes: Option<usize>) -> Result<usize, RunManagementError> {
    let page_bytes = page_bytes.unwrap_or(DEFAULT_EVENT_PAGE_BYTES);
    if page_bytes == 0 || page_bytes > MAX_EVENT_PAGE_BYTES {
        return Err(RunManagementError::invalid_request(format!(
            "event page must be between 1 and {MAX_EVENT_PAGE_BYTES} bytes"
        )));
    }
    Ok(page_bytes)
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
