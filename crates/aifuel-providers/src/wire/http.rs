//! Transport and credential plumbing for the wire adapter: client
//! construction, configuration validation, and error/diagnostic handling.
//! Kept separate from `mod.rs` so the adapter body reads as run orchestration
//! only.

use crate::{CredentialStoreError, ResolvedAuth};
use aifuel_core::{AgentRunError, AuthBinding, EndpointConfig, KeyDelivery};
use reqwest::header::{HeaderName, HeaderValue};
use std::io;
use std::str::FromStr;
use std::time::Duration;

use super::WireAdapterError;

/// The TCP connect bound for endpoint requests, so a dead address fails
/// fast instead of waiting out the run deadline.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The HTTP client: same-origin redirects only, and a bounded connect. A
/// redirect that changes origin fails the request rather than replaying
/// managed auth material onto a different host; reqwest would strip the
/// headers and follow, but a silent unauthenticated follow is a wrong
/// answer, not a failure.
pub(super) fn build_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let same_origin = attempt.previous().last().is_some_and(|previous| {
                let next = attempt.url();
                previous.scheme() == next.scheme()
                    && previous.host_str() == next.host_str()
                    && previous.port_or_known_default() == next.port_or_known_default()
            });
            if !same_origin {
                return attempt.error(
                    "the endpoint redirected to a different origin; managed \
                     authentication is never replayed across origins",
                );
            }
            if attempt.previous().len() >= 10 {
                return attempt.error("too many redirects");
            }
            attempt.follow()
        }))
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
}

/// Configuration checks performed once at construction so a malformed
/// endpoint fails loudly at startup rather than at first run.
pub(super) fn validate_configuration(
    endpoint: &EndpointConfig,
    auth: &AuthBinding,
) -> Result<(), WireAdapterError> {
    let url = reqwest::Url::parse(endpoint.base_url.trim_end_matches('/')).map_err(|error| {
        WireAdapterError::InvalidConfiguration(format!(
            "endpoint base URL is not parseable: {error}"
        ))
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(WireAdapterError::InvalidConfiguration(format!(
            "endpoint base URL scheme {:?} is not http or https",
            url.scheme()
        )));
    }
    for (name, value) in &endpoint.extra_headers {
        HeaderName::from_str(name).map_err(|_| {
            WireAdapterError::InvalidConfiguration(format!(
                "endpoint header {name:?} is not a valid header name"
            ))
        })?;
        HeaderValue::from_str(value).map_err(|_| {
            WireAdapterError::InvalidConfiguration(format!(
                "endpoint header {name:?} is not a valid header value"
            ))
        })?;
    }
    if let AuthBinding::ApiKey {
        delivery: KeyDelivery::Header { name },
        ..
    } = auth
        && HeaderName::from_str(name).is_err()
    {
        return Err(WireAdapterError::InvalidConfiguration(format!(
            "credential delivery header {name:?} is not a valid header name"
        )));
    }
    Ok(())
}

/// A credential resolution failure mapped onto the run error surface.
/// Missing, mismatched, or destination-bound material is a request-level
/// problem; store-level failures (I/O, corruption, lock timeout) are
/// reported as I/O.
pub(super) fn auth_error(error: CredentialStoreError) -> AgentRunError {
    match error {
        CredentialStoreError::CredentialAbsent(_)
        | CredentialStoreError::EnvVarAbsent { .. }
        | CredentialStoreError::CredentialDestinationMismatch { .. }
        | CredentialStoreError::UnexpectedCredentialKind { .. } => {
            AgentRunError::InvalidRequest(error.to_string())
        }
        other => AgentRunError::Io(io::Error::other(other)),
    }
}

/// Scrub resolved credential material out of text that will surface in
/// diagnostics. An endpoint-controlled error body can reflect the sent
/// `Authorization` value verbatim; the spec's credential boundary holds
/// even against a hostile endpoint.
pub(super) fn redact(text: String, auth: &ResolvedAuth) -> String {
    let secret = match auth {
        ResolvedAuth::ApiKey { key, .. } => Some(key.as_str()),
        ResolvedAuth::OAuth { access_token, .. } => Some(access_token.as_str()),
        ResolvedAuth::None => None,
    };
    match secret {
        Some(secret) if !secret.is_empty() => text.replace(secret, "<redacted>"),
        _ => text,
    }
}
