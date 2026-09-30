//! The scripted in-crate [`RuntimeAdapter`] double.
//!
//! `FakeAdapter` mirrors the CLI fallback's emission contract - it emits
//! `session.created`/`session.closed` itself (the pump skips them), emits
//! `approval.resolved` with a placeholder `answered_by` (the pump stamps
//! the real consumer), and reports a provider session id where scripted -
//! so the facade's fact ownership and attribution paths are exercised
//! exactly as a real adapter drives them.
//!
//! The scripted session machinery lives here; the adapter surface - the
//! [`AgentAdapter`] and [`RuntimeAdapter`] implementations - lives in
//! [`adapter`].

mod adapter;

pub use adapter::FakeAdapter;

use aifuel_core::{
    AgentEventKind, ApprovalDecision, ApprovalRequest, MessageStream, ModelSelection, ReceiptCode,
    RequestId, RunId, RunOutcome, SessionStatus,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// The canned scripted behavior of one `send` call.
pub enum FakeScript {
    /// Emit each delta as `message.delta`, then `run.completed` success.
    /// `cursor` becomes the session's reported provider session id.
    Complete {
        deltas: Vec<String>,
        cursor: Option<&'static str>,
    },
    /// Emit `approval.requested`, block on the facade's answer, then emit
    /// `approval.resolved` and `run.completed` success.
    Approval { request: ApprovalRequest },
    /// Block until cancelled, then emit `run.completed` cancelled.
    /// `cursor` is reported immediately, before blocking.
    Block { cursor: Option<&'static str> },
    /// Emit a non-retryable run-scoped `error` fact, then `run.completed`
    /// failed - the provider-side failure shape a session worker emits when
    /// the run's execution call itself errored.
    Fail { message: &'static str },
}

impl FakeScript {
    fn cursor(&self) -> Option<&'static str> {
        match self {
            Self::Complete { cursor, .. } | Self::Block { cursor } => *cursor,
            Self::Approval { .. } | Self::Fail { .. } => None,
        }
    }
}

/// One live fake session: its own event channel plus answer routing,
/// mirroring `CliSession`'s shape.
pub(super) struct FakeSession {
    selection: Mutex<ModelSelection>,
    emit: Sender<AgentEventKind>,
    events: Mutex<Option<Receiver<AgentEventKind>>>,
    pending: Mutex<HashMap<String, Sender<ApprovalDecision>>>,
    /// The resume cursor the session continues from: the provider-native
    /// session id the last run reported, or the cursor a reconcile resume
    /// seeded, mirroring `CliSession`.
    resume_cursor: Mutex<Option<String>>,
    cancelled: Arc<AtomicBool>,
    closed: AtomicBool,
    run_active: AtomicBool,
}

impl FakeSession {
    fn new(selection: ModelSelection, resume_cursor: Option<String>) -> Arc<Self> {
        let (emit, events) = mpsc::channel();
        Arc::new(Self {
            selection: Mutex::new(selection),
            emit,
            events: Mutex::new(Some(events)),
            pending: Mutex::new(HashMap::new()),
            resume_cursor: Mutex::new(resume_cursor),
            cancelled: Arc::new(AtomicBool::new(false)),
            closed: AtomicBool::new(false),
            run_active: AtomicBool::new(false),
        })
    }

    fn emit(&self, kind: AgentEventKind) {
        let _ = self.emit.send(kind);
    }
}

/// The worker behind one scripted `send`: `run.started`, `working`, the
/// scripted middle, one terminal `run.completed`, then `idle`.
fn run_script(
    session: Arc<FakeSession>,
    run_id: RunId,
    selection: ModelSelection,
    script: FakeScript,
) {
    session.emit(AgentEventKind::RunStarted {
        run_id: run_id.clone(),
        selection,
    });
    session.emit(AgentEventKind::SessionStatus {
        status: SessionStatus::Working,
    });
    let outcome = match script {
        FakeScript::Complete { deltas, .. } => {
            for delta in deltas {
                session.emit(AgentEventKind::MessageDelta {
                    run_id: run_id.clone(),
                    stream: MessageStream::Assistant,
                    text: delta,
                });
            }
            RunOutcome::Success
        }
        FakeScript::Approval { request } => {
            let request_id = RequestId::new(format!("fake-approval-{}", run_id.as_str()));
            let (sender, receiver) = mpsc::channel();
            session
                .pending
                .lock()
                .expect("pending mutex")
                .insert(request_id.as_str().to_owned(), sender);
            session.emit(AgentEventKind::SessionStatus {
                status: SessionStatus::WaitingApproval,
            });
            session.emit(AgentEventKind::ApprovalRequested {
                run_id: run_id.clone(),
                request_id: request_id.clone(),
                request,
            });
            match receiver.recv_timeout(Duration::from_secs(30)) {
                Ok(decision) => {
                    // The placeholder attribution the facade's pump rewrites
                    // with the answering consumer id.
                    session.emit(AgentEventKind::ApprovalResolved {
                        request_id,
                        decision,
                        answered_by: "adapter".to_owned(),
                    });
                    session.emit(AgentEventKind::SessionStatus {
                        status: SessionStatus::Working,
                    });
                    RunOutcome::Success
                }
                Err(_) => RunOutcome::Cancelled,
            }
        }
        FakeScript::Block { .. } => {
            while !session.cancelled.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
            RunOutcome::Cancelled
        }
        FakeScript::Fail { message } => {
            session.emit(AgentEventKind::Error {
                run_id: Some(run_id.clone()),
                code: ReceiptCode::ProviderError,
                message: message.to_owned(),
                retryable: false,
            });
            RunOutcome::Failed
        }
    };
    session.run_active.store(false, Ordering::Release);
    session.emit(AgentEventKind::RunCompleted {
        run_id,
        outcome,
        usage: None,
    });
    if !session.closed.load(Ordering::Acquire) {
        session.emit(AgentEventKind::SessionStatus {
            status: SessionStatus::Idle,
        });
    }
}
