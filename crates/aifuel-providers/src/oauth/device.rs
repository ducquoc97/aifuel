//! Device-class login flows: RFC 8628 device authorization (GitHub, for
//! `copilot`) and OpenAI's `deviceauth` grant (Codex's headless flavor).
//!
//! Both are synchronous: [`super::run_blocking`] drives the reqwest calls
//! on a scoped worker so `aifuel auth login` stays a plain blocking
//! command. Device codes and returned tokens never appear in errors.

use super::{
    DeviceAuthParams, DeviceFlowParams, OAuthFlowSpec, TokenResponse, http, login_deadline,
    login_timeout, run_blocking, tokens_from_response,
};
use crate::credentials::OAuthTokens;
use crate::wire::http as wire;
use serde::Deserialize;
use std::time::{Duration, Instant};

/// An RFC 8628 device grant in progress: the code request has been made
/// and [`Self::wait`] polls until the user authorizes, denies, or the
/// device code expires.
pub struct DeviceLogin {
    /// The page the user opens.
    pub verification_uri: String,
    /// The one-time code the user types there.
    pub user_code: String,
    spec: &'static OAuthFlowSpec,
    params: DeviceFlowParams,
    device_code: String,
    interval: Duration,
    deadline: Instant,
}

impl DeviceLogin {
    /// Poll the token endpoint until authorization completes. RFC 8628
    /// pending states (`authorization_pending`, `slow_down`) keep polling;
    /// terminal states abort. A failed HTTP status ends the flow rather
    /// than risking a double-consumed device code.
    pub fn wait(self) -> Result<OAuthTokens, String> {
        let spec = self.spec;
        run_blocking(move || async move {
            let params = &self.params;
            let client = wire::build_client()
                .map_err(|error| format!("the OAuth client could not start: {error}"))?;
            let mut interval = self.interval;
            loop {
                if Instant::now() >= self.deadline {
                    return Err("the device code expired before authorization completed; \
                         run `aifuel auth login` again"
                        .to_owned());
                }
                std::thread::sleep(
                    interval.min(self.deadline.saturating_duration_since(Instant::now())),
                );
                let response = client
                    .post(&params.token_url)
                    .headers(headers(&params.headers))
                    .form(&[
                        ("client_id", params.client_id.as_str()),
                        ("device_code", self.device_code.as_str()),
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ])
                    .timeout(http::SEND_TIMEOUT)
                    .send()
                    .await
                    .map_err(|error| format!("the device poll request failed: {error}"))?;
                let status = response.status();
                let grant: TokenResponse = response.json().await.map_err(|error| {
                    format!(
                        "the token endpoint answered HTTP {status} with a non-JSON body: {error}"
                    )
                })?;
                // GitHub serves pending, slow-down, denied, and expired
                // polls with HTTP 200 - the `error` field drives the loop,
                // not the status code.
                match grant.error.as_deref() {
                    Some("authorization_pending") => {}
                    Some("slow_down") => interval += Duration::from_secs(5),
                    Some("access_denied") => {
                        return Err("the authorization was denied on the device page".to_owned());
                    }
                    Some("expired_token") => {
                        return Err(
                            "the device code expired; run `aifuel auth login` again".to_owned()
                        );
                    }
                    Some(other) => return Err(format!("the device flow failed: {other}")),
                    None if grant.access_token.is_empty() => {
                        return Err("the token endpoint returned no access token".to_owned());
                    }
                    None => return Ok(tokens_from_response(spec, grant)),
                }
            }
        })
    }
}

/// An RFC 8628 `device/code` response.
#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default = "default_poll_interval")]
    interval: u64,
    #[serde(default)]
    expires_in: Option<u64>,
}

fn default_poll_interval() -> u64 {
    5
}

/// Request the device code the user will type on the verification page.
pub fn begin(
    spec: &'static OAuthFlowSpec,
    params: DeviceFlowParams,
) -> Result<DeviceLogin, String> {
    run_blocking(move || async move {
        let client = wire::build_client()
            .map_err(|error| format!("the OAuth client could not start: {error}"))?;
        let response = client
            .post(&params.device_url)
            .headers(headers(&params.headers))
            .form(&[
                ("client_id", params.client_id.as_str()),
                ("scope", params.scope.as_str()),
            ])
            .timeout(http::SEND_TIMEOUT)
            .send()
            .await
            .map_err(|error| format!("the device code request failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "the device code endpoint answered HTTP {}",
                response.status()
            ));
        }
        let code: DeviceCodeResponse = response
            .json()
            .await
            .map_err(|error| format!("the device code endpoint's response is not JSON: {error}"))?;
        if code.device_code.is_empty() || code.user_code.is_empty() {
            return Err("the device code endpoint returned an incomplete code".to_owned());
        }
        let deadline = code
            .expires_in
            .map(|seconds| Instant::now() + Duration::from_secs(seconds))
            .map(|expires| expires.min(login_deadline()))
            .unwrap_or_else(login_deadline);
        Ok(DeviceLogin {
            verification_uri: code.verification_uri,
            user_code: code.user_code,
            device_code: code.device_code,
            interval: Duration::from_secs(code.interval.max(1)),
            deadline,
            spec,
            params,
        })
    })
}

/// The request headers a device flow declares: GitHub wants the editor
/// identity fields on both the code request and every poll.
fn headers(declared: &[(String, String)]) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    for (name, value) in declared {
        if let (Ok(name), Ok(value)) = (
            name.parse::<reqwest::header::HeaderName>(),
            value.parse::<reqwest::header::HeaderValue>(),
        ) {
            map.insert(name, value);
        }
    }
    map
}

// --- OpenAI `deviceauth` (codex-rs `device_code_auth.rs`) ---

/// An OpenAI headless device grant in progress. The provider returns the
/// PKCE pair alongside the authorization code, so completion is a plain
/// code exchange - no loopback listener, no locally minted verifier.
pub struct DeviceAuthLogin {
    /// `{issuer}/codex/device` - the page the user opens.
    pub verification_uri: String,
    /// The one-time code the user types there.
    pub user_code: String,
    spec: &'static OAuthFlowSpec,
    params: DeviceAuthParams,
    device_auth_id: String,
    interval: Duration,
    deadline: Instant,
}

#[derive(Deserialize)]
struct UserCodeResponse {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(default = "default_poll_interval")]
    interval: u64,
}

#[derive(Deserialize)]
struct DeviceCodeSuccess {
    authorization_code: String,
    code_verifier: String,
}

/// POST `{issuer}/api/accounts/deviceauth/usercode` for the code the user
/// types on the device page.
pub fn begin_device_auth(
    spec: &'static OAuthFlowSpec,
    params: DeviceAuthParams,
) -> Result<DeviceAuthLogin, String> {
    run_blocking(move || async move {
        let client = wire::build_client()
            .map_err(|error| format!("the OAuth client could not start: {error}"))?;
        let issuer = params.issuer.trim_end_matches('/');
        let url = format!("{issuer}/api/accounts/deviceauth/usercode");
        let response = client
            .post(&url)
            .json(&serde_json::json!({ "client_id": params.client_id }))
            .timeout(http::SEND_TIMEOUT)
            .send()
            .await
            .map_err(|error| format!("the device code request failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "the device code endpoint answered HTTP {}",
                response.status()
            ));
        }
        let code: UserCodeResponse = response
            .json()
            .await
            .map_err(|error| format!("the device code response is not JSON: {error}"))?;
        if code.user_code.is_empty() || code.device_auth_id.is_empty() {
            return Err("the device code endpoint returned an incomplete code".to_owned());
        }
        Ok(DeviceAuthLogin {
            verification_uri: format!("{issuer}/codex/device"),
            user_code: code.user_code,
            device_auth_id: code.device_auth_id,
            interval: Duration::from_secs(code.interval.max(1)),
            deadline: Instant::now() + login_timeout(),
            spec,
            params,
        })
    })
}

impl DeviceAuthLogin {
    /// Poll `{issuer}/api/accounts/deviceauth/token` (pending: `403`/`404`),
    /// then exchange the returned authorization code - with the
    /// server-issued PKCE verifier - against the token endpoint.
    pub fn wait(self) -> Result<OAuthTokens, String> {
        let spec = self.spec;
        run_blocking(move || async move {
            let params = &self.params;
            let client = wire::build_client()
                .map_err(|error| format!("the OAuth client could not start: {error}"))?;
            let issuer = params.issuer.trim_end_matches('/');
            let poll_url = format!("{issuer}/api/accounts/deviceauth/token");
            loop {
                if Instant::now() >= self.deadline {
                    return Err(
                        "the device code timed out; run `aifuel auth login` again".to_owned()
                    );
                }
                std::thread::sleep(
                    self.interval
                        .min(self.deadline.saturating_duration_since(Instant::now())),
                );
                let response = client
                    .post(&poll_url)
                    .json(&serde_json::json!({
                        "device_auth_id": self.device_auth_id,
                        "user_code": self.user_code,
                    }))
                    .timeout(http::SEND_TIMEOUT)
                    .send()
                    .await
                    .map_err(|error| format!("the device poll request failed: {error}"))?;
                let status = response.status();
                if matches!(status.as_u16(), 403 | 404) {
                    continue;
                }
                if !status.is_success() {
                    return Err(format!("the device grant failed with HTTP {status}"));
                }
                let code: DeviceCodeSuccess = response
                    .json()
                    .await
                    .map_err(|error| format!("the device grant response is not JSON: {error}"))?;
                let redirect_uri = format!("{issuer}/deviceauth/callback");
                return super::pkce::exchange_code(
                    &client,
                    spec,
                    &params.token_url,
                    &params.client_id,
                    &redirect_uri,
                    &code.authorization_code,
                    &code.code_verifier,
                )
                .await;
            }
        })
    }
}
