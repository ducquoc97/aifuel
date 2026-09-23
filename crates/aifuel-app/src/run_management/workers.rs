use super::*;

pub(super) fn shutdown_inner(inner: &Arc<ManagerInner>) {
    if inner.shutdown.swap(true, Ordering::AcqRel) {
        return;
    }
    #[cfg(any(unix, windows))]
    let approval_server = inner
        .approval_server
        .lock()
        .expect("approval server mutex")
        .take();
    #[cfg(any(unix, windows))]
    drop(approval_server);
    let records = inner
        .records
        .lock()
        .expect("run records mutex")
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for record in &records {
        let mut metadata = record.metadata.lock().expect("run metadata mutex");
        if !metadata.state.is_terminal() {
            metadata.state = RunState::Cancelling;
            record.cancellation.cancel();
            record.interaction_changed.notify_all();
        }
    }
    for record in records {
        if let Some(worker) = record.worker.lock().expect("run worker mutex").take() {
            let _ = worker.join();
        }
    }
}

impl Drop for OwnerToken {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            shutdown_inner(&inner);
        }
    }
}

pub(super) fn run_worker(
    weak_inner: Weak<ManagerInner>,
    record: Arc<RunRecord>,
    adapter: AdapterHandle,
    request: RunRequest,
    output_handler: Arc<dyn AgentRunOutputHandler>,
) {
    let Some(inner) = weak_inner.upgrade() else {
        return;
    };
    let manager = RunManager {
        inner,
        _owner: None,
    };
    {
        let mut metadata = record.metadata.lock().expect("run metadata mutex");
        if metadata.state == RunState::Cancelling {
            drop(metadata);
            manager.complete(&record, Err(AgentRunError::Cancelled));
            return;
        }
        metadata.state = RunState::Running;
    }
    manager.push_event(&record, RunEventKind::Running, None);
    let result = adapter.execute_with_output_handler(
        &request,
        &record.cancellation,
        output_handler.as_ref(),
    );
    manager.complete(&record, result);
}
