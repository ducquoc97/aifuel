//! Transport and credential plumbing for the wire adapter: client
//! construction, configuration validation, and error/diagnostic handling.
//! Kept separate from `mod.rs` so the adapter body reads as run orchestration
//! only.

use crate::{CredentialStoreError, ResolvedAuth};
use aifuel_core::{AgentRunError, AuthBinding, EndpointConfig, KeyDelivery};
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, COOKIE, HeaderMap, HeaderName, HeaderValue, RETRY_AFTER,
    USER_AGENT,
};
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
    if let AuthBinding::ApiKey {
        delivery: KeyDelivery::Cookie { name },
        ..
    } = auth
        && !valid_cookie_name(name)
    {
        return Err(WireAdapterError::InvalidConfiguration(format!(
            "credential delivery cookie name {name:?} is not a valid cookie name"
        )));
    }
    Ok(())
}

/// Whether `name` can head a `name=value` cookie pair: a non-empty ASCII
/// token with no separators. The check is deliberately narrow - the cookie
/// name is a fixed constant of the binding, never user input - but a
/// config-declared name still fails loudly at construction.
fn valid_cookie_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_graphic()
                && !matches!(
                    byte,
                    b'(' | b')'
                        | b'<'
                        | b'>'
                        | b'@'
                        | b','
                        | b';'
                        | b':'
                        | b'\\'
                        | b'"'
                        | b'/'
                        | b'['
                        | b']'
                        | b'?'
                        | b'='
                        | b'{'
                        | b'}'
                )
        })
}

/// The transport headers every wire request sends: a JSON body, an SSE
/// accept, and the aifuel user agent. Protocol modules add their own
/// protocol headers on top.
pub(super) fn transport_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(concat!("aifuel/", env!("CARGO_PKG_VERSION"))),
    );
    headers
}

/// The configured endpoint headers, applied over the transport defaults so
/// endpoint config can adjust documented protocol headers such as
/// `anthropic-version`.
pub(super) fn apply_configured_headers(
    headers: &mut HeaderMap,
    endpoint: &EndpointConfig,
) -> Result<(), AgentRunError> {
    for (name, value) in &endpoint.extra_headers {
        let name = HeaderName::from_str(name).map_err(|_| {
            AgentRunError::InvalidRequest(format!(
                "endpoint header {name:?} is not a valid header name"
            ))
        })?;
        let value = HeaderValue::from_str(value).map_err(|_| {
            AgentRunError::InvalidRequest(format!(
                "endpoint header {name:?} is not a valid header value"
            ))
        })?;
        headers.insert(name, value);
    }
    Ok(())
}

/// The managed Authentication Binding, applied last so endpoint config
/// cannot override managed credential material on the wire.
pub(super) fn apply_auth(
    headers: &mut HeaderMap,
    auth: &ResolvedAuth,
) -> Result<(), AgentRunError> {
    match auth {
        ResolvedAuth::None => {}
        ResolvedAuth::ApiKey { key, delivery } => match delivery {
            KeyDelivery::Bearer => {
                insert_sensitive(headers, AUTHORIZATION, &format!("Bearer {key}"))?
            }
            KeyDelivery::Header { name } => {
                let name = HeaderName::from_str(name).map_err(|_| {
                    AgentRunError::InvalidRequest(format!(
                        "credential delivery header {name:?} is not a valid header name"
                    ))
                })?;
                insert_sensitive(headers, name, key)?;
            }
            KeyDelivery::Cookie { name } => insert_sensitive(
                headers,
                COOKIE,
                &aifuel_core::cookie_header_value(name, key),
            )?,
        },
        ResolvedAuth::OAuth { access_token, .. } => {
            insert_sensitive(headers, AUTHORIZATION, &format!("Bearer {access_token}"))?
        }
    }
    Ok(())
}

/// Insert credential material as a sensitive header so reqwest strips it on
/// any redirect and never prints it in header `Debug` output.
fn insert_sensitive(
    headers: &mut HeaderMap,
    name: HeaderName,
    value: &str,
) -> Result<(), AgentRunError> {
    let mut value = HeaderValue::from_str(value).map_err(|_| {
        AgentRunError::InvalidRequest("credential material is not a valid header value".to_owned())
    })?;
    value.set_sensitive(true);
    headers.insert(name, value);
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

/// The cooldown an endpoint declares through `Retry-After`, when it uses
/// the delta-seconds form common to rate-limited APIs. HTTP-date and
/// malformed values read as absent; the caller then applies its own
/// backoff step.
pub(super) fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
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
