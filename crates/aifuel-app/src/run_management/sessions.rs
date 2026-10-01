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
        let (native_id, session) = self.stored_session(session_id, None).ok_or_else(|| {
            RunManagementError::new(
                RunManagementErrorCode::SessionUnavailable,
                "no stored Agent Session matches this id",
            )
        })?;
        Ok(StoredSessionSelection {
            schema_version: RUN_MANAGEMENT_SCHEMA_VERSION,
            session_id: native_id,
            integration: session.integration,
            provider: session.provider,
            requested_model: session.model,
            requested_effort: session.effort,
        })
    }

    /// Start a new Agent Run against a known same-integration native session.
    /// Associations persist in the run history store when one is attached.
    pub fn resume_session(
        &self,
        session_id: &str,
        mut request: RunRequest,
    ) -> Result<ManagedRun, RunManagementError> {
        // Canonicalize the caller's selection first: a bare provider id must
        // resolve to the session's integration before the scoped lookup runs.
        request.integration = self.resolve_integration(&request.integration)?;
        // Native session ids are only unique per integration, so the lookup
        // is scoped to the requested integration. Distinguish "no session for
        // this integration" from "the id belongs to another integration" for
        // the caller.
        let (native_id, session) = self
            .stored_session(session_id, Some(&request.integration))
            .ok_or_else(|| {
                let message = if self.stored_session(session_id, None).is_some() {
                    "session integration does not match the requested integration"
                } else {
                    "no stored Agent Session matches this id"
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
        request.resume = Some(native_id);
        self.start_run(request)
    }

    /// Resolve a stored session. `integration` scopes the lookup because
    /// native session ids share no namespace across integrations; the
    /// in-memory and legacy maps are keyed by bare id, so a mismatched entry
    /// is skipped and the query falls through to the integration-scoped
    /// history store.
    ///
    /// Run output carries both a provider-native session id and the local
    /// session id a host was shown, so when the direct lookup misses the id
    /// is translated through the run history before giving up. The resolved
    /// native id is returned with the record so callers resume the provider
    /// session rather than the local alias.
    fn stored_session(
        &self,
        session_id: &str,
        integration: Option<&aifuel_core::IntegrationId>,
    ) -> Option<(String, SessionRecord)> {
        let direct = self
            .native_session(session_id, integration)
            .map(|record| (session_id.to_owned(), record));
        direct.or_else(|| {
            let native = {
                let store = self.inner.run_store.lock().expect("run store mutex");
                store
                    .as_ref()?
                    .native_session_for_local(session_id)
                    .ok()
                    .flatten()?
            };
            (native != session_id)
                .then(|| self.native_session(&native, integration))
                .flatten()
                .map(|record| (native, record))
        })
    }

    /// Look up `session_id` as a provider-native id across the in-memory,
    /// legacy, and durable session maps.
    fn native_session(
        &self,
        session_id: &str,
        integration: Option<&aifuel_core::IntegrationId>,
    ) -> Option<SessionRecord> {
        let matches = |stored: &aifuel_core::IntegrationId| {
            integration.is_none_or(|integration| integration == stored)
        };
        self.inner
            .sessions
            .lock()
            .expect("run sessions mutex")
            .get(session_id)
            .cloned()
            .filter(|session| matches(&session.integration))
            .or_else(|| {
                self.inner
                    .session_store
                    .lock()
                    .expect("session store mutex")
                    .as_ref()
                    .and_then(|store| store.get(session_id))
                    .map(SessionRecord::from)
                    .filter(|session| matches(&session.integration))
            })
            .or_else(|| {
                self.inner
                    .run_store
                    .lock()
                    .expect("run store mutex")
                    .as_ref()
                    .and_then(|store| store.session(session_id, integration).ok().flatten())
                    .map(SessionRecord::from)
            })
    }
}
