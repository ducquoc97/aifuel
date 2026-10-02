//! `CredentialStore`: locked, atomic access to `credentials.json`.

use super::lock::{StoreLock, atomic_replace};
use super::schema::{
    CREDENTIALS_SCHEMA_VERSION, CredentialFile, CredentialKind, CredentialMetadata,
    ManagedCredential, OAuthTokens, VersionProbe, now_unix,
};
use aifuel_core::{AuthBinding, CredentialRef, IntegrationId, KeyDelivery};
use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

mod pool;
pub use pool::{PoolKey, is_pool_member};

const STORE_FILE_NAME: &str = "credentials.json";
const LOCK_FILE_NAME: &str = "credentials.json.lock";

/// The AI Fuel Credential Store rooted at a caller-provided directory.
///
/// The executable passes the AI Fuel configuration directory; tests use
/// temporary directories. Every method is synchronous and performs blocking
/// file I/O: async callers must run them inside `tokio::task::spawn_blocking`
/// or an equivalent blocking context (spec transaction rule 8). The store is
/// `Send + Sync`, so one instance can be shared across threads; the sidecar
/// lock serializes mutations between them and between processes.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    data_path: PathBuf,
    lock_path: PathBuf,
}

impl CredentialStore {
    /// Root a store at `directory`. Nothing is created on disk until the
    /// first write, so read-only discovery stays side-effect-free.
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        let directory = directory.into();
        Self {
            data_path: directory.join(STORE_FILE_NAME),
            lock_path: directory.join(LOCK_FILE_NAME),
        }
    }

    /// The data file path (`credentials.json` inside the store directory).
    pub fn path(&self) -> &Path {
        &self.data_path
    }

    /// Read the material for one Managed Credential.
    ///
    /// This returns secret material and is for execution paths only;
    /// discovery and `auth list` use [`Self::metadata`], which never returns
    /// material.
    pub fn get(
        &self,
        reference: &CredentialRef,
    ) -> Result<Option<ManagedCredential>, CredentialStoreError> {
        Ok(self.read()?.credentials.get(reference).cloned())
    }

    /// Presence, kind, expiry state, and account identity for one Managed
    /// Credential, without any secret material (spec rule 9).
    pub fn metadata(
        &self,
        reference: &CredentialRef,
    ) -> Result<Option<CredentialMetadata>, CredentialStoreError> {
        let now = now_unix();
        Ok(self
            .read()?
            .credentials
            .get(reference)
            .map(|credential| credential.metadata(now)))
    }

    /// Every stored Credential Reference with its metadata, sorted by
    /// reference. Metadata carries no secret material.
    pub fn list(&self) -> Result<Vec<(CredentialRef, CredentialMetadata)>, CredentialStoreError> {
        let now = now_unix();
        Ok(self
            .read()?
            .credentials
            .iter()
            .map(|(reference, credential)| (reference.clone(), credential.metadata(now)))
            .collect())
    }

    /// Store an API-key Managed Credential, replacing any record at
    /// `reference`. An empty key is not a credential and is rejected. The
    /// record is unbound; [`Self::set_api_key_for`] records the destination
    /// integration instead.
    pub fn set_api_key(
        &self,
        reference: &CredentialRef,
        key: &str,
    ) -> Result<(), CredentialStoreError> {
        if key.is_empty() {
            return Err(CredentialStoreError::InvalidMaterial(
                "an API key cannot be empty",
            ));
        }
        self.update(|credentials| {
            credentials.insert(reference.clone(), ManagedCredential::api(key));
            Ok(())
        })
    }

    /// Store an API-key Managed Credential bound to `destination`: the
    /// integration it was created for. Resolution through a different
    /// integration's binding is refused, so a config endpoint change cannot
    /// inherit a credential recorded for another destination (spec trust
    /// boundary).
    pub fn set_api_key_for(
        &self,
        reference: &CredentialRef,
        key: &str,
        destination: &IntegrationId,
    ) -> Result<(), CredentialStoreError> {
        if key.is_empty() {
            return Err(CredentialStoreError::InvalidMaterial(
                "an API key cannot be empty",
            ));
        }
        self.update(|credentials| {
            credentials.insert(
                reference.clone(),
                ManagedCredential::api_for(key, destination.clone()),
            );
            Ok(())
        })
    }

    /// Store an OAuth Managed Credential, replacing any record at
    /// `reference`. A `tokens.refresh` of `None` preserves a stored refresh
    /// token rather than deleting it (spec rule 4); [`Self::remove`] is the
    /// only way to drop a grant.
    pub fn set_oauth(
        &self,
        reference: &CredentialRef,
        tokens: OAuthTokens,
    ) -> Result<(), CredentialStoreError> {
        if tokens.access.is_empty() {
            return Err(CredentialStoreError::InvalidMaterial(
                "an OAuth access token cannot be empty",
            ));
        }
        self.update(|credentials| {
            credentials.insert(reference.clone(), ManagedCredential::oauth(tokens));
            Ok(())
        })
    }

    /// Store a browser-session Managed Credential bound to `destination`.
    ///
    /// Sessions are single records: they do not participate in Key Pools,
    /// and re-storing overwrites the record so a refreshed cookie replaces
    /// the stale one. `key` holds whatever session material the user
    /// pasted - a bare token or a full `Cookie` header line; delivery
    /// normalizes it at send time.
    pub fn set_session_for(
        &self,
        reference: &CredentialRef,
        key: &str,
        destination: &IntegrationId,
    ) -> Result<(), CredentialStoreError> {
        if key.is_empty() {
            return Err(CredentialStoreError::InvalidMaterial(
                "a session credential cannot be empty",
            ));
        }
        self.update(|credentials| {
            credentials.insert(
                reference.clone(),
                ManagedCredential::session_for(key, destination.clone()),
            );
            Ok(())
        })
    }

    /// Store a browser-session Managed Credential under a raw `reference`
    /// with no recorded destination.
    pub fn set_session(
        &self,
        reference: &CredentialRef,
        key: &str,
    ) -> Result<(), CredentialStoreError> {
        if key.is_empty() {
            return Err(CredentialStoreError::InvalidMaterial(
                "a session credential cannot be empty",
            ));
        }
        self.update(|credentials| {
            credentials.insert(reference.clone(), ManagedCredential::session(key));
            Ok(())
        })
    }

    /// Delete one Managed Credential. Returns whether it was present.
    pub fn remove(&self, reference: &CredentialRef) -> Result<bool, CredentialStoreError> {
        self.update(|credentials| Ok(credentials.remove(reference).is_some()))
    }

    /// A locked read-modify-write over the decoded credential map.
    ///
    /// The sequence is the spec's mutation transaction: acquire the sidecar
    /// lock, reread the store, apply `mutate`, preserve stored refresh tokens
    /// the mutation omitted, persist via temporary sibling and atomic rename,
    /// then release. Rereading under the lock lets a recheck observe work a
    /// concurrent process already committed. A `mutate` error aborts the
    /// transaction before persist, so a failed mutation never lands partial
    /// state.
    ///
    /// `mutate` runs under the lock, so keep it bounded; a bounded OAuth
    /// refresh belongs inside it (spec rule 2) so refresh contenders
    /// serialize.
    pub fn update<R>(
        &self,
        mutate: impl FnOnce(
            &mut BTreeMap<CredentialRef, ManagedCredential>,
        ) -> Result<R, CredentialStoreError>,
    ) -> Result<R, CredentialStoreError> {
        let _guard = StoreLock::acquire(&self.lock_path)?;
        let mut file = self.read()?;
        let before = file.credentials.clone();
        let result = mutate(&mut file.credentials)?;
        preserve_refresh_tokens(&before, &mut file.credentials);
        write_file(&self.data_path, &file)?;
        Ok(result)
    }

    /// Resolve the credential material an [`AuthBinding`] applies to a
    /// request for `integration`.
    ///
    /// An env-var source is honored only because the binding declares it;
    /// there is no global name guessing. A managed credential that recorded
    /// a different destination at write time is refused rather than sent to
    /// an endpoint it was not created for. An expired OAuth grant resolves
    /// to its access token with `needs_refresh: true`: resolve never
    /// refreshes inline, because refresh is a separate transaction the
    /// caller routes through the OAuth flow layer.
    pub fn resolve(
        &self,
        binding: &AuthBinding,
        integration: &IntegrationId,
    ) -> Result<ResolvedAuth, CredentialStoreError> {
        match binding {
            AuthBinding::None => Ok(ResolvedAuth::None),
            AuthBinding::ApiKey { source, delivery } => {
                // Cookie delivery binds session material: the single stored
                // record or declared variable, never the Key Pool - a
                // session does not rotate.
                if matches!(delivery, KeyDelivery::Cookie { .. }) {
                    return Ok(ResolvedAuth::ApiKey {
                        key: self.resolve_session(source, integration)?,
                        delivery: delivery.clone(),
                    });
                }
                let pool = self.resolve_api_key_pool(source, integration)?;
                // Single-key callers (monitoring, diagnostics) get the best
                // available member: the first healthy key, else the first
                // member so a status check still reports the provider's own
                // answer. The execution path iterates the full pool itself.
                let member = pool
                    .iter()
                    .find(|member| member.healthy())
                    .unwrap_or(&pool[0]);
                Ok(ResolvedAuth::ApiKey {
                    key: member.key.clone(),
                    delivery: delivery.clone(),
                })
            }
            AuthBinding::OAuth { credential, .. } => match self.get(credential)? {
                Some(credential_record) => {
                    check_destination(credential, &credential_record, integration)?;
                    match credential_record {
                        ManagedCredential::OAuth {
                            access, expires, ..
                        } => Ok(ResolvedAuth::OAuth {
                            access_token: access,
                            needs_refresh: expires.is_some_and(|at| at <= now_unix()),
                        }),
                        other => Err(CredentialStoreError::UnexpectedCredentialKind {
                            reference: credential.clone(),
                            expected: CredentialKind::OAuth,
                            found: other.kind(),
                        }),
                    }
                }
                None => Err(CredentialStoreError::CredentialAbsent(credential.clone())),
            },
        }
    }

    /// Reread and decode the data file. A missing file is an empty store; a
    /// malformed one is [`CredentialStoreError::Corrupt`].
    fn read(&self) -> Result<CredentialFile, CredentialStoreError> {
        let bytes = match fs::read(&self.data_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(CredentialFile::empty());
            }
            Err(error) => return Err(CredentialStoreError::Io(error)),
        };
        parse_file(&bytes)
    }
}

/// The value of a declared environment variable, or `None` when it is unset,
/// empty, not Unicode, or when `var` is not a legal name. An invalid name is
/// treated as absent rather than panicking: `std::env::var` panics on empty
/// names and names containing `=` or NUL, and a malformed `providers.json`
/// must degrade to a missing credential, not a crash.
pub fn env_override(var: &str) -> Option<String> {
    if !valid_env_var_name(var) {
        return None;
    }
    env::var(var).ok().filter(|value| !value.is_empty())
}

/// Whether `name` can be looked up without panicking: non-empty and free of
/// `=` and NUL, matching the `std::env::var` contract.
pub fn valid_env_var_name(name: &str) -> bool {
    !name.is_empty() && !name.bytes().any(|byte| byte == b'=' || byte == 0)
}

/// Spec trust boundary: a managed credential bound to a destination refuses
/// resolution through a different integration's binding. Unbound records
/// (no recorded destination) resolve for any binding that names them - the
/// config `api-key-ref` is then the user's explicit claim.
fn check_destination(
    reference: &CredentialRef,
    credential: &ManagedCredential,
    integration: &IntegrationId,
) -> Result<(), CredentialStoreError> {
    match credential.destination() {
        Some(bound_to) if bound_to != integration => {
            Err(CredentialStoreError::CredentialDestinationMismatch {
                reference: reference.clone(),
                bound_to: bound_to.clone(),
                requested: integration.clone(),
            })
        }
        _ => Ok(()),
    }
}

/// The credential material a request builder applies to one request.
///
/// `Debug` redacts the material.
#[derive(Clone, PartialEq, Eq)]
pub enum ResolvedAuth {
    /// The binding declares no credential.
    None,
    /// An API key plus the delivery that puts it on the wire.
    ApiKey { key: String, delivery: KeyDelivery },
    /// An OAuth access token. `needs_refresh` marks a grant whose declared
    /// expiry is at or in the past.
    OAuth {
        access_token: String,
        needs_refresh: bool,
    },
}

impl fmt::Debug for ResolvedAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("ResolvedAuth::None"),
            Self::ApiKey { delivery, .. } => f
                .debug_struct("ResolvedAuth::ApiKey")
                .field("key", &"<redacted>")
                .field("delivery", delivery)
                .finish(),
            Self::OAuth { needs_refresh, .. } => f
                .debug_struct("ResolvedAuth::OAuth")
                .field("access_token", &"<redacted>")
                .field("needs_refresh", needs_refresh)
                .finish(),
        }
    }
}

/// Decode a store file, checking the schema version before the full parse so
/// a newer store reports its version rather than a misleading shape error.
fn parse_file(bytes: &[u8]) -> Result<CredentialFile, CredentialStoreError> {
    let probe: VersionProbe = serde_json::from_slice(bytes).map_err(corrupt)?;
    if probe.schema_version != CREDENTIALS_SCHEMA_VERSION {
        return Err(CredentialStoreError::UnknownSchemaVersion {
            found: probe.schema_version,
        });
    }
    serde_json::from_slice(bytes).map_err(corrupt)
}

/// A sanitized corruption report. Serde error text can echo stored strings,
/// so only the error class and position are surfaced, never file content.
fn corrupt(error: serde_json::Error) -> CredentialStoreError {
    let class = match error.classify() {
        serde_json::error::Category::Io => "unreadable data",
        serde_json::error::Category::Syntax => "invalid JSON",
        serde_json::error::Category::Data => "data does not match the credential schema",
        serde_json::error::Category::Eof => "truncated JSON",
    };
    CredentialStoreError::Corrupt {
        detail: format!("{class} at line {} column {}", error.line(), error.column()),
    }
}

fn write_file(path: &Path, file: &CredentialFile) -> Result<(), CredentialStoreError> {
    let bytes = serde_json::to_vec_pretty(file)
        .map_err(|error| CredentialStoreError::Io(io::Error::other(error)))?;
    atomic_replace(path, &bytes)
}

/// Spec rule 4: a written record that omits `refresh` keeps the stored
/// refresh token. A missing field is not a deletion; only an explicit
/// [`CredentialStore::remove`] drops a grant.
fn preserve_refresh_tokens(
    before: &BTreeMap<CredentialRef, ManagedCredential>,
    after: &mut BTreeMap<CredentialRef, ManagedCredential>,
) {
    for (reference, credential) in after.iter_mut() {
        let ManagedCredential::OAuth { refresh, .. } = credential else {
            continue;
        };
        if refresh.is_some() {
            continue;
        }
        if let Some(ManagedCredential::OAuth {
            refresh: stored, ..
        }) = before.get(reference)
        {
            *refresh = stored.clone();
        }
    }
}

/// The failures a Credential Store operation can report. Messages never
/// contain credential material; Credential References and variable names are
/// safe to surface.
#[derive(Debug)]
pub enum CredentialStoreError {
    /// A filesystem or lock operation failed.
    Io(io::Error),
    /// The store file is malformed. It is never overwritten on parse
    /// failure (spec rule 5).
    Corrupt { detail: String },
    /// The store declares a schema version this build does not know (spec
    /// rule 10).
    UnknownSchemaVersion { found: u32 },
    /// The sidecar lock stayed held past the bounded wait.
    LockedTimeout,
    /// A referenced Managed Credential is not in the store.
    CredentialAbsent(CredentialRef),
    /// A referenced credential exists but is the wrong kind for the binding.
    UnexpectedCredentialKind {
        reference: CredentialRef,
        expected: CredentialKind,
        found: CredentialKind,
    },
    /// A referenced credential was created for a different integration and
    /// must not be sent to this one's endpoint.
    CredentialDestinationMismatch {
        reference: CredentialRef,
        bound_to: IntegrationId,
        requested: IntegrationId,
    },
    /// A declared environment variable is unset or empty.
    EnvVarAbsent { var: String },
    /// The material presented for storage is not a credential.
    InvalidMaterial(&'static str),
}

impl fmt::Display for CredentialStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "credential store operation failed: {error}"),
            Self::Corrupt { detail } => write!(
                f,
                "credential store is malformed ({detail}); it was left untouched - restore a backup or delete it to start over"
            ),
            Self::UnknownSchemaVersion { found } => write!(
                f,
                "credential store schema version {found} is unknown to this build (supports {CREDENTIALS_SCHEMA_VERSION})"
            ),
            Self::LockedTimeout => f.write_str("timed out waiting for the credential store lock"),
            Self::CredentialAbsent(reference) => {
                write!(f, "no managed credential is stored under '{reference}'")
            }
            Self::UnexpectedCredentialKind {
                reference,
                expected,
                found,
            } => write!(
                f,
                "managed credential '{reference}' is {found}, not {expected}"
            ),
            Self::CredentialDestinationMismatch {
                reference,
                bound_to,
                requested,
            } => write!(
                f,
                "managed credential '{reference}' was created for integration '{bound_to}' and cannot be sent to '{requested}'"
            ),
            Self::EnvVarAbsent { var } => {
                write!(f, "environment variable '{var}' is unset or empty")
            }
            Self::InvalidMaterial(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for CredentialStoreError {}

impl From<io::Error> for CredentialStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
