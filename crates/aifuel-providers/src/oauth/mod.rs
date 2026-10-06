//! The OAuth flow layer: AI Fuel-owned login and refresh for Managed
//! Credentials, as `docs/specs/provider-integrations.md` prescribes.
//!
//! - Profiles are a closed compile-time set ([`profiles`]): config selects
//!   a profile, it cannot invent flow behavior.
//! - `aifuel auth login` runs [`begin_login`]/[`PendingLogin::wait`] to
//!   mint an `OAuthTokens` grant the caller stores under a Credential
//!   Reference bound to the profile's integration.
//! - [`resolve_ready_with_env`] is `CredentialStore::resolve` plus the
//!   refresh transaction: an expired grant refreshes inside the store's
//!   sidecar-lock mutation so contenders serialize, re-read under the
//!   lock, and a refresh token the response omits is preserved.
//! - Endpoint fields pass through `AIFUEL_OAUTH_*` overrides so end-to-end
//!   tests can point a flow at a stub authorization server; flow mechanics
//!   themselves take resolved params and never read the environment.

mod device;
pub(crate) mod http;
mod pkce;
mod spec;

use crate::credentials::{
    CredentialKind, CredentialStore, CredentialStoreError, ManagedCredential, OAuthTokens,
    ResolvedAuth, env_override,
};
use crate::wire::http as wire;
use aifuel_core::{AuthBinding, CredentialRef, IntegrationId};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub use device::DeviceLogin;
pub use pkce::LoopbackLogin;
pub use spec::*;

/// The bound codex-rs puts on both its login flavors; GitHub device codes
/// carry their own `expires_in`, honored inside the flow itself.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// A started login: the user-facing instructions plus the mechanics to
/// finish the grant once the user acts.
pub enum PendingLogin {
    /// An RFC 8628 device-code poll.
    Device(DeviceLogin),
    /// A loopback listener waiting for the browser redirect.
    Loopback(LoopbackLogin),
    /// An OpenAI `deviceauth` poll.
    DeviceAuth(device::DeviceAuthLogin),
}

/// What the user must do for a [`PendingLogin`] to complete.
pub enum LoginInstructions {
    /// Open the page and type the one-time code.
    EnterCode {
        verification_uri: String,
        user_code: String,
    },
    /// Open the URL in a browser on this machine.
    OpenBrowser { authorization_url: String },
}

impl PendingLogin {
    /// What to show the user before [`Self::wait`] blocks on their action.
    pub fn instructions(&self) -> LoginInstructions {
        match self {
            Self::Device(login) => LoginInstructions::EnterCode {
                verification_uri: login.verification_uri.clone(),
                user_code: login.user_code.clone(),
            },
            Self::DeviceAuth(login) => LoginInstructions::EnterCode {
                verification_uri: login.verification_uri.clone(),
                user_code: login.user_code.clone(),
            },
            Self::Loopback(login) => LoginInstructions::OpenBrowser {
                authorization_url: login.authorization_url.clone(),
            },
        }
    }

    /// Block until the user completes or abandons the flow, then return
    /// the minted grant material (destination unset; the caller binds it).
    pub fn wait(self) -> Result<OAuthTokens, String> {
        match self {
            Self::Device(login) => login.wait(),
            Self::Loopback(login) => login.wait(),
            Self::DeviceAuth(login) => login.wait(),
        }
    }
}

/// Start one of a spec's compiled flows: request the device code or bind
/// the loopback listener and build the authorization URL.
pub fn begin_login(spec: &'static OAuthFlowSpec, flow: &OAuthFlow) -> Result<PendingLogin, String> {
    match flow_params(spec, flow) {
        FlowParams::Device(params) => device::begin(spec, params).map(PendingLogin::Device),
        FlowParams::Loopback(params) => pkce::begin(spec, params).map(PendingLogin::Loopback),
        FlowParams::DeviceAuth(params) => {
            device::begin_device_auth(spec, params).map(PendingLogin::DeviceAuth)
        }
    }
}

/// The login timeout a flow should observe when the provider does not
/// declare its own expiry.
pub(crate) fn login_timeout() -> Duration {
    LOGIN_TIMEOUT
}

/// The instant a device grant or callback wait must give up.
pub(crate) fn login_deadline() -> Instant {
    Instant::now() + LOGIN_TIMEOUT
}

/// The managed OAuth grant a `*:oauth` adapter prefers over the
/// provider's own credential files: absent yields `None`, a record of the
/// wrong kind or bound to another destination fails closed - the grant
/// must never be sent to an endpoint it was not minted for.
pub(crate) fn managed_oauth_grant(
    store: &CredentialStore,
    reference: &CredentialRef,
) -> Result<Option<ManagedCredential>, CredentialStoreError> {
    let Some(record) = store.get(reference)? else {
        return Ok(None);
    };
    if record.kind() != CredentialKind::OAuth {
        return Err(CredentialStoreError::UnexpectedCredentialKind {
            reference: reference.clone(),
            expected: CredentialKind::OAuth,
            found: record.kind(),
        });
    }
    let ManagedCredential::OAuth { destination, .. } = &record else {
        unreachable!("the kind check above settles it");
    };
    if let Some(bound) = destination
        && bound.as_str() != reference.as_str()
    {
        return Err(CredentialStoreError::CredentialDestinationMismatch {
            reference: reference.clone(),
            bound_to: bound.clone(),
            requested: IntegrationId::new(reference.as_str()),
        });
    }
    Ok(Some(record))
}

/// `CredentialStore::resolve` plus the managed-OAuth refresh transaction.
///
/// A resolved grant whose expiry is already past refreshes inside a store
/// `update`: contenders serialize on the sidecar lock, the reread lets a
/// second process observe the first's refresh and skip its own, and an
/// omitted replacement refresh token preserves the stored one. All other
/// bindings pass through unchanged.
pub fn resolve_ready(
    store: &CredentialStore,
    binding: &AuthBinding,
    integration: &IntegrationId,
) -> Result<ResolvedAuth, CredentialStoreError> {
    resolve_ready_with_env(store, binding, integration, &BTreeMap::new())
}

/// [`resolve_ready`] against the instance's resolved environment overlay,
/// mirroring `CredentialStore::resolve_with_env`.
pub fn resolve_ready_with_env(
    store: &CredentialStore,
    binding: &AuthBinding,
    integration: &IntegrationId,
    env: &BTreeMap<String, String>,
) -> Result<ResolvedAuth, CredentialStoreError> {
    let resolved = store.resolve_with_env(binding, integration, env)?;
    let (
        AuthBinding::OAuth {
            credential,
            profile: profile_id,
        },
        ResolvedAuth::OAuth {
            needs_refresh: true,
            ..
        },
    ) = (binding, &resolved)
    else {
        return Ok(resolved);
    };
    let spec = profile(profile_id).ok_or_else(|| CredentialStoreError::OAuthRefreshFailed {
        reference: credential.clone(),
        detail: format!("OAuth profile '{profile_id}' is not compiled into this build"),
    })?;
    refresh_grant(store, credential, spec)?;
    store.resolve_with_env(binding, integration, env)
}

/// The refresh transaction: under the sidecar lock, reread the grant,
/// skip when another writer already refreshed it, exchange the stored
/// refresh token, and persist the rotated pair (spec rules 2-4). Compiled
/// `*:oauth` adapters run the same transaction for their managed grants.
pub(crate) fn refresh_grant(
    store: &CredentialStore,
    reference: &CredentialRef,
    spec: &OAuthFlowSpec,
) -> Result<(), CredentialStoreError> {
    let token_url =
        env_override("AIFUEL_OAUTH_TOKEN_URL").unwrap_or_else(|| spec.token_url.to_owned());
    refresh_grant_at(store, reference, spec, &token_url)
}

/// [`refresh_grant`] against an explicit token endpoint - the env override
/// or spec constant at runtime, a stub server in tests.
fn refresh_grant_at(
    store: &CredentialStore,
    reference: &CredentialRef,
    spec: &OAuthFlowSpec,
    token_url: &str,
) -> Result<(), CredentialStoreError> {
    let token_url = token_url.to_owned();
    store.update(|credentials| {
        let Some(ManagedCredential::OAuth {
            refresh,
            expires,
            account_id,
            destination,
            ..
        }) = credentials.get(reference).cloned()
        else {
            return Err(CredentialStoreError::CredentialAbsent(reference.clone()));
        };
        if expires.is_none_or(|at| at > http::unix_now() as i64) {
            // A competing refresh already landed, or the grant carries no
            // declared expiry to be stale against.
            return Ok(());
        }
        let Some(refresh) = refresh else {
            return Err(CredentialStoreError::OAuthRefreshFailed {
                reference: reference.clone(),
                detail: "the access token expired and the grant holds no refresh token; \
                         run `aifuel auth login` again"
                    .to_owned(),
            });
        };
        let token_url = token_url.clone();
        let refreshed =
            run_blocking(|| refresh_exchange(spec, &token_url, &refresh)).map_err(|detail| {
                CredentialStoreError::OAuthRefreshFailed {
                    reference: reference.clone(),
                    detail,
                }
            })?;
        credentials.insert(
            reference.clone(),
            ManagedCredential::oauth(OAuthTokens {
                access: refreshed.access,
                // `None` here means the provider rotated no refresh token;
                // `preserve_refresh_tokens` keeps the stored one.
                refresh: refreshed.refresh,
                expires: refreshed.expires,
                account_id,
                destination,
            }),
        );
        Ok(())
    })
}

/// POST a refresh grant to the profile's token endpoint. Codex's backend
/// expects JSON (matching codex-rs); GitHub-style endpoints take a form.
///
/// Spec rule 6 bounds the ambiguity: a connect failure means the request
/// never left - one immediate retry is safe; any later transport failure
/// means the grant may already be consumed, so a single confirm exchange
/// verifies the outcome rather than a blind retry loop. A confirm that
/// itself fails resolves to re-authentication.
async fn refresh_exchange(
    spec: &OAuthFlowSpec,
    token_url: &str,
    refresh_token: &str,
) -> Result<OAuthTokens, String> {
    let client = wire::build_client()
        .map_err(|error| format!("the OAuth refresh client could not start: {error}"))?;
    let post = |client: &reqwest::Client| {
        let builder = client.post(token_url).timeout(http::SEND_TIMEOUT);
        if spec.refresh_form {
            builder.form(&[
                ("client_id", spec.client_id),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
            ])
        } else {
            builder.json(&serde_json::json!({
                "client_id": spec.client_id,
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
            }))
        }
    };
    let response = match post(&client).send().await {
        Ok(response) => response,
        Err(error) if error.is_connect() => post(&client)
            .send()
            .await
            .map_err(|error| format!("the refresh request failed: {error}"))?,
        Err(_) => {
            // Possibly sent: one bounded confirm learns whether the grant
            // rotated rather than replaying it inside a retry loop.
            tokio::time::sleep(Duration::from_millis(500)).await;
            post(&client).send().await.map_err(|error| {
                format!(
                    "the refresh request's outcome is unknown and the confirm failed: {error}; \
                     run `aifuel auth login` again"
                )
            })?
        }
    };
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "the token endpoint answered HTTP {status}; run `aifuel auth login` again"
        ));
    }
    let tokens: TokenResponse = response
        .json()
        .await
        .map_err(|error| format!("the token endpoint's response is not JSON: {error}"))?;
    if tokens.access_token.is_empty() {
        return Err("the token endpoint returned no access token".to_owned());
    }
    Ok(tokens_from_response(spec, tokens))
}

/// Drive an async OAuth exchange from synchronous code - a credential
/// transaction or a CLI command - on a scoped worker's single-threaded
/// runtime, mirroring `http::block_on_run`.
pub(crate) fn run_blocking<Fut>(build: impl FnOnce() -> Fut + Send) -> Fut::Output
where
    Fut: std::future::Future + Send,
    Fut::Output: Send,
{
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("oauth runtime")
                    .block_on(build())
            })
            .join()
            .expect("oauth worker panicked")
    })
}

#[cfg(test)]
mod tests;
