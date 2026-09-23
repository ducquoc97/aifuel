use super::helpers::{event_size, now, truncate_string, utf8_prefix_len};
use super::*;

impl RunRecord {
    pub(super) fn new(
        run_id: String,
        request: &RunRequest,
        deadline: Option<Instant>,
        workspace_lock: Option<WorkspaceWriteLock>,
    ) -> Self {
        Self {
            run_id,
            provider: request.provider,
            access: request.access,
            requested_model: request.model.clone(),
            requested_effort: request.effort.clone(),
            external_tools: request.external_tools.clone(),
            created_at: now(),
            deadline,
            metadata: Mutex::new(RunMetadata {
                state: RunState::Starting,
                completed_at: None,
                content_available: false,
                content_persisted: false,
                result: ResultMetadata::empty(),
            }),
            pending_input: Mutex::new(None),
            interaction_response: Mutex::new(None),
            interaction_changed: Condvar::new(),
            output_capture: Mutex::new(OutputCapture::default()),
            events: Mutex::new(EventBuffer::default()),
            cancellation: RunCancellationToken::new(),
            retained_content_bytes: AtomicUsize::new(0),
            worker: Mutex::new(None),
            workspace_lock: Mutex::new(workspace_lock),
        }
    }

    pub(super) fn snapshot(&self) -> ManagedRun {
        let metadata = self.metadata.lock().expect("run metadata mutex");
        ManagedRun {
            schema_version: RUN_MANAGEMENT_SCHEMA_VERSION,
            run_id: self.run_id.clone(),
            state: metadata.state,
            provider: self.provider,
            requested_model: self.requested_model.clone(),
            requested_effort: self.requested_effort.clone(),
            external_tools: self.external_tools.clone(),
            created_at: self.created_at,
            completed_at: metadata.completed_at,
            content_available: metadata.content_available,
            pending_input: self
                .pending_input
                .lock()
                .expect("pending input mutex")
                .clone(),
        }
    }

    pub(super) fn result_snapshot(&self) -> ManagedRunResult {
        let metadata = self.metadata.lock().expect("run metadata mutex");
        let result = &metadata.result;
        let output_capture = self
            .output_capture
            .lock()
            .expect("run output capture mutex");
        let partial_output = (!metadata.state.is_terminal() && output_capture.streamed)
            .then(|| output_capture.text.clone());
        let partial_available = partial_output.is_some();
        ManagedRunResult {
            schema_version: RUN_MANAGEMENT_SCHEMA_VERSION,
            run_id: self.run_id.clone(),
            state: metadata.state,
            provider: self.provider,
            requested_model: self.requested_model.clone(),
            requested_effort: self.requested_effort.clone(),
            effective_model: result.effective_model.clone(),
            effective_effort: result.effective_effort.clone(),
            local_session_id: result.local_session_id.clone(),
            session_id: result.session_id.clone(),
            status: result.status,
            exit_code: result.exit_code,
            output: result.output.clone().or(partial_output),
            error: result.error.clone(),
            diagnostics: result.diagnostics.clone(),
            content_available: metadata.content_available || partial_available,
            output_truncated: result.output_truncated || output_capture.truncated,
            diagnostics_truncated: result.diagnostics_truncated,
            output_bytes: if partial_available {
                output_capture.observed_bytes
            } else {
                result.output_bytes
            },
            diagnostics_bytes: result.diagnostics_bytes,
        }
    }

    pub(super) fn clear_content(&self) {
        let mut metadata = self.metadata.lock().expect("run metadata mutex");
        metadata.content_available = metadata.content_persisted;
        metadata.result.output = None;
        metadata.result.error = None;
        metadata.result.diagnostics = None;
        self.output_capture
            .lock()
            .expect("run output capture mutex")
            .text
            .clear();
    }

    pub(super) fn capture_output(&self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let mut capture = self
            .output_capture
            .lock()
            .expect("run output capture mutex");
        capture.streamed = true;
        capture.observed_bytes = capture.observed_bytes.saturating_add(delta.len());
        let remaining = MAX_ANSWER_BYTES_PER_RUN.saturating_sub(capture.text.len());
        let retained = utf8_prefix_len(delta, remaining);
        capture.text.push_str(&delta[..retained]);
        if retained < delta.len() {
            capture.truncated = true;
        }

        let event_limit = MAX_EVENT_BYTES_PER_RUN.saturating_sub(96);
        let mut start = 0;
        while start < delta.len() {
            let end_limit = start.saturating_add(event_limit).min(delta.len());
            let end = end_limit
                - (0..=3)
                    .find(|offset| {
                        end_limit.saturating_sub(*offset) >= start
                            && delta.is_char_boundary(end_limit.saturating_sub(*offset))
                    })
                    .unwrap_or(0);
            if end == start {
                break;
            }
            self.push_event(RunEventKind::Output, Some(delta[start..end].to_owned()));
            start = end;
        }
    }

    pub(super) fn push_event(&self, kind: RunEventKind, data: Option<String>) {
        self.events
            .lock()
            .expect("run events mutex")
            .push(&self.run_id, kind, data);
    }
}

impl ResultMetadata {
    fn empty() -> Self {
        Self {
            status: None,
            effective_model: None,
            effective_effort: None,
            local_session_id: None,
            session_id: None,
            exit_code: None,
            output: None,
            error: None,
            diagnostics: None,
            output_truncated: false,
            diagnostics_truncated: false,
            output_bytes: 0,
            diagnostics_bytes: 0,
        }
    }
}

impl EventBuffer {
    fn push(&mut self, run_id: &str, kind: RunEventKind, data: Option<String>) {
        let (data, _) = truncate_string(data, MAX_EVENT_BYTES_PER_RUN.saturating_sub(96));
        let event = RunEvent {
            schema_version: RUN_MANAGEMENT_SCHEMA_VERSION,
            run_id: run_id.to_owned(),
            sequence: self.next_sequence,
            created_at: now(),
            kind,
            data,
        };
        self.next_sequence = self.next_sequence.saturating_add(1);
        let bytes = event_size(&event);
        while !self.events.is_empty() && self.bytes.saturating_add(bytes) > MAX_EVENT_BYTES_PER_RUN
        {
            if let Some(oldest) = self.events.pop_front() {
                self.bytes = self.bytes.saturating_sub(event_size(&oldest));
                self.gap = true;
            }
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.events.push_back(event);
    }
}
