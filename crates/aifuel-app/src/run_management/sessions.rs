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
        let session = self.stored_session(session_id, None).ok_or_else(|| {
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
    /// Associations persist in the run history store when one is attached.
    pub fn resume_session(
        &self,
        session_id: &str,
        mut request: RunRequest,
    ) -> Result<ManagedRun, RunManagementError> {
        // Native session ids are only unique per provider, so the lookup is
        // scoped to the requested provider. Distinguish "no session for this
        // provider" from "the id belongs to another provider" for the caller.
        let session = self
            .stored_session(session_id, Some(request.provider))
            .ok_or_else(|| {
                let message = if self.stored_session(session_id, None).is_some() {
                    "session provider does not match the requested provider"
                } else {
                    "native Agent Session is not available to this owner"
                };
                RunManagementError::new(RunManagementErrorCode::SessionUnavailable, message)
            })?;
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

    /// Resolve a stored session. `provider` scopes the lookup because native
    /// session ids share no namespace across providers; the in-memory and
    /// legacy maps are keyed by bare id, so a mismatched entry is skipped and
    /// the query falls through to the provider-scoped history store.
    fn stored_session(
        &self,
        session_id: &str,
        provider: Option<aifuel_core::ProviderKey>,
    ) -> Option<SessionRecord> {
        let matches =
            |stored: aifuel_core::ProviderKey| provider.is_none_or(|provider| provider == stored);
        self.inner
            .sessions
            .lock()
            .expect("run sessions mutex")
            .get(session_id)
            .cloned()
            .filter(|session| matches(session.provider))
            .or_else(|| {
                self.inner
                    .session_store
                    .lock()
                    .expect("session store mutex")
                    .as_ref()
                    .and_then(|store| store.get(session_id))
                    .map(SessionRecord::from)
                    .filter(|session| matches(session.provider))
            })
            .or_else(|| {
                self.inner
                    .run_store
                    .lock()
                    .expect("run store mutex")
                    .as_ref()
                    .and_then(|store| store.session(session_id, provider).ok().flatten())
                    .map(SessionRecord::from)
            })
    }
}
