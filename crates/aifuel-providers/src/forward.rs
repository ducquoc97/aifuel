//! Bounded `POST {base_url}/{path}` forwarding against an HTTP Provider
//! Integration, backing the gateway's non-chat `/v1` surfaces: decisions,
//! audio, and any OpenAI-compatible endpoint the Agent Run adapter's
//! prompt-in/text-out contract cannot express.
//!
//! The module shares the wire transport policy (same-origin redirects, a
//! bounded connect, sensitive credential headers) without borrowing run
//! machinery. Auth resolution stays the caller's: [`ResolvedAuth`] arrives
//! already resolved, so credential material crosses the async boundary by
//! value exactly as the wire adapter's prepared request does.

use crate::credentials::ResolvedAuth;
use crate::wire::http;
use aifuel_core::{EndpointConfig, KeyDelivery};
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, COOKIE, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};
use serde_json::Value;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Duration;

/// The send bound when the endpoint config names no request timeout. A
/// forwarded POST is a bounded request, not a long-lived stream, so the
/// wire adapter's idle-stream budget does not apply.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The shared client, built on first use - the wire adapter's lazy policy:
/// constructing it eagerly would pay TLS setup for a request that may
/// never be sent.
static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// What one `POST {base_url}/{path}` produced.
///
/// `Answered` carries the endpoint's own status, response content type,
/// and body - the caller maps them to its wire contract. `Unreachable`
/// means no usable response ever arrived, so the request may safely move
/// to the next candidate: a send that never completed cannot have been
/// billed, and a truncated body is no answer at all.
pub enum ForwardOutcome {
    /// The endpoint answered: its HTTP status, its own `Content-Type`
    /// when it sent one, and its body. On a non-success status the body
    /// is credential-scrubbed before it leaves the crate.
    Answered {
        status: u16,
        content_type: Option<String>,
        body: Vec<u8>,
    },
    /// Connect refused, DNS, TLS, timeout, or a body that never finished
    /// arriving. `reason` carries the transport's own description.
    Unreachable { reason: String },
}

/// POST `endpoint.base_url + "/" + path` with `body` as JSON,
/// authenticated by the already-resolved `auth`.
///
/// `Err` is a configuration failure - an unparseable base URL, a
/// non-http(s) scheme, a configured header that cannot be represented, or
/// a client that cannot be built - and never carries credential material.
/// Transport failure is `Ok(ForwardOutcome::Unreachable)`, not `Err`: the
/// distinction decides whether the caller may try another integration.
pub async fn post_json(
    endpoint: &EndpointConfig,
    auth: &ResolvedAuth,
    path: &str,
    body: &Value,
) -> Result<ForwardOutcome, String> {
    let bytes = serde_json::to_vec(body)
        .map_err(|error| format!("the request body could not be serialized: {error}"))?;
    post_raw(endpoint, auth, path, bytes, "application/json", "application/json").await
}

/// POST `endpoint.base_url + "/" + path` with a raw body and explicit
/// `content_type`/`accept` headers, authenticated by `auth`. Multipart
/// form posts (audio transcriptions) and binary answers (speech) travel
/// through here: the gateway does not re-encode a form it can already
/// carry, and the caller states what response type it accepts.
pub async fn post_raw(
    endpoint: &EndpointConfig,
    auth: &ResolvedAuth,
    path: &str,
    body: Vec<u8>,
    content_type: &str,
    accept: &str,
) -> Result<ForwardOutcome, String> {
    let url = url(endpoint, path)?;
    let headers = request_headers(endpoint, auth, content_type, accept)?;
    let response = client()?
        .post(url)
        .headers(headers)
        .body(body)
        .timeout(timeout(endpoint))
        .send()
        .await;
    finish(response, auth).await
}

/// The destination URL: `base_url` joined with `path`, http(s) only -
/// the same refusal the wire adapter applies so a configured `file:` or
/// `gopher:` base can never smuggle credentials out of the client.
fn url(endpoint: &EndpointConfig, path: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(&format!(
        "{}/{}",
        endpoint.base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    ))
    .map_err(|error| format!("endpoint base URL is not parseable: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "endpoint base URL scheme {:?} is not http or https",
            url.scheme()
        ));
    }
    Ok(url)
}

/// The transport headers a forwarded request sends: the request's own
/// `Content-Type` when supplied, a JSON `Accept` the endpoints in this
/// family answer with, then configured endpoint headers, then managed
/// credential material last.
fn request_headers(
    endpoint: &EndpointConfig,
    auth: &ResolvedAuth,
    content_type: &str,
    accept: &str,
) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    let content_type = HeaderValue::from_str(content_type)
        .map_err(|_| format!("request content type {content_type:?} is not a valid header value"))?;
    headers.insert(CONTENT_TYPE, content_type);
    let accept = HeaderValue::from_str(accept)
        .map_err(|_| format!("request accept {accept:?} is not a valid header value"))?;
    headers.insert(ACCEPT, accept);
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(concat!("aifuel/", env!("CARGO_PKG_VERSION"))),
    );
    // Configured endpoint headers apply under managed auth, matching the
    // wire adapter's rule that endpoint config may adjust protocol headers
    // but never credential material.
    for (name, value) in &endpoint.extra_headers {
        let name = HeaderName::from_str(name)
            .map_err(|_| format!("endpoint header {name:?} is not a valid header name"))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| format!("endpoint header {name:?} is not a valid header value"))?;
        headers.insert(name, value);
    }
    apply_auth(&mut headers, auth)?;
    Ok(headers)
}

/// Shape one sent request into its outcome: a completed response is an
/// `Answered` carrying status, `Content-Type`, and the (error-scrubbed)
/// body; anything the transport could not finish is `Unreachable`.
async fn finish(
    response: Result<reqwest::Response, reqwest::Error>,
    auth: &ResolvedAuth,
) -> Result<ForwardOutcome, String> {
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            return Ok(ForwardOutcome::Unreachable {
                reason: error.to_string(),
            });
        }
    };
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = match response.bytes().await {
        Ok(bytes) => bytes.to_vec(),
        Err(error) => {
            return Ok(ForwardOutcome::Unreachable {
                reason: format!("the response body did not complete: {error}"),
            });
        }
    };
    Ok(ForwardOutcome::Answered {
        status,
        content_type,
        body: if status >= 400 {
            scrub(&body, auth)
        } else {
            body
        },
    })
}

/// The resolved Authentication Binding, applied last so endpoint config
/// cannot override managed credential material - the wire adapter's
/// `apply_auth` rule restated for this module's own header set (the wire
/// helper is module-private to `wire`).
fn apply_auth(headers: &mut HeaderMap, auth: &ResolvedAuth) -> Result<(), String> {
    match auth {
        ResolvedAuth::None => {}
        ResolvedAuth::ApiKey { key, delivery } => match delivery {
            KeyDelivery::Bearer => insert(headers, AUTHORIZATION, &format!("Bearer {key}"))?,
            KeyDelivery::Header { name } => {
                let name = HeaderName::from_str(name).map_err(|_| {
                    format!("credential delivery header {name:?} is not a valid header name")
                })?;
                insert(headers, name, key)?;
            }
            KeyDelivery::Cookie { name } => insert(
                headers,
                COOKIE,
                &aifuel_core::cookie_header_value(name, key),
            )?,
        },
        ResolvedAuth::OAuth { access_token, .. } => {
            insert(headers, AUTHORIZATION, &format!("Bearer {access_token}"))?
        }
    }
    Ok(())
}

/// Insert credential material through the wire module's sensitive-header
/// helper so reqwest strips it on redirects and never prints it.
fn insert(headers: &mut HeaderMap, name: HeaderName, value: &str) -> Result<(), String> {
    http::insert_sensitive(headers, name, value).map_err(|error| error.to_string())
}

/// Scrub resolved credential material out of an upstream error body before
/// it leaves the crate - the wire adapter's `redact` contract: a hostile
/// endpoint can echo `Authorization` back in its own error body. Only
/// non-success responses run through here, and only decodable UTF-8 is
/// rewritten; an opaque binary error body has no header echo to strip.
fn scrub(body: &[u8], auth: &ResolvedAuth) -> Vec<u8> {
    let secret = match auth {
        ResolvedAuth::ApiKey { key, .. } => Some(key.as_str()),
        ResolvedAuth::OAuth { access_token, .. } => Some(access_token.as_str()),
        ResolvedAuth::None => None,
    };
    let Some(secret) = secret.filter(|secret| !secret.is_empty()) else {
        return body.to_vec();
    };
    match std::str::from_utf8(body) {
        Ok(text) => text.replace(secret, "<redacted>").into_bytes(),
        Err(_) => body.to_vec(),
    }
}

fn timeout(endpoint: &EndpointConfig) -> Duration {
    endpoint
        .request_timeout_seconds
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT)
}

fn client() -> Result<&'static reqwest::Client, String> {
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    let client = http::build_client()
        .map_err(|error| format!("could not create the HTTP client: {error}"))?;
    Ok(CLIENT.get_or_init(|| client))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::ResolvedAuth;
    use aifuel_core::KeyDelivery;

    /// Credential material must arrive on the wire exactly the way the
    /// binding's delivery declares - Bearer, a named header, or a cookie
    /// pair - and must stay marked sensitive so reqwest strips it on
    /// redirects.
    #[test]
    fn apply_auth_delivers_each_binding_the_declared_way() {
        let mut headers = HeaderMap::new();
        apply_auth(
            &mut headers,
            &ResolvedAuth::ApiKey {
                key: "sk-test".to_owned(),
                delivery: KeyDelivery::Bearer,
            },
        )
        .expect("bearer applies");
        assert_eq!(headers[AUTHORIZATION], "Bearer sk-test");
        assert!(headers[AUTHORIZATION].is_sensitive());

        let mut headers = HeaderMap::new();
        apply_auth(
            &mut headers,
            &ResolvedAuth::ApiKey {
                key: "sk-test".to_owned(),
                delivery: KeyDelivery::Header {
                    name: "x-api-key".to_owned(),
                },
            },
        )
        .expect("named header applies");
        assert_eq!(headers["x-api-key"], "sk-test");
        assert!(headers.get(AUTHORIZATION).is_none());

        let mut headers = HeaderMap::new();
        apply_auth(
            &mut headers,
            &ResolvedAuth::ApiKey {
                key: "session-material".to_owned(),
                delivery: KeyDelivery::Cookie {
                    name: "sessionKey".to_owned(),
                },
            },
        )
        .expect("cookie applies");
        assert_eq!(headers[COOKIE], "sessionKey=session-material");

        let mut headers = HeaderMap::new();
        apply_auth(
            &mut headers,
            &ResolvedAuth::OAuth {
                access_token: "tok".to_owned(),
                needs_refresh: false,
            },
        )
        .expect("oauth applies");
        assert_eq!(headers[AUTHORIZATION], "Bearer tok");

        let mut headers = HeaderMap::new();
        apply_auth(&mut headers, &ResolvedAuth::None).expect("none applies");
        assert!(headers.is_empty());
    }

    /// An upstream error body that echoes the sent credential back must
    /// never carry the material to the gateway client - the same trust
    /// boundary the wire adapter's `redact` enforces on diagnostics.
    #[test]
    fn scrub_strips_credential_echoes_from_error_bodies() {
        let auth = ResolvedAuth::ApiKey {
            key: "sk-secret".to_owned(),
            delivery: KeyDelivery::Bearer,
        };
        let body = br#"{"error": {"message": "bad key sk-secret"}}"#;
        let scrubbed = scrub(body, &auth);
        let text = String::from_utf8(scrubbed).expect("scrub keeps utf8");
        assert!(!text.contains("sk-secret"));
        assert!(text.contains("<redacted>"));

        // Nothing to scrub: unauthenticated bindings and opaque bodies
        // pass through byte-identical.
        assert_eq!(scrub(body, &ResolvedAuth::None), body);
        let binary = [0xff, 0x00, b's', b'k'];
        assert_eq!(scrub(&binary, &auth), binary);
    }
}
