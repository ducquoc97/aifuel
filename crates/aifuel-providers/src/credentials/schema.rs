//! The on-disk schema of `credentials.json` and the metadata views over it.
//!
//! Records follow the opencode `auth.json` shape: a map from Credential
//! Reference to a `type`-tagged record. The file wraps that map with an
//! explicit `schema_version` so unknown layouts fail instead of guessing.

use aifuel_core::{CredentialRef, IntegrationId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// The only schema version this build reads and writes.
pub(crate) const CREDENTIALS_SCHEMA_VERSION: u32 = 1;

/// The current unix-seconds timestamp.
pub(crate) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since_epoch| since_epoch.as_secs() as i64)
        .unwrap_or(0)
}

/// The whole `credentials.json` document.
///
/// Contains secret material, so it deliberately does not implement `Debug`:
/// there is no way to accidentally print a decoded store.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct CredentialFile {
    pub schema_version: u32,
    #[serde(default)]
    pub credentials: BTreeMap<CredentialRef, ManagedCredential>,
}

impl CredentialFile {
    /// The empty store used when no data file exists yet.
    pub(crate) fn empty() -> Self {
        Self {
            schema_version: CREDENTIALS_SCHEMA_VERSION,
            credentials: BTreeMap::new(),
        }
    }
}

/// A minimal decode of the version field, run before the full parse so a
/// newer store reports its schema version rather than a misleading shape
/// error against the current schema.
#[derive(Deserialize)]
pub(crate) struct VersionProbe {
    pub schema_version: u32,
}

/// One Managed Credential record in the Credential Store.
///
/// Secret material lives here. `Debug` is implemented manually to redact it,
/// matching `RunRequest`'s prompt-redaction precedent, and `Serialize` exists
/// only to write the store file.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ManagedCredential {
    /// A billed API key. `destination` records the Integration Identity the
    /// credential was created for; a binding resolving it through a
    /// different integration is refused rather than forwarded to another
    /// endpoint. `state` is the key's pool health, maintained by the
    /// execution path; it is absent on records written before key pools
    /// existed and on keys that have never failed.
    #[serde(rename = "api")]
    Api {
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        destination: Option<IntegrationId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<ApiKeyState>,
    },
    /// An OAuth grant pair. `refresh` is optional because some flows are
    /// access-token-only; `expires` is the unix-seconds access-token
    /// deadline; `account_id` ties the grant to a Provider Account;
    /// `destination` records the integration the grant was created for.
    #[serde(rename = "oauth")]
    OAuth {
        access: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refresh: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        destination: Option<IntegrationId>,
    },
    /// A browser-session credential entered by hand (`aifuel auth
    /// set-session`). `key` holds whatever session material the user
    /// pasted - a bare session token or a full `Cookie` header line;
    /// delivery normalizes it at send time. Sessions carry no expiry or
    /// pool state: there is no safe way to learn either from the material.
    /// `destination` records the integration the credential was created
    /// for, same as `api`.
    #[serde(rename = "session")]
    Session {
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        destination: Option<IntegrationId>,
    },
}

impl ManagedCredential {
    /// An unbound API-key Managed Credential: stored under a raw Credential
    /// Reference with no recorded destination.
    pub fn api(key: impl Into<String>) -> Self {
        Self::Api {
            key: key.into(),
            destination: None,
            state: None,
        }
    }

    /// An API-key Managed Credential bound to the integration it was
    /// created for.
    pub fn api_for(key: impl Into<String>, destination: IntegrationId) -> Self {
        Self::Api {
            key: key.into(),
            destination: Some(destination),
            state: None,
        }
    }

    /// An OAuth Managed Credential from token material.
    pub fn oauth(tokens: OAuthTokens) -> Self {
        Self::from(tokens)
    }

    /// An unbound browser-session Managed Credential: stored under a raw
    /// Credential Reference with no recorded destination.
    pub fn session(key: impl Into<String>) -> Self {
        Self::Session {
            key: key.into(),
            destination: None,
        }
    }

    /// A browser-session Managed Credential bound to the integration it
    /// was created for.
    pub fn session_for(key: impl Into<String>, destination: IntegrationId) -> Self {
        Self::Session {
            key: key.into(),
            destination: Some(destination),
        }
    }

    /// The Integration Identity this credential was created for, when one
    /// was recorded at write time.
    pub fn destination(&self) -> Option<&IntegrationId> {
        match self {
            Self::Api { destination, .. }
            | Self::OAuth { destination, .. }
            | Self::Session { destination, .. } => destination.as_ref(),
        }
    }

    /// The kind of credential, for metadata reads and binding checks.
    pub fn kind(&self) -> CredentialKind {
        match self {
            Self::Api { .. } => CredentialKind::ApiKey,
            Self::OAuth { .. } => CredentialKind::OAuth,
            Self::Session { .. } => CredentialKind::Session,
        }
    }

    /// The metadata view of this record: kind, expiry state, account
    /// identity, destination binding, and key-pool health, never secret
    /// material (spec rule 9).
    pub(crate) fn metadata(&self, now: i64) -> CredentialMetadata {
        match self {
            Self::Api {
                destination, state, ..
            } => CredentialMetadata {
                kind: CredentialKind::ApiKey,
                expiry: CredentialExpiry::None,
                account_id: None,
                destination: destination.clone(),
                key_health: Some(state.clone().unwrap_or_default().health_at(now)),
            },
            Self::OAuth {
                expires,
                account_id,
                destination,
                ..
            } => CredentialMetadata {
                kind: CredentialKind::OAuth,
                expiry: match expires {
                    Some(at) if *at <= now => CredentialExpiry::Expired { at: *at },
                    Some(at) => CredentialExpiry::Valid { until: *at },
                    None => CredentialExpiry::None,
                },
                account_id: account_id.clone(),
                destination: destination.clone(),
                key_health: None,
            },
            Self::Session { destination, .. } => CredentialMetadata {
                kind: CredentialKind::Session,
                expiry: CredentialExpiry::None,
                account_id: None,
                destination: destination.clone(),
                key_health: None,
            },
        }
    }
}

impl fmt::Debug for ManagedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api {
                destination, state, ..
            } => f
                .debug_struct("ManagedCredential")
                .field("type", &"api")
                .field("key", &"<redacted>")
                .field("destination", destination)
                .field("state", state)
                .finish(),
            Self::OAuth {
                expires,
                account_id,
                destination,
                ..
            } => f
                .debug_struct("ManagedCredential")
                .field("type", &"oauth")
                .field("access", &"<redacted>")
                .field("refresh", &"<redacted>")
                .field("expires", expires)
                .field("account_id", account_id)
                .field("destination", destination)
                .finish(),
            Self::Session { destination, .. } => f
                .debug_struct("ManagedCredential")
                .field("type", &"session")
                .field("key", &"<redacted>")
                .field("destination", destination)
                .finish(),
        }
    }
}

/// The token material for storing an OAuth Managed Credential.
///
/// `Debug` redacts the token material.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthTokens {
    /// The access token sent to the provider.
    pub access: String,
    /// The refresh grant, when the provider issued one. `None` on a rewrite
    /// preserves the stored refresh token rather than deleting it.
    pub refresh: Option<String>,
    /// The unix-seconds access-token expiry, when the provider declares one.
    pub expires: Option<i64>,
    /// The provider account identifier, when the flow reports one.
    pub account_id: Option<String>,
    /// The integration the grant was created for, recorded at write time.
    pub destination: Option<IntegrationId>,
}

impl OAuthTokens {
    /// Token material for an access-token-only grant.
    pub fn new(access: impl Into<String>) -> Self {
        Self {
            access: access.into(),
            refresh: None,
            expires: None,
            account_id: None,
            destination: None,
        }
    }
}

impl From<OAuthTokens> for ManagedCredential {
    fn from(tokens: OAuthTokens) -> Self {
        // Empty optional fields are normalized to absent so an empty string
        // cannot silently replace a stored refresh grant.
        Self::OAuth {
            access: tokens.access,
            refresh: tokens.refresh.filter(|grant| !grant.is_empty()),
            expires: tokens.expires,
            account_id: tokens.account_id.filter(|id| !id.is_empty()),
            destination: tokens.destination,
        }
    }
}

impl fmt::Debug for OAuthTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthTokens")
            .field("access", &"<redacted>")
            .field("refresh", &"<redacted>")
            .field("expires", &self.expires)
            .field("account_id", &self.account_id)
            .finish()
    }
}

/// The kind of a Managed Credential, reported by metadata reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    ApiKey,
    OAuth,
    /// A browser-session credential: pasted session material delivered as
    /// a `Cookie` header or session header, never a Bearer token.
    Session,
}

impl fmt::Display for CredentialKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey => f.write_str("an API key"),
            Self::OAuth => f.write_str("an OAuth grant"),
            Self::Session => f.write_str("a session credential"),
        }
    }
}

/// The expiry state of a Managed Credential, reported by metadata reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialExpiry {
    /// The credential declares no expiry. API keys never expire.
    None,
    /// The OAuth access token expires at this unix-seconds timestamp.
    Valid { until: i64 },
    /// The OAuth access token expired at this unix-seconds timestamp.
    Expired { at: i64 },
}

/// The recorded failure state of one API key in a pool.
///
/// The execution path maintains these fields: a `429` sets `cooling_until`
/// (honoring the endpoint's `Retry-After` when it declares one, else a
/// modest exponential step kept in `cooling_step_seconds`), a `401`/`403`
/// sets `invalid_at`, and a success clears the whole state. The record
/// persists so a key stays on cooldown across restarts; expired cooldowns
/// read as healthy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKeyState {
    /// The unix-seconds deadline of the current rate-limit cooldown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooling_until: Option<i64>,
    /// The cooldown step applied for the latest `429`, in seconds. It
    /// doubles per consecutive rate-limit response (bounded), so backoff
    /// keeps growing across restarts without a separate counter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooling_step_seconds: Option<u64>,
    /// The unix-seconds time the provider rejected this key outright
    /// (`401`/`403`). An invalid key stays out of rotation until the user
    /// re-stores it or removes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_at: Option<i64>,
}

impl ApiKeyState {
    /// The key's health right now.
    pub fn health(&self) -> KeyHealth {
        self.health_at(now_unix())
    }

    /// The key's health at `now`: a recorded rejection dominates, then an
    /// unexpired cooldown. `KeyHealth` is a view - the fields stay as
    /// recorded regardless of what health they produce.
    pub(crate) fn health_at(&self, now: i64) -> KeyHealth {
        if let Some(at) = self.invalid_at {
            return KeyHealth::Invalid { at };
        }
        if let Some(until) = self.cooling_until
            && until > now
        {
            return KeyHealth::Cooling { until };
        }
        KeyHealth::Healthy
    }
}

/// The health of one pooled API key, as metadata reads report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyHealth {
    /// No recorded failure state, or a cooldown that already expired.
    Healthy,
    /// Rate-limited: the key is skipped until the unix-seconds deadline.
    Cooling { until: i64 },
    /// Rejected outright: the key stays out of rotation until re-stored.
    Invalid { at: i64 },
}

/// Presence, kind, expiry state, account identity, destination binding, and
/// key-pool health for one Managed Credential. This is what discovery and
/// `auth list` consume; it never carries secret material, so a derived
/// `Debug` is safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialMetadata {
    /// Whether the stored record is an API key, an OAuth grant, or a
    /// session credential.
    pub kind: CredentialKind,
    /// The expiry state of the grant.
    pub expiry: CredentialExpiry,
    /// The provider account identifier, when the OAuth flow reported one.
    pub account_id: Option<String>,
    /// The integration the credential was created for, when recorded.
    pub destination: Option<IntegrationId>,
    /// The pool health of an API-key credential; `None` for OAuth grants.
    pub key_health: Option<KeyHealth>,
}
