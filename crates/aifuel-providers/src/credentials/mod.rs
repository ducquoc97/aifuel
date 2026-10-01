//! The AI Fuel Credential Store for Managed Credentials: BYOK API keys and
//! OAuth grants that AI Fuel owns and maintains on the user's behalf.
//!
//! Invariants per `docs/specs/provider-integrations.md`, "Credential Store
//! Contract":
//!
//! - Data lives in `credentials.json` under a caller-provided directory. The
//!   record schema follows the opencode `auth.json` shape (a map from
//!   Credential Reference to a `type`-tagged record) wrapped with an explicit
//!   `schema_version` that is rejected when unknown.
//! - Mutations serialize on the `credentials.json.lock` sidecar file, never
//!   the data file, because atomic rename swaps the data file's inode.
//! - Every mutation rereads the store under the lock, applies the change,
//!   then writes a temporary sibling and atomically renames it over the data
//!   file. A stored refresh token survives any rewrite that omits it.
//! - A malformed file fails with [`CredentialStoreError::Corrupt`] and is
//!   never overwritten or treated as empty.
//! - Credential material never appears in `Debug` output or error messages.
//! - API-key records whose Credential Reference extends a binding's
//!   reference with `/suffix` form that binding's Key Pool: each member is
//!   an ordinary Managed Credential carrying optional health state
//!   (cooldown deadline, invalid mark) that the HTTP execution path
//!   maintains and `auth list` reports.
//! - All operations are synchronous blocking I/O. Async callers must run them
//!   inside `tokio::task::spawn_blocking` or an equivalent blocking context.
//! - On Unix the data file is created mode 0600 and repaired toward 0600 on
//!   every write. Windows has no portable mode bits, so the file inherits the
//!   profile directory's ACL: best-effort protection only, documented
//!   honestly per the spec. An OS credential backend remains a hardening
//!   option, not a claim.

mod lock;
mod schema;
mod store;

pub use schema::{
    ApiKeyState, CredentialExpiry, CredentialKind, CredentialMetadata, KeyHealth,
    ManagedCredential, OAuthTokens,
};
pub use store::{
    CredentialStore, CredentialStoreError, PoolKey, ResolvedAuth, env_override, is_pool_member,
    valid_env_var_name,
};

#[cfg(test)]
mod tests;
