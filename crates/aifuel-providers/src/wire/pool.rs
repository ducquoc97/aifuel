//! Key Pool rotation bookkeeping for the wire adapter: which credentials a
//! run may try, which failure state each carries, and how a `429`/`401`/`403`
//! recorded against one member persists before the next member serves.

use super::{WireExecutionAdapter, http};
use crate::{ApiKeyState, KeyHealth, ResolvedAuth};
use aifuel_core::{AuthBinding, CredentialRef};
use reqwest::header::HeaderMap;
use std::collections::BTreeMap;

/// One credential attempt within a run: the material to send plus the
/// pool-member identity whose persisted health the run maintains.
pub(super) struct KeyAttempt {
    /// The request auth this attempt applies.
    pub(super) auth: ResolvedAuth,
    /// The pooled Managed Credential the attempt exercises; `None` for an
    /// environment-sourced key or a non-key binding, which carry no
    /// persisted per-key state.
    pub(super) reference: Option<CredentialRef>,
    /// The member's failure state as persisted at resolve time.
    pub(super) state: ApiKeyState,
    /// Whether the run already sent a request with this credential.
    pub(super) attempted: bool,
}

impl KeyAttempt {
    /// Whether this credential can still serve the run: not yet attempted
    /// and not recorded as cooling or invalid at resolve time.
    pub(super) fn usable(&self) -> bool {
        !self.attempted && matches!(self.state.health(), KeyHealth::Healthy)
    }
}

/// Join the pool rotation notes with a result's own diagnostics, so a run
/// that rotated reports both what the endpoint said and why a different
/// key answered.
pub(super) fn merge_notes(base: Option<String>, notes: Vec<String>) -> Option<String> {
    let mut parts = notes;
    parts.extend(base);
    (!parts.is_empty()).then(|| parts.join(" "))
}

impl WireExecutionAdapter {
    /// This run's ordered credential attempts. An `AuthBinding::ApiKey`
    /// resolves to its whole Key Pool in first-healthy order so the run can
    /// rotate on `429`/`401`/`403`; a cookie-delivered session credential is
    /// a single attempt with no rotation or per-key state, as is any other
    /// binding.
    ///
    /// `env` is the instance's resolved overlay - empty for a base
    /// integration - so an env-sourced binding reads the variables the
    /// provider process will actually spawn with, and the destination
    /// check runs under the auth identity (the instance id when the
    /// instance rebinds the credential slot).
    pub(super) fn resolve_attempts(
        &self,
        env: &BTreeMap<String, String>,
    ) -> Result<Vec<KeyAttempt>, crate::CredentialStoreError> {
        match &self.auth {
            AuthBinding::ApiKey { source, delivery } => {
                if matches!(delivery, aifuel_core::KeyDelivery::Cookie { .. }) {
                    return Ok(vec![KeyAttempt {
                        auth: self
                            .credentials
                            .resolve_session_with_env(source, &self.auth_identity, env)
                            .map(|key| ResolvedAuth::ApiKey {
                                key,
                                delivery: delivery.clone(),
                            })?,
                        reference: None,
                        state: ApiKeyState::default(),
                        attempted: false,
                    }]);
                }
                Ok(self
                    .credentials
                    .resolve_api_key_pool_with_env(source, &self.auth_identity, env)?
                    .into_iter()
                    .map(|member| KeyAttempt {
                        auth: ResolvedAuth::ApiKey {
                            key: member.key,
                            delivery: delivery.clone(),
                        },
                        reference: member.reference,
                        state: member.state,
                        attempted: false,
                    })
                    .collect())
            }
            other => Ok(vec![KeyAttempt {
                auth: self
                    .credentials
                    .resolve_with_env(other, &self.auth_identity, env)?,
                reference: None,
                state: ApiKeyState::default(),
                attempted: false,
            }]),
        }
    }

    /// Persist the failure state one credential earned: `429` starts or
    /// grows its cooldown (honoring a `Retry-After` the endpoint sent);
    /// `401`/`403` mark it invalid. Persisting happens even when no healthy
    /// sibling remains, so the *next* run skips this key too. Store errors
    /// degrade to diagnostics notes rather than aborting the rotation the
    /// state was meant to drive.
    pub(super) fn record_key_failure(
        &self,
        attempt: &KeyAttempt,
        status: u16,
        response_headers: &HeaderMap,
        notes: &mut Vec<String>,
    ) {
        let Some(reference) = &attempt.reference else {
            return;
        };
        let outcome = match status {
            429 => self
                .credentials
                .mark_key_cooling(reference, http::retry_after(response_headers)),
            _ => self.credentials.mark_key_invalid(reference),
        };
        if let Err(error) = outcome {
            notes.push(format!(
                "the pool state for credential {reference} could not be persisted: {error}"
            ));
        }
    }

    /// Why no credential attempt can run: every pooled key is cooling or
    /// marked invalid. Counts and the earliest cooldown expiry tell the
    /// user whether to wait or re-store keys.
    pub(super) fn pool_exhausted_message(&self, attempts: &[KeyAttempt]) -> String {
        let cooling = attempts
            .iter()
            .filter(|a| matches!(a.state.health(), KeyHealth::Cooling { .. }))
            .count();
        let invalid = attempts
            .iter()
            .filter(|a| matches!(a.state.health(), KeyHealth::Invalid { .. }))
            .count();
        let earliest = attempts
            .iter()
            .filter_map(|a| match a.state.health() {
                KeyHealth::Cooling { until } => Some(until),
                _ => None,
            })
            .min();
        let retry_hint = earliest
            .map(|until| format!("; the earliest cooldown ends at {until}"))
            .unwrap_or_default();
        format!(
            "every API key for {} is cooling or marked invalid \
             ({cooling} cooling, {invalid} invalid of {}){retry_hint}; \
             re-store a key with 'aifuel auth set-key' or remove it to recover",
            self.integration,
            attempts.len(),
        )
    }
}
