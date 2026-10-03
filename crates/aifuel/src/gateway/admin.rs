//! Dashboard admin endpoints for the gateway: `GET /api/gateway/keys`,
//! `POST /api/gateway/keys`, `POST /api/gateway/keys/revoke`,
//! `GET /api/gateway/logs`, `GET /api/gateway/providers`.
//!
//! The surface mirrors the dashboard's own JSON conventions - pretty
//! bodies, `{"error": "..."}` rejections, 8 KiB mutation bodies - rather
//! than the `/v1` OpenAI error envelope, because it answers dashboard
//! fetches, not OpenAI SDK clients.

use super::{Gateway, cors_headers, respond};
use aifuel_core::{DiscoveryState, ExecutionConfig, WireApi};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::Read;
use tiny_http::{Method, Request};

/// Mutation bodies are small by contract - a key name or an id - so the
/// dashboard's own mutation bound applies here too.
const MAX_BODY_BYTES: u64 = 8 * 1024;

/// Handle one `/api/gateway/*` request. Runs under the dashboard's strict
/// same-origin guard, not the relaxed `/v1` guard.
pub(crate) fn handle(request: Request, gateway: &Gateway) {
    let path = request.url().split('?').next().unwrap_or(request.url());
    match (request.method(), path) {
        (&Method::Get, "/api/gateway/keys") => match super::keys::list() {
            Ok(keys) => respond_json(request, 200, &json!({"keys": keys})),
            Err(error) => respond_admin_error(request, 500, &error),
        },
        (&Method::Post, "/api/gateway/keys") => create_key(request),
        (&Method::Post, "/api/gateway/keys/revoke") => revoke_key(request),
        (&Method::Post, "/api/gateway/keys/update") => update_key(request),
        (&Method::Get, "/api/gateway/routes") => super::route_config::get(request),
        (&Method::Put, "/api/gateway/routes") | (&Method::Post, "/api/gateway/routes") => {
            super::route_config::put(request)
        }
        (&Method::Get, "/api/gateway/logs") => list_logs(request),
        (&Method::Get, "/api/gateway/providers") => {
            respond_json(request, 200, &json!({"providers": providers(gateway)}))
        }
        (&Method::Get, "/api/gateway/models") => {
            respond_json(request, 200, &super::models::list(gateway))
        }
        _ => respond_admin_error(request, 404, "unknown gateway admin endpoint"),
    }
}

#[derive(serde::Deserialize)]
struct CreateKeyBody {
    name: String,
    #[serde(default)]
    models: Option<Vec<String>>,
}

/// `POST /api/gateway/keys`: issue a downstream key. The raw key answers
/// once in `key` - the store keeps only its digest, so it can never be
/// read back later.
fn create_key(mut request: Request) {
    let body: CreateKeyBody = match read_json_body(&mut request) {
        Ok(body) => body,
        Err(error) => return respond_admin_error(request, 400, &error),
    };
    match super::keys::create(&body.name, body.models) {
        Ok((summary, raw)) => respond_json(request, 200, &json!({"id": summary.id, "key": raw})),
        Err(error) => respond_admin_error(request, 400, &error),
    }
}

#[derive(serde::Deserialize)]
struct RevokeKeyBody {
    id: String,
}

/// `POST /api/gateway/keys/revoke`: reject a key from now on.
fn revoke_key(mut request: Request) {
    let body: RevokeKeyBody = match read_json_body(&mut request) {
        Ok(body) => body,
        Err(error) => return respond_admin_error(request, 400, &error),
    };
    match super::keys::revoke(&body.id) {
        Ok(()) => respond_json(request, 200, &json!({"ok": true})),
        Err(error) => respond_admin_error(request, 400, &error),
    }
}

#[derive(serde::Deserialize)]
struct UpdateKeyBody {
    id: String,
    #[serde(default)]
    models: Option<Vec<String>>,
}

/// `POST /api/gateway/keys/update`: replace a key's model allowlist.
fn update_key(mut request: Request) {
    let body: UpdateKeyBody = match read_json_body(&mut request) {
        Ok(body) => body,
        Err(error) => return respond_admin_error(request, 400, &error),
    };
    match super::keys::update(&body.id, body.models) {
        Ok(()) => respond_json(request, 200, &json!({"ok": true})),
        Err(error) => respond_admin_error(request, 400, &error),
    }
}

/// `GET /api/gateway/logs?limit=N`: the newest-first request log, capped
/// at the log's own bound.
fn list_logs(request: Request) {
    let limit = match log_limit(request.url()) {
        Ok(limit) => limit,
        Err(error) => return respond_admin_error(request, 400, &error),
    };
    respond_json(
        request,
        200,
        &json!({"entries": super::logs::entries(limit)}),
    );
}

/// `?limit=N` → the clamped entry count: absent is the whole buffer, a
/// value past the cap clamps down, and garbage is a client error rather
/// than a silent default.
fn log_limit(url: &str) -> Result<usize, String> {
    let Some(query) = url.split('?').nth(1) else {
        return Ok(super::logs::MAX_ENTRIES);
    };
    for part in query.split('&') {
        if let Some(value) = part.strip_prefix("limit=") {
            return value
                .parse::<usize>()
                .map(|limit| limit.min(super::logs::MAX_ENTRIES))
                .map_err(|_| format!("invalid limit {value:?}"));
        }
    }
    Ok(super::logs::MAX_ENTRIES)
}

/// Provider rows for the dashboard, per registered adapter with registry
/// enrichment when it resolves: `kind` classifies the integration id
/// suffix (`:oauth`, `:api-key`, `:local`, `:web`, else `cli`), `ready`
/// reports whether the integration's discovery evidence is present
/// locally, and `embeddings` marks the `Http` + OpenAI-chat descriptors
/// the embeddings surface can serve. Enrichment degrades to `null`s on
/// registry failure rather than failing the listing.
fn providers(gateway: &Gateway) -> Vec<Value> {
    let registry = crate::integration_registry().ok();
    let discovery = aifuel_providers::DiscoveryContext::from_environment().ok();
    let credentials = crate::aifuel_config_dir()
        .ok()
        .map(aifuel_providers::CredentialStore::new);
    let evidence = match (&registry, &discovery, &credentials) {
        (Some(registry), Some(discovery), Some(credentials)) => {
            Some(registry.evidence_context(discovery, credentials))
        }
        _ => None,
    };
    let mut seen = BTreeSet::new();
    gateway
        .adapters()
        .iter()
        .filter(|adapter| seen.insert(adapter.integration().as_str().to_owned()))
        .map(|adapter| {
            let integration = adapter.integration();
            let descriptor = registry.as_ref().and_then(|registry| {
                registry
                    .list()
                    .find(|descriptor| *descriptor.id() == integration)
            });
            let embeddings = descriptor.is_some_and(|descriptor| {
                matches!(
                    &descriptor.integration.execution,
                    ExecutionConfig::Http { protocol, .. }
                        if *protocol == WireApi::OpenAiChat
                )
            });
            let ready = descriptor
                .zip(evidence.as_ref())
                .and_then(|(descriptor, context)| descriptor.discover(context).ok())
                .map(|state| state == DiscoveryState::Present);
            json!({
                "integration": integration.as_str(),
                "provider": adapter.provider().as_str(),
                "kind": integration_kind(integration.as_str()),
                "ready": ready,
                "embeddings": embeddings,
            })
        })
        .collect()
}

/// The integration-family classifier the dashboard groups cards by: the
/// distinguishing id suffix, falling back to `cli` for bare ids and any
/// suffix it does not recognize.
fn integration_kind(integration: &str) -> &'static str {
    match integration.rsplit_once(':').map(|(_, suffix)| suffix) {
        Some("oauth") => "oauth",
        Some("api-key") => "api-key",
        Some("local") => "local",
        Some("web") => "web",
        _ => "cli",
    }
}

/// One JSON answer in the dashboard's envelope; `/v1` CORS headers ride
/// along so a permitted loopback caller's browser can read the response.
fn respond_json(request: Request, status: u16, value: &impl Serialize) {
    match serde_json::to_vec_pretty(value) {
        Ok(body) => respond(
            request,
            status,
            body,
            Some("application/json; charset=utf-8"),
            cors_headers(),
        ),
        Err(_) => respond(
            request,
            500,
            b"{\"error\": \"could not serialize the response\"}".to_vec(),
            Some("application/json; charset=utf-8"),
            cors_headers(),
        ),
    }
}

/// `{"error": "..."}` - the dashboard's error shape, not the OpenAI
/// envelope `/v1` uses; this is a dashboard surface.
fn respond_admin_error(request: Request, status: u16, message: &str) {
    respond_json(request, status, &json!({"error": message}));
}

/// Decode a JSON mutation body bounded by `MAX_BODY_BYTES`. Requiring
/// `application/json` is part of the CSRF posture the dashboard relies
/// on: a cross-site form can only post form-encodings, so forged
/// submissions fail here even before the origin guard runs.
fn read_json_body<T: serde::de::DeserializeOwned>(request: &mut Request) -> Result<T, String> {
    let content_type = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Content-Type"))
        .map(|header| header.value.as_str().to_string())
        .unwrap_or_default();
    if !content_type.starts_with("application/json") {
        // Drain the rejected body: closing a socket while the kernel still
        // holds unread request bytes can RST the connection before the
        // client reads the rejection.
        let mut reader = request.as_reader().take(MAX_BODY_BYTES + 1);
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
        return Err("expected an application/json request body".to_owned());
    }
    let mut body = String::new();
    request
        .as_reader()
        .take(MAX_BODY_BYTES + 1)
        .read_to_string(&mut body)
        .map_err(|error| format!("could not read the request body: {error}"))?;
    if body.len() as u64 > MAX_BODY_BYTES {
        return Err("the request body is too large".to_owned());
    }
    serde_json::from_str(&body).map_err(|error| format!("invalid JSON body: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aifuel_core::{
        AgentCapability, AgentCapabilityEvidence, AgentExecutionAdapter, AgentRunError,
        CapabilityState, IntegrationId, ProviderId, RunCancellationToken, RunRequest, RunResult,
    };
    use std::collections::BTreeMap;
    use std::sync::Arc;

    /// A listing-only adapter: the admin surface never executes, so the
    /// run contract stays `unreachable` while capabilities are declared.
    struct TestAdapter {
        id: &'static str,
        streaming: CapabilityState,
        read_only: CapabilityState,
    }

    impl AgentExecutionAdapter for TestAdapter {
        fn integration(&self) -> IntegrationId {
            IntegrationId::new(self.id)
        }

        fn provider(&self) -> ProviderId {
            ProviderId::new("test")
        }

        fn declared_agent_capabilities(
            &self,
        ) -> BTreeMap<AgentCapability, AgentCapabilityEvidence> {
            AgentCapability::ALL
                .into_iter()
                .map(|capability| {
                    let state = match capability {
                        AgentCapability::Streaming => self.streaming,
                        AgentCapability::ReadOnly => self.read_only,
                        _ => CapabilityState::Unknown,
                    };
                    (
                        capability,
                        AgentCapabilityEvidence {
                            state,
                            reason: "test adapter".to_owned(),
                        },
                    )
                })
                .collect()
        }

        fn execute(
            &self,
            _request: &RunRequest,
            _cancellation: &RunCancellationToken,
        ) -> Result<RunResult, AgentRunError> {
            unreachable!("the admin listing never executes an adapter")
        }
    }

    #[test]
    fn providers_classify_kind_and_degrade_enrichment_honestly() {
        // Unknown test ids are not registry descriptors, so enrichment
        // answers null instead of guessing at readiness or embeddings.
        let gateway = Gateway {
            adapters: vec![
                Arc::new(TestAdapter {
                    id: "a:cli",
                    streaming: CapabilityState::Supported,
                    read_only: CapabilityState::Supported,
                }),
                Arc::new(TestAdapter {
                    id: "b:oauth",
                    streaming: CapabilityState::Unknown,
                    read_only: CapabilityState::Unsupported,
                }),
            ],
        };
        let rows = providers(&gateway);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            json!({
                "integration": "a:cli",
                "provider": "test",
                "kind": "cli",
                "ready": null,
                "embeddings": false,
            })
        );
        assert_eq!(
            rows[1],
            json!({
                "integration": "b:oauth",
                "provider": "test",
                "kind": "oauth",
                "ready": null,
                "embeddings": false,
            }),
            "the id suffix classifies the family independent of capability evidence"
        );
    }

    #[test]
    fn providers_dedup_repeated_integrations() {
        // `adapter()` resolves the first match for a repeated integration
        // id, so the listing mirrors it rather than showing two rows that
        // would route identically.
        let adapter = || TestAdapter {
            id: "same:cli",
            streaming: CapabilityState::Supported,
            read_only: CapabilityState::Supported,
        };
        let gateway = Gateway {
            adapters: vec![Arc::new(adapter()), Arc::new(adapter())],
        };
        assert_eq!(providers(&gateway).len(), 1);
    }

    #[test]
    fn log_limit_defaults_to_the_full_buffer_and_caps_at_its_bound() {
        assert_eq!(
            log_limit("/api/gateway/logs").expect("no query"),
            crate::gateway::logs::MAX_ENTRIES
        );
        assert_eq!(
            log_limit("/api/gateway/logs?limit=10").expect("small limit"),
            10
        );
        assert_eq!(
            log_limit("/api/gateway/logs?limit=9999").expect("huge limit"),
            crate::gateway::logs::MAX_ENTRIES,
            "a limit past the log's bound clamps down"
        );
        assert!(
            log_limit("/api/gateway/logs?limit=soon").is_err(),
            "a malformed limit is a client error, not a silent default"
        );
        assert_eq!(
            log_limit("/api/gateway/logs?other=1").expect("unrelated query"),
            crate::gateway::logs::MAX_ENTRIES
        );
    }
}
