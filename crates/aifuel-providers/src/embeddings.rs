//! Direct `POST {base_url}/embeddings` against an HTTP Provider
//! Integration, backing the inbound `/v1/embeddings` gateway surface.
//!
//! The wire adapter serves the prompt-in/text-out Agent Run contract only;
//! an embeddings call is a different endpoint on the same OpenAI-compatible
//! surface, so this module shares the wire transport policy (same-origin
//! redirects, a bounded connect, sensitive credential headers) without
//! borrowing run machinery. Auth resolution stays the caller's:
//! [`ResolvedAuth`] arrives already resolved, so credential material
//! crosses the async boundary by value exactly as the wire adapter's
//! prepared request does.

use crate::credentials::ResolvedAuth;
use crate::wire::http;
use aifuel_core::{EndpointConfig, KeyDelivery};
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, COOKIE, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Duration;

/// The send bound when the endpoint config names no request timeout. One
/// embeddings POST is a bounded request, not a long-lived stream, so the
/// wire adapter's idle-stream budget does not apply.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The shared client, built on first use - the wire adapter's lazy policy:
/// constructing it eagerly would pay TLS setup for a request that may
/// never be sent.
static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// What one `POST {base_url}/embeddings` produced.
///
/// `Answered` carries the endpoint's own status and body - the caller maps
/// them to its wire contract. `Unreachable` means no usable response ever
/// arrived, so the request may safely move to the next candidate: a send
/// that never completed cannot have been billed, and a truncated body is
/// no answer at all (embeddings are idempotent reads, not mutations).
pub enum EmbeddingsOutcome {
    /// The endpoint answered: its HTTP status and body. On a non-success
    /// status the body is credential-scrubbed before it leaves the crate.
    Answered { status: u16, body: Vec<u8> },
    /// Connect refused, DNS, TLS, timeout, or a body that never finished
    /// arriving. `reason` carries the transport's own description.
    Unreachable { reason: String },
}

/// POST `endpoint.base_url + "/embeddings"` with `body`, authenticated by
/// the already-resolved `auth`.
///
/// `Err` is a configuration failure - an unparseable base URL, a
/// non-http(s) scheme, a configured header that cannot be represented, or
/// a client that cannot be built - and never carries credential material.
/// Transport failure is `Ok(EmbeddingsOutcome::Unreachable)`, not `Err`:
/// the distinction decides whether the caller may try another integration.
pub async fn embeddings(
    endpoint: &EndpointConfig,
    auth: &ResolvedAuth,
    body: &serde_json::Value,
) -> Result<EmbeddingsOutcome, String> {
    let url = reqwest::Url::parse(&format!(
        "{}/embeddings",
        endpoint.base_url.trim_end_matches('/')
    ))
    .map_err(|error| format!("endpoint base URL is not parseable: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "endpoint base URL scheme {:?} is not http or https",
            url.scheme()
        ));
    }

    let mut headers = request_headers();
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

    let timeout = endpoint
        .request_timeout_seconds
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT);
    let response = match client()?
        .post(url)
        .headers(headers)
        .json(body)
        .timeout(timeout)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return Ok(EmbeddingsOutcome::Unreachable {
                reason: error.to_string(),
            });
        }
    };
    let status = response.status().as_u16();
    let body = match response.bytes().await {
        Ok(bytes) => bytes.to_vec(),
        Err(error) => {
            return Ok(EmbeddingsOutcome::Unreachable {
                reason: format!("the response body did not complete: {error}"),
            });
        }
    };
    Ok(EmbeddingsOutcome::Answered {
        status,
        body: if status >= 400 {
            scrub(&body, auth)
        } else {
            body
        },
    })
}

/// The transport headers an embeddings request sends. Unlike the wire
/// adapter's SSE surface, this endpoint answers one JSON document.
fn request_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(concat!("aifuel/", env!("CARGO_PKG_VERSION"))),
    );
    headers
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
    use aifuel_core::KeyDelivery;
    use std::collections::BTreeMap;

    fn endpoint(base_url: &str) -> EndpointConfig {
        EndpointConfig {
            base_url: base_url.to_owned(),
            extra_headers: BTreeMap::new(),
            request_timeout_seconds: None,
        }
    }

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

    /// A base URL that cannot produce an http(s) request fails at
    /// configuration, not after a send: the trailing-slash join and the
    /// scheme check are the same validation the wire adapter performs.
    #[tokio::test]
    async fn malformed_endpoints_fail_as_configuration_errors() {
        let auth = ResolvedAuth::None;
        for base_url in ["not a url", "ftp://endpoint.test"] {
            let outcome = embeddings(&endpoint(base_url), &auth, &serde_json::json!({})).await;
            assert!(outcome.is_err(), "{base_url} must fail configuration");
        }
        // The `/embeddings` suffix joins exactly once.
        for base_url in ["http://127.0.0.1:1/v1", "http://127.0.0.1:1/v1/"] {
            let outcome = embeddings(&endpoint(base_url), &auth, &serde_json::json!({}))
                .await
                .expect("a parseable http url configures");
            assert!(matches!(outcome, EmbeddingsOutcome::Unreachable { .. }));
        }
    }

    /// A refused connect is `Unreachable`, not `Err`: the distinction is
    /// what lets the gateway try the next ranked candidate.
    #[tokio::test]
    async fn a_refused_connect_is_unreachable_not_a_failure() {
        let outcome = embeddings(
            &endpoint("http://127.0.0.1:1"),
            &ResolvedAuth::None,
            &serde_json::json!({"model": "m", "input": "x"}),
        )
        .await
        .expect("transport failure is an outcome, not an error");
        match outcome {
            EmbeddingsOutcome::Unreachable { reason } => assert!(!reason.is_empty()),
            EmbeddingsOutcome::Answered { .. } => panic!("a dead port cannot answer"),
        }
    }
}
