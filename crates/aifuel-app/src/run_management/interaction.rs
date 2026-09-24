use super::helpers::{now, ordinary_input_response};
use super::*;

impl RunManager {
    pub fn request_input(
        &self,
        run_id: &str,
        kind: RunInputKind,
        description: impl Into<String>,
    ) -> Result<PendingRunInput, RunManagementError> {
        let record = self.record(run_id)?;
        let mut metadata = record.metadata.lock().expect("run metadata mutex");
        if metadata.state.is_terminal() {
            return Err(RunManagementError::new(
                RunManagementErrorCode::InputConflict,
                "run is already terminal",
            ));
        }
        let interaction_kind = match &kind {
            RunInputKind::Ordinary => AgentInteractionKind::OrdinaryInput,
            RunInputKind::Permission => AgentInteractionKind::PermissionProfileApproval,
        };
        let input = PendingRunInput {
            input_id: format!("input-{}-{}", std::process::id(), now()),
            run_id: run_id.to_owned(),
            kind,
            interaction_kind,
            description: description.into(),
            native_method: None,
            questions: Vec::new(),
            question_ids: Vec::new(),
            parameters: None,
            requires_expanded_access: false,
        };
        metadata.state = match input.kind {
            RunInputKind::Ordinary => RunState::WaitingForInput,
            RunInputKind::Permission => RunState::WaitingForApproval,
        };
        drop(metadata);
        *record.pending_input.lock().expect("pending input mutex") = Some(input.clone());
        *record
            .interaction_response
            .lock()
            .expect("interaction response mutex") = None;
        self.push_event(
            &record,
            match input.kind {
                RunInputKind::Ordinary => RunEventKind::WaitingForInput,
                RunInputKind::Permission => RunEventKind::WaitingForApproval,
            },
            None,
        );
        Ok(input)
    }

    pub fn answer_input(
        &self,
        run_id: &str,
        input_id: &str,
        response: &str,
    ) -> Result<ManagedRun, RunManagementError> {
        self.answer_input_value(
            run_id,
            input_id,
            serde_json::Value::String(response.to_owned()),
        )
    }

    /// Answer an ordinary provider request with either a single string, a
    /// question-id-to-string-list map, or a typed MCP elicitation object.
    /// Permission requests remain local-only and are rejected here.
    pub fn answer_input_value(
        &self,
        run_id: &str,
        input_id: &str,
        response: serde_json::Value,
    ) -> Result<ManagedRun, RunManagementError> {
        let record = self.record(run_id)?;
        let pending = record
            .pending_input
            .lock()
            .expect("pending input mutex")
            .clone()
            .ok_or_else(|| {
                RunManagementError::new(
                    RunManagementErrorCode::InputConflict,
                    "no pending input exists",
                )
            })?;
        if pending.input_id != input_id {
            return Err(RunManagementError::new(
                RunManagementErrorCode::InputConflict,
                "input response does not match the pending request",
            ));
        }
        if pending.kind == RunInputKind::Permission {
            return Err(RunManagementError::new(
                RunManagementErrorCode::UnsupportedCapability,
                "permission approvals must be delivered through the local terminal operation",
            ));
        }
        let response_size = response.to_string().len();
        if response_size > MAX_ANSWER_BYTES_PER_RUN {
            return Err(RunManagementError::new(
                RunManagementErrorCode::InputConflict,
                format!(
                    "ordinary input response exceeds the {MAX_ANSWER_BYTES_PER_RUN} byte limit"
                ),
            ));
        }
        let response = ordinary_input_response(&pending, response)?;
        let mut metadata = record.metadata.lock().expect("run metadata mutex");
        if metadata.state.is_terminal() {
            return Err(RunManagementError::new(
                RunManagementErrorCode::InputConflict,
                "run became terminal before the input response",
            ));
        }
        let mut pending_guard = record.pending_input.lock().expect("pending input mutex");
        if pending_guard
            .as_ref()
            .is_none_or(|pending| pending.input_id != input_id)
        {
            return Err(RunManagementError::new(
                RunManagementErrorCode::InputConflict,
                "input response does not match the current pending request",
            ));
        }
        *pending_guard = None;
        *record
            .interaction_response
            .lock()
            .expect("interaction response mutex") = Some(response);
        record.interaction_changed.notify_all();
        metadata.state = RunState::Running;
        drop(metadata);
        drop(pending_guard);
        self.push_event(&record, RunEventKind::Running, None);
        Ok(record.snapshot())
    }

    pub fn approve_locally(
        &self,
        run_id: &str,
        input_id: &str,
        decision: PermissionApprovalDecision,
    ) -> Result<ManagedRun, RunManagementError> {
        let record = self.record(run_id)?;
        let pending = record
            .pending_input
            .lock()
            .expect("pending input mutex")
            .clone()
            .ok_or_else(|| {
                RunManagementError::new(
                    RunManagementErrorCode::InputConflict,
                    "no pending permission request exists",
                )
            })?;
        if pending.input_id != input_id || pending.kind != RunInputKind::Permission {
            return Err(RunManagementError::new(
                RunManagementErrorCode::InputConflict,
                "approval does not match the pending permission request",
            ));
        }
        let interaction_response = match pending.interaction_kind {
            AgentInteractionKind::CommandApproval => match decision {
                PermissionApprovalDecision::Accept
                    if record.access == aifuel_core::AccessMode::ReadOnly =>
                {
                    return Err(RunManagementError::new(
                        RunManagementErrorCode::PolicyDenied,
                        "command approval cannot expand a read-only run's access",
                    ));
                }
                PermissionApprovalDecision::Accept if pending.requires_expanded_access => {
                    return Err(RunManagementError::new(
                        RunManagementErrorCode::PolicyDenied,
                        "command approval cannot add access beyond the run policy",
                    ));
                }
                _ => AgentInteractionResponse::Permission(decision),
            },
            AgentInteractionKind::FileChangeApproval => match decision {
                PermissionApprovalDecision::Accept
                    if record.access == aifuel_core::AccessMode::ReadOnly =>
                {
                    return Err(RunManagementError::new(
                        RunManagementErrorCode::PolicyDenied,
                        "file-change approval cannot expand a read-only run's access",
                    ));
                }
                PermissionApprovalDecision::Accept if pending.requires_expanded_access => {
                    return Err(RunManagementError::new(
                        RunManagementErrorCode::PolicyDenied,
                        "file-change approval cannot add access beyond the run policy",
                    ));
                }
                _ => AgentInteractionResponse::Permission(decision),
            },
            AgentInteractionKind::PermissionProfileApproval => match decision {
                PermissionApprovalDecision::Accept => {
                    return Err(RunManagementError::new(
                        RunManagementErrorCode::PolicyDenied,
                        "permission profile approvals cannot expand the current run policy",
                    ));
                }
                PermissionApprovalDecision::Decline => {
                    AgentInteractionResponse::PermissionProfile {
                        permissions: serde_json::json!({}),
                        scope: "turn".to_owned(),
                    }
                }
                PermissionApprovalDecision::Cancel => return self.cancel_run(run_id),
            },
            _ => {
                return Err(RunManagementError::new(
                    RunManagementErrorCode::PolicyDenied,
                    "this permission request cannot be granted within the current run policy",
                ));
            }
        };
        #[cfg(any(unix, windows))]
        self.remove_local_approval_pending(run_id, input_id);
        *record.pending_input.lock().expect("pending input mutex") = None;
        *record
            .interaction_response
            .lock()
            .expect("interaction response mutex") = Some(interaction_response);
        record.interaction_changed.notify_all();
        let mut metadata = record.metadata.lock().expect("run metadata mutex");
        if !metadata.state.is_terminal() {
            metadata.state = RunState::Running;
        }
        drop(metadata);
        self.push_event(&record, RunEventKind::Running, None);
        Ok(record.snapshot())
    }

    pub(super) fn wait_for_provider_interaction(
        &self,
        record: &Arc<RunRecord>,
        request: AgentInteractionRequest,
        cancellation: &RunCancellationToken,
    ) -> Result<AgentInteractionResponse, AgentRunError> {
        let kind = match request.kind {
            AgentInteractionKind::OrdinaryInput | AgentInteractionKind::McpElicitation => {
                RunInputKind::Ordinary
            }
            AgentInteractionKind::CommandApproval
            | AgentInteractionKind::FileChangeApproval
            | AgentInteractionKind::PermissionProfileApproval => RunInputKind::Permission,
        };
        let state = match kind {
            RunInputKind::Ordinary => RunState::WaitingForInput,
            RunInputKind::Permission => RunState::WaitingForApproval,
        };
        let question_ids = request
            .questions
            .iter()
            .map(|question| question.id.clone())
            .collect();
        let pending = PendingRunInput {
            input_id: format!("input-{}-{}", std::process::id(), now()),
            run_id: record.run_id.clone(),
            kind: kind.clone(),
            interaction_kind: request.kind,
            description: request.description,
            native_method: Some(request.method),
            questions: request.questions,
            question_ids,
            parameters: Some(request.parameters),
            requires_expanded_access: request.requires_expanded_access,
        };
        let mut metadata = record.metadata.lock().expect("run metadata mutex");
        if metadata.state.is_terminal() || cancellation.is_cancelled() {
            return Err(AgentRunError::Cancelled);
        }
        metadata.state = state;
        *record.pending_input.lock().expect("pending input mutex") = Some(pending.clone());
        *record
            .interaction_response
            .lock()
            .expect("interaction response mutex") = None;
        #[cfg(any(unix, windows))]
        if pending.kind == RunInputKind::Permission
            && let Err(error) = self.register_local_approval_pending(&pending)
        {
            *record.pending_input.lock().expect("pending input mutex") = None;
            *record
                .interaction_response
                .lock()
                .expect("interaction response mutex") = None;
            metadata.state = RunState::Running;
            drop(metadata);
            self.remove_local_approval_pending(&pending.run_id, &pending.input_id);
            return Err(error);
        }
        self.push_event(
            record,
            match kind {
                RunInputKind::Ordinary => RunEventKind::WaitingForInput,
                RunInputKind::Permission => RunEventKind::WaitingForApproval,
            },
            None,
        );
        drop(metadata);
        let mut response = record
            .interaction_response
            .lock()
            .expect("interaction response mutex");
        loop {
            if cancellation.is_cancelled() {
                #[cfg(any(unix, windows))]
                {
                    let pending = record
                        .pending_input
                        .lock()
                        .expect("pending input mutex")
                        .take();
                    if let Some(pending) =
                        pending.filter(|pending| pending.kind == RunInputKind::Permission)
                    {
                        self.remove_local_approval_pending(&pending.run_id, &pending.input_id);
                    }
                }
                #[cfg(not(any(unix, windows)))]
                {
                    *record.pending_input.lock().expect("pending input mutex") = None;
                }
                return Err(AgentRunError::Cancelled);
            }
            if record
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                #[cfg(any(unix, windows))]
                {
                    let pending = record
                        .pending_input
                        .lock()
                        .expect("pending input mutex")
                        .take();
                    if let Some(pending) =
                        pending.filter(|pending| pending.kind == RunInputKind::Permission)
                    {
                        self.remove_local_approval_pending(&pending.run_id, &pending.input_id);
                    }
                }
                #[cfg(not(any(unix, windows)))]
                {
                    *record.pending_input.lock().expect("pending input mutex") = None;
                }
                return Err(AgentRunError::Timeout(
                    "Agent Run exceeded its deadline while waiting for input".to_owned(),
                ));
            }
            if let Some(response) = response.take() {
                return Ok(response);
            }
            let wait = record
                .deadline
                .map(|deadline| {
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(100))
                })
                .unwrap_or(Duration::from_millis(100));
            let (next, _) = record
                .interaction_changed
                .wait_timeout(response, wait)
                .expect("interaction response mutex");
            response = next;
        }
    }
}

pub(super) struct OwnerInteractionHandler {
    pub(super) inner: Weak<ManagerInner>,
    pub(super) record: Weak<RunRecord>,
}

impl std::fmt::Debug for OwnerInteractionHandler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OwnerInteractionHandler")
    }
}

impl AgentInteractionHandler for OwnerInteractionHandler {
    fn interact(
        &self,
        request: AgentInteractionRequest,
        cancellation: &RunCancellationToken,
    ) -> Result<AgentInteractionResponse, AgentRunError> {
        let inner = self.inner.upgrade().ok_or(AgentRunError::Cancelled)?;
        let record = self.record.upgrade().ok_or(AgentRunError::Cancelled)?;
        RunManager {
            inner,
            _owner: None,
        }
        .wait_for_provider_interaction(&record, request, cancellation)
    }
}
