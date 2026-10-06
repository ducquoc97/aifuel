//! Authorization-code + PKCE login on a loopback listener (RFC 8252 and
//! RFC 7636), matching codex-rs's flow: bind the first free redirect port,
//! mint a verifier and `state`, hand the user an authorization URL, then
//! exchange the callback's `code` on the token endpoint.
//!
//! The verifier and `state` never leave this module's values - they are
//! logged nowhere and errors carry no request data. `AIFUEL_OAUTH_STATE`
//! pins `state` for end-to-end tests; it is ignored when unset.

use super::{
    LoopbackFlowParams, OAuthFlowSpec, TokenResponse, http, login_timeout, tokens_from_response,
};
use crate::credentials::OAuthTokens;
use crate::wire::http as wire;
use sha2::Digest;
use std::time::Instant;
use tiny_http::{Response, Server};

/// The callback path the provider redirects to. Codex's redirect
/// allowlist expects `/auth/callback`.
const CALLBACK_PATH: &str = "/auth/callback";

/// A bound loopback listener plus the PKCE/`state` material for one
/// authorization request. [`Self::wait`] serves the browser redirect and
/// exchanges the returned code.
pub struct LoopbackLogin {
    /// The authorization URL the user opens in a browser.
    pub authorization_url: String,
    /// The loopback URL the provider will redirect to - surfaced so tests
    /// and diagnostics can reach the listener directly.
    pub redirect_uri: String,
    spec: &'static OAuthFlowSpec,
    params: LoopbackFlowParams,
    server: Server,
    verifier: String,
    state: String,
}

/// Bind the first free allowlisted port, mint the PKCE pair and `state`,
/// and build the authorization URL.
pub fn begin(
    spec: &'static OAuthFlowSpec,
    params: LoopbackFlowParams,
) -> Result<LoopbackLogin, String> {
    let mut server = None;
    for port in &params.ports {
        if let Ok(bound) = Server::http(format!("127.0.0.1:{port}")) {
            server = Some(bound);
            break;
        }
    }
    let server = server.ok_or_else(|| {
        format!(
            "no loopback port in {} is free for the OAuth callback",
            params
                .ports
                .iter()
                .map(|port| port.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    let port = server
        .server_addr()
        .to_ip()
        .map(|ip| ip.port())
        .ok_or_else(|| "the loopback listener's port could not be read".to_owned())?;
    let redirect_uri = format!("http://localhost:{port}{CALLBACK_PATH}");
    // 64 random bytes -> 86-char verifier (codex-rs's size); SHA-256 is
    // the only challenge method OAuth providers accept.
    let verifier = http::base64url_encode(&random_bytes(64)?);
    let challenge = http::base64url_encode(&sha2::Sha256::digest(verifier.as_bytes()));
    let state = match crate::credentials::env_override("AIFUEL_OAUTH_STATE") {
        Some(state) => state,
        None => http::base64url_encode(&random_bytes(32)?),
    };
    let mut authorization_url = reqwest::Url::parse(&params.authorize_url)
        .map_err(|error| format!("the authorization endpoint is malformed: {error}"))?;
    {
        let mut query = authorization_url.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", &params.client_id);
        query.append_pair("redirect_uri", &redirect_uri);
        query.append_pair("scope", &params.scope);
        query.append_pair("code_challenge", &challenge);
        query.append_pair("code_challenge_method", "S256");
        query.append_pair("state", &state);
        for (name, value) in &params.extra_params {
            query.append_pair(name, value);
        }
    }
    Ok(LoopbackLogin {
        authorization_url: authorization_url.to_string(),
        redirect_uri,
        spec,
        params,
        server,
        verifier,
        state,
    })
}

impl LoopbackLogin {
    /// Serve the redirect until a `code`+`state` pair arrives or the
    /// login times out, then run the code exchange. Requests off the
    /// callback path get a 404; a mismatched `state` gets a 400 and the
    /// listener keeps waiting - a stray hit is not the user finishing.
    pub fn wait(self) -> Result<OAuthTokens, String> {
        let deadline = Instant::now() + login_timeout();
        let code = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("the browser sign-in timed out".to_owned());
            }
            let Some(request) = self
                .server
                .recv_timeout(remaining)
                .map_err(|error| format!("the loopback listener failed: {error}"))?
            else {
                return Err("the browser sign-in timed out".to_owned());
            };
            let (path, query) = request
                .url()
                .split_once('?')
                .map(|(path, query)| (path, query))
                .unwrap_or((request.url(), ""));
            if path != CALLBACK_PATH {
                respond(request, 404, "Not found");
                continue;
            }
            let params = parse_query(query);
            // Only a callback carrying the minted `state` may end the
            // flow - a stray local hit with `error` set must not abort a
            // sign-in still in progress.
            match params.get("state").map(String::as_str) {
                Some(state) if state == self.state => {
                    if let Some(error) = params.get("error") {
                        respond(request, 400, "Sign-in failed; return to the terminal.");
                        return Err(format!("the authorization server returned: {error}"));
                    }
                    match params.get("code") {
                        Some(code) => {
                            respond(
                                request,
                                200,
                                "Sign-in complete. You can close this tab and return to the terminal.",
                            );
                            break code.clone();
                        }
                        None => {
                            respond(request, 400, "Missing authorization code.");
                            continue;
                        }
                    }
                }
                Some(_) => {
                    respond(request, 400, "State mismatch; retry the login.");
                    continue;
                }
                None => {
                    respond(request, 400, "Missing authorization parameters.");
                    continue;
                }
            }
        };
        let spec = self.spec;
        let params = &self.params;
        super::run_blocking(move || {
            let verifier = self.verifier;
            let redirect_uri = self.redirect_uri;
            async move {
                let client = wire::build_client()
                    .map_err(|error| format!("the OAuth client could not start: {error}"))?;
                exchange_code(
                    &client,
                    spec,
                    &params.token_url,
                    &params.client_id,
                    &redirect_uri,
                    &code,
                    &verifier,
                )
                .await
            }
        })
    }
}

/// POST the authorization-code grant. Shared with OpenAI's `deviceauth`
/// completion, which supplies the server-issued PKCE verifier.
pub(crate) async fn exchange_code(
    client: &reqwest::Client,
    spec: &OAuthFlowSpec,
    token_url: &str,
    client_id: &str,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<OAuthTokens, String> {
    let response = client
        .post(token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ])
        .timeout(http::SEND_TIMEOUT)
        .send()
        .await
        .map_err(|error| format!("the token exchange request failed: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("the token endpoint answered HTTP {status}"));
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

/// The login page answer: a tiny HTML body so the browser tab reads as
/// finished rather than blank.
fn respond(request: tiny_http::Request, status: u16, message: &str) {
    let body = format!(
        "<!doctype html><html><body style=\"font-family:system-ui;margin:3em\">{message}</body></html>"
    );
    let response = Response::from_string(body).with_status_code(status);
    let _ = request.respond(response);
}

/// Parse a `key=value&...` query string with percent-decoding - only what
/// the callback needs, so no `url` dependency is pulled in.
fn parse_query(query: &str) -> std::collections::BTreeMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (percent_decode(key), percent_decode(value)))
        .collect()
}

/// Decode `%XX` escapes and `+` spaces in a form component.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' if index + 2 < bytes.len()
                && let Ok(value) = u8::from_str_radix(&input[index + 1..index + 3], 16) =>
            {
                out.push(value);
                index += 2;
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Cryptographically random bytes for the PKCE verifier and `state`.
fn random_bytes(count: usize) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0u8; count];
    getrandom::fill(&mut bytes).map_err(|error| format!("the OS random source failed: {error}"))?;
    Ok(bytes)
}
