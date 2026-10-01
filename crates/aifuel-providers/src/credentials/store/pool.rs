//! Key Pool operations: member addressing under a binding's Credential
//! Reference, ordered pool resolution, member appends, and the persisted
//! per-key health transitions the execution path records.
//!
//! A pool is not a stored entity: `root` and every `root/…` API-key record
//! form the pool bound under `root`, so the flat `credentials.json` map and
//! single-key files written before pools existed need no migration.

use super::super::schema::{ApiKeyState, CredentialKind, KeyHealth, now_unix};
use super::{
    CredentialStore, CredentialStoreError, ManagedCredential, check_destination, env_override,
};
use aifuel_core::{ApiKeySource, CredentialRef, IntegrationId};
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

/// The first cooldown step when a `429` carries no usable `Retry-After`.
/// Modest on purpose: provider rate windows are usually per-minute, so a
/// short first step keeps a transient limit cheap to recover from.
const BASE_COOLDOWN_SECONDS: u64 = 15;

/// The longest a key stays cooling on backoff alone. `Retry-After` values
/// the endpoint declares are clamped separately and may run longer.
const MAX_COOLDOWN_SECONDS: u64 = 300;

/// The bound applied to an endpoint-declared `Retry-After`, so a hostile or
/// broken endpoint cannot park a key effectively forever.
const MAX_RETRY_AFTER_SECONDS: u64 = 3600;

/// Whether `member` belongs to the Key Pool rooted at `root`: it is the
/// root record itself or a `root/…` member record. The suffix is never
/// parsed - `aifuel auth set-key` assigns `/2`, `/3`, ... but any API-key
/// record under the prefix is a member.
pub fn is_pool_member(root: &CredentialRef, member: &CredentialRef) -> bool {
    match member.as_str().strip_prefix(root.as_str()) {
        Some("") => true,
        Some(suffix) => suffix.starts_with('/'),
        None => false,
    }
}

/// The first unoccupied member slot in the pool rooted at `root`: `root`
/// itself when free, else the smallest `root/N` with N >= 2.
fn next_pool_slot(
    credentials: &BTreeMap<CredentialRef, ManagedCredential>,
    root: &CredentialRef,
) -> CredentialRef {
    if !credentials.contains_key(root) {
        return root.clone();
    }
    let mut index = 2u64;
    loop {
        let candidate = CredentialRef::new(format!("{}/{index}", root.as_str()));
        if !credentials.contains_key(&candidate) {
            return candidate;
        }
        index += 1;
    }
}

impl CredentialStore {
    /// Resolve an API-key source into its ordered Key Pool: every usable
    /// member in Credential Reference order.
    ///
    /// A store-backed source resolves to the pool rooted at its Credential
    /// Reference - the record at `reference` plus every `reference/…`
    /// member. An environment source is a pool of one carrying no persisted
    /// health (`reference` is `None`). `EnvOrStore` prefers the stored pool
    /// and falls back to the variable only when no member is stored.
    /// Member order is Credential Reference order: `reference`, then
    /// `reference/2`, `reference/3`, ... - so selection is a stable
    /// first-healthy pick. Kind and destination mismatches are errors,
    /// never a silent skip.
    pub fn resolve_api_key_pool(
        &self,
        source: &ApiKeySource,
        integration: &IntegrationId,
    ) -> Result<Vec<PoolKey>, CredentialStoreError> {
        match source {
            ApiKeySource::Env { var } => {
                let key = env_override(var)
                    .ok_or_else(|| CredentialStoreError::EnvVarAbsent { var: var.clone() })?;
                Ok(vec![PoolKey::environment(key)])
            }
            ApiKeySource::Store { credential } => {
                let pool = self.api_key_pool(credential, integration)?;
                if pool.is_empty() {
                    return Err(CredentialStoreError::CredentialAbsent(credential.clone()));
                }
                Ok(pool)
            }
            ApiKeySource::EnvOrStore { var, credential } => {
                let pool = self.api_key_pool(credential, integration)?;
                if !pool.is_empty() {
                    return Ok(pool);
                }
                let key = env_override(var)
                    .ok_or_else(|| CredentialStoreError::EnvVarAbsent { var: var.clone() })?;
                Ok(vec![PoolKey::environment(key)])
            }
        }
    }

    /// The pool members stored under `reference`: the record at
    /// `reference` itself plus every `reference/…` API-key record, in
    /// Credential Reference order. An empty result means no member is
    /// stored; it is not an error, so `EnvOrStore` bindings can fall back.
    pub fn api_key_pool(
        &self,
        reference: &CredentialRef,
        integration: &IntegrationId,
    ) -> Result<Vec<PoolKey>, CredentialStoreError> {
        let file = self.read()?;
        let mut members = Vec::new();
        for (member_ref, credential) in &file.credentials {
            if !is_pool_member(reference, member_ref) {
                continue;
            }
            check_destination(member_ref, credential, integration)?;
            match credential {
                ManagedCredential::Api { key, state, .. } => members.push(PoolKey {
                    reference: Some(member_ref.clone()),
                    key: key.clone(),
                    state: state.clone().unwrap_or_default(),
                }),
                other => {
                    return Err(CredentialStoreError::UnexpectedCredentialKind {
                        reference: member_ref.clone(),
                        expected: CredentialKind::ApiKey,
                        found: other.kind(),
                    });
                }
            }
        }
        Ok(members)
    }

    /// Add an API key to the pool rooted at `reference`, bound to the
    /// `destination` integration it was created for.
    ///
    /// The first member takes `reference` itself; later members take
    /// `reference/2`, `reference/3`, and so on, so every member remains an
    /// ordinary Managed Credential that `auth list` and `auth remove` can
    /// address individually. Re-adding identical material resets the
    /// existing member's recorded failure state instead of duplicating it -
    /// a deliberate re-store is how a user revives a key marked invalid.
    ///
    /// Returns the member's Credential Reference and whether a new member
    /// was created.
    pub fn add_pool_api_key(
        &self,
        reference: &CredentialRef,
        key: &str,
        destination: &IntegrationId,
    ) -> Result<(CredentialRef, bool), CredentialStoreError> {
        if key.is_empty() {
            return Err(CredentialStoreError::InvalidMaterial(
                "an API key cannot be empty",
            ));
        }
        self.update(|credentials| {
            // Every record in the pool namespace must be an API key: a
            // foreign record sharing the prefix is a store inconsistency,
            // not a pool member to silently absorb.
            for (member_ref, credential) in credentials.iter() {
                if is_pool_member(reference, member_ref)
                    && !matches!(credential, ManagedCredential::Api { .. })
                {
                    return Err(CredentialStoreError::UnexpectedCredentialKind {
                        reference: member_ref.clone(),
                        expected: CredentialKind::ApiKey,
                        found: credential.kind(),
                    });
                }
            }
            // Re-adding identical material revives the existing member: its
            // destination binding is re-asserted and its recorded failure
            // state cleared rather than duplicated into a second slot.
            let existing = credentials
                .iter()
                .find_map(|(r, credential)| match credential {
                    ManagedCredential::Api { key: stored, .. }
                        if is_pool_member(reference, r) && stored == key =>
                    {
                        Some(r.clone())
                    }
                    _ => None,
                });
            if let Some(member_ref) = existing {
                if let Some(ManagedCredential::Api {
                    destination: bound,
                    state,
                    ..
                }) = credentials.get_mut(&member_ref)
                {
                    *bound = Some(destination.clone());
                    *state = None;
                }
                return Ok((member_ref, false));
            }
            let member_ref = next_pool_slot(credentials, reference);
            credentials.insert(
                member_ref.clone(),
                ManagedCredential::api_for(key, destination.clone()),
            );
            Ok((member_ref, true))
        })
    }

    /// Record a rate-limit (`429`) against one pooled key: the cooldown
    /// deadline is `retry_after` when the endpoint declared one (clamped to
    /// a sane bound), else a step that doubles per consecutive `429` from
    /// [`BASE_COOLDOWN_SECONDS`] up to [`MAX_COOLDOWN_SECONDS`]. Persisted
    /// state keeps the key out of rotation across restarts.
    pub fn mark_key_cooling(
        &self,
        reference: &CredentialRef,
        retry_after: Option<Duration>,
    ) -> Result<(), CredentialStoreError> {
        self.update(|credentials| {
            let Some(ManagedCredential::Api { state, .. }) = credentials.get_mut(reference) else {
                return Ok(());
            };
            let state = state.get_or_insert_with(ApiKeyState::default);
            let step = match retry_after {
                Some(declared) => declared.as_secs().clamp(1, MAX_RETRY_AFTER_SECONDS),
                None => state
                    .cooling_step_seconds
                    .map_or(BASE_COOLDOWN_SECONDS, |previous| {
                        previous.saturating_mul(2).min(MAX_COOLDOWN_SECONDS)
                    }),
            };
            state.cooling_until = Some(now_unix() + step as i64);
            state.cooling_step_seconds = Some(step);
            Ok(())
        })
    }

    /// Record a terminal rejection (`401`/`403`) against one pooled key.
    /// The mark has no expiry: the provider said the key itself is bad, so
    /// it stays out of rotation until the user re-stores or removes it.
    pub fn mark_key_invalid(&self, reference: &CredentialRef) -> Result<(), CredentialStoreError> {
        self.update(|credentials| {
            if let Some(ManagedCredential::Api { state, .. }) = credentials.get_mut(reference) {
                *state = Some(ApiKeyState {
                    invalid_at: Some(now_unix()),
                    ..Default::default()
                });
            }
            Ok(())
        })
    }

    /// Drop a pooled key's recorded failure state after a success. The
    /// success is fresher evidence than any concurrent cooldown mark, so a
    /// run that completed with this key clears it unconditionally.
    pub fn clear_key_state(&self, reference: &CredentialRef) -> Result<(), CredentialStoreError> {
        self.update(|credentials| {
            if let Some(ManagedCredential::Api { state, .. }) = credentials.get_mut(reference) {
                *state = None;
            }
            Ok(())
        })
    }

    /// Whether `reference` itself or any `reference/…` pool member is
    /// stored. This is a presence check for discovery evidence: it reads
    /// the same metadata-only map as [`Self::metadata`], never material.
    pub fn contains_credential(
        &self,
        reference: &CredentialRef,
    ) -> Result<bool, CredentialStoreError> {
        Ok(self
            .read()?
            .credentials
            .iter()
            .any(|(r, _)| is_pool_member(reference, r)))
    }
}

/// One member of an [`AuthBinding::ApiKey`] Key Pool: the key material plus
/// the identity and health the execution path needs for cooldown
/// bookkeeping. Members arrive in Credential Reference order, and the
/// caller picks the first healthy one (see
/// [`CredentialStore::resolve_api_key_pool`]).
///
/// Carries secret material, so `Debug` is manual and redacts it.
#[derive(Clone, PartialEq, Eq)]
pub struct PoolKey {
    /// The member's Credential Reference, for recording cooldown state.
    /// `None` for an environment-sourced key, which has no persisted state.
    pub reference: Option<CredentialRef>,
    /// The key material.
    pub key: String,
    /// The persisted failure state observed at resolve time.
    pub state: ApiKeyState,
}

impl PoolKey {
    /// The pool-of-one an environment-sourced key forms: material only, no
    /// persisted health.
    fn environment(key: String) -> Self {
        Self {
            reference: None,
            key,
            state: ApiKeyState::default(),
        }
    }

    /// Whether the key is currently usable - neither cooling nor marked
    /// invalid at resolve time.
    pub fn healthy(&self) -> bool {
        matches!(self.state.health(), KeyHealth::Healthy)
    }
}

impl fmt::Debug for PoolKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolKey")
            .field("reference", &self.reference)
            .field("key", &"<redacted>")
            .field("state", &self.state)
            .finish()
    }
}
