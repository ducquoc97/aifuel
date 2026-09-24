use super::*;

impl RunManager {
    /// Look up the explicit provider/model/effort selection associated with a
    /// stored native Agent Session. A missing model or effort remains `None`
    /// so callers can distinguish a provider-managed default from a concrete
    /// stored value. This projection excludes workspace and run content.
    pub fn session_selection(
        &self,
        session_id: &str,
    ) -> Result<StoredSessionSelection, RunManagementError> {
        let session = self.stored_session(session_id).ok_or_else(|| {
            RunManagementError::new(
                RunManagementErrorCode::SessionUnavailable,
                "native Agent Session is not available to this owner",
            )
        })?;
        Ok(StoredSessionSelection {
            schema_version: RUN_MANAGEMENT_SCHEMA_VERSION,
            session_id: session_id.to_owned(),
            provider: session.provider,
            requested_model: session.model,
            requested_effort: session.effort,
        })
    }

    /// Start a new Agent Run against a known same-provider native session.
    /// Associations are owner-local until persistent session storage is enabled.
    pub fn resume_session(
        &self,
        session_id: &str,
        mut request: RunRequest,
    ) -> Result<ManagedRun, RunManagementError> {
        let session = self.stored_session(session_id).ok_or_else(|| {
            RunManagementError::new(
                RunManagementErrorCode::SessionUnavailable,
                "native Agent Session is not available to this owner",
            )
        })?;
        if request.provider != session.provider {
            return Err(RunManagementError::new(
                RunManagementErrorCode::SessionUnavailable,
                "session provider does not match the requested provider",
            ));
        }
        if request.model.is_none() {
            request.model = session.model;
        }
        if request.effort.is_none() {
            request.effort = session.effort;
        }
        request.working_directory = request
            .working_directory
            .or(Some(session.working_directory));
        request.resume = Some(session_id.to_owned());
        self.start_run(request)
    }

    fn stored_session(&self, session_id: &str) -> Option<SessionRecord> {
        self.inner
            .sessions
            .lock()
            .expect("run sessions mutex")
            .get(session_id)
            .cloned()
            .or_else(|| {
                self.inner
                    .session_store
                    .lock()
                    .expect("session store mutex")
                    .as_ref()
                    .and_then(|store| store.get(session_id))
                    .map(|session| SessionRecord {
                        provider: session.provider,
                        model: session.model,
                        effort: session.effort,
                        working_directory: session.working_directory,
                    })
            })
    }
}
