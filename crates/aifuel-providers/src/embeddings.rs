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
use crate::forward::{self, ForwardOutcome};
use aifuel_core::EndpointConfig;

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
/// the already-resolved `auth`. The transport lives in `crate::forward`;
/// this entry point keeps the embeddings-shaped outcome the gateway's
/// `/v1/embeddings` handler maps.
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
    match forward::post_json(endpoint, auth, "embeddings", body).await? {
        ForwardOutcome::Answered { status, body, .. } => {
            Ok(EmbeddingsOutcome::Answered { status, body })
        }
        ForwardOutcome::Unreachable { reason } => Ok(EmbeddingsOutcome::Unreachable { reason }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn endpoint(base_url: &str) -> EndpointConfig {
        EndpointConfig {
            base_url: base_url.to_owned(),
            extra_headers: BTreeMap::new(),
            request_timeout_seconds: None,
        }
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
