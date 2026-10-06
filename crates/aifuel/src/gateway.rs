//! The inbound OpenAI-compatible gateway: `POST /v1/chat/completions` and
//! `GET /v1/models` served on the dashboard listener, so one port fronts
//! quota monitoring and prompt execution for apps configured with an
//! OpenAI `base_url`.
//!
//! An inbound `model` string resolves to a target by this convention:
//!
//! - `auto` (or `auto/<model>`) plans a route through the shared route
//!   planner: quota-headroom Providers first, then free-tier and paid
//!   API-key integrations, then the unmeasured tail - the same order
//!   `run --provider auto` produces.
//! - `<integration>/<model>` pins one integration and passes the remainder
//!   as the provider-native model id (split on the first `/`, so model ids
//!   containing slashes like `openai/gpt-5` still work).
//! - A `@<effort>` suffix on any selector pins a model-specific effort
//!   (`codex/gpt-5.6-terra@high`, `devin/swe-2@swe-2-max`); the
//!   request-level `reasoning_effort` field applies when no selector pins
//!   one. Effort values are the provider's own spellings - Codex takes its
//!   reasoning levels, Devin takes the variant uid from `devin models
//!   list`.
//! - A bare `<integration>` or unique `<provider>` runs that target with
//!   the provider's default model.
//! - Any other bare string is treated as a catalog model id and routes to
//!   the ranked candidate whose provider advertises it.
//!
//! Requests execute as read-only, prompt-only Agent Runs directly on the
//! compiled adapter set: no RunManager session, approval channel, or run
//! record. `messages[]` flattens to a single prompt transcript - the
//! execution contract carries one prompt, so tool calling and media parts
//! do not round-trip yet; see the module docs on `flatten` for the
//! convention.

mod admin;
mod audio;
mod chat;
mod completions;
mod decisions;
mod effort;
mod embeddings;
mod execute;
mod flatten;
mod forward;
mod keys;
mod logs;
mod messages;
mod models;
mod responses;
mod route_config;
mod routes;
mod types;

use aifuel_app::MonitoringFacade;
use aifuel_core::{AgentExecutionAdapter, StatusCollector, StatusReport};
use std::sync::Arc;

/// Largest accepted `/v1` request body. Chat payloads carry code context,
/// so the bound sits well above the credential mutation limit.
const MAX_BODY_BYTES: u64 = 4 * 1024 * 1024;

/// The executable surface the `/v1` endpoints serve: the owned adapter set
/// `execution_adapters` builds, shared by every request thread. Constructed
/// once at server start; a build failure degrades the gateway to an
/// answered error instead of taking the dashboard down.
pub struct Gateway {
    adapters: Vec<Arc<dyn AgentExecutionAdapter>>,
}

impl Gateway {
    /// Build the gateway's adapter set from the runtime registry. Errors
    /// surface at serve time; per-integration failures already degrade to
    /// "no adapter" inside the set.
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            adapters: crate::execution_adapters()?,
        })
    }

    /// The adapter registered for an exact Integration Identity, when the
    /// execution surface serves it.
    fn adapter(
        &self,
        integration: &aifuel_core::IntegrationId,
    ) -> Option<Arc<dyn AgentExecutionAdapter>> {
        self.adapters
            .iter()
            .find(|adapter| adapter.integration() == *integration)
            .cloned()
    }

    /// Resolve a selector - an Integration Identity or a bare Provider Id -
    /// against the executable adapter set, mirroring `AgentRunFacade`.
    fn resolve(
        &self,
        selector: &str,
    ) -> Result<aifuel_core::IntegrationId, aifuel_core::AgentRunError> {
        aifuel_core::resolve_integration(
            &aifuel_core::IntegrationId::new(selector),
            self.adapters
                .iter()
                .map(|adapter| (adapter.integration(), adapter.provider())),
        )
    }

    /// The executable adapter set, for `GET /v1/models` listing.
    fn adapters(&self) -> &[Arc<dyn AgentExecutionAdapter>] {
        &self.adapters
    }

    /// One collected status snapshot for route planning. The monitoring
    /// facade's own cache bounds how fresh this is; the caller decides when
    /// a fresh collection is worth the latency.
    fn status<C: StatusCollector>(
        facade: &MonitoringFacade<C>,
        runtime: &tokio::runtime::Runtime,
    ) -> StatusReport {
        runtime.block_on(facade.status(false))
    }
}

/// The providers.json optimizer plan for one inbound request: `rtk`
/// compresses tool-role content at flatten, and the plan rides the
/// RunRequest so wire integrations receive the caveman instruction. A
/// registry that cannot load reads as the inert plan - the gateway keeps
/// answering rather than failing on config it did not write.
pub(crate) fn request_plan() -> aifuel_core::OptimizePlan {
    crate::integration_registry()
        .map(|registry| registry.optimizer().clone())
        .unwrap_or_default()
}

/// Dispatch one `/v1` request. `None` gateway answers every route with an
/// OpenAI-shaped 503 - the dashboard stays up when the execution surface
/// cannot build.
pub fn handle<C: StatusCollector>(
    request: tiny_http::Request,
    gateway: Option<&Gateway>,
    facade: &MonitoringFacade<C>,
    runtime: &tokio::runtime::Runtime,
) {
    let path = request.url().split('?').next().unwrap_or(request.url());
    if request.method() == &tiny_http::Method::Options {
        // Preflight answers the CORS contract; the loopback guard upstream
        // still decides whether the actual request may follow.
        respond(request, 204, Vec::new(), None, cors_headers());
        return;
    }
    let Some(gateway) = gateway else {
        respond_error(
            request,
            503,
            "the execution surface is unavailable; check `aifuel run` works on this host",
            "server_error",
        );
        return;
    };
    if let Err(reason) = keys::authorize(&request) {
        respond_error(request, 401, &reason, "authentication_error");
        return;
    }
    match (request.method(), path) {
        (&tiny_http::Method::Get, "/v1/models") => {
            respond(
                request,
                200,
                serde_json::to_vec(&models::list(gateway)).expect("the models list serializes"),
                Some("application/json"),
                cors_headers(),
            );
        }
        (&tiny_http::Method::Post, "/v1/chat/completions") => {
            chat::completions(request, gateway, facade, runtime);
        }
        (&tiny_http::Method::Post, "/v1/completions") => {
            completions::handle(request, gateway, facade, runtime);
        }
        (&tiny_http::Method::Post, "/v1/embeddings") => {
            embeddings::handle(request, gateway, facade, runtime);
        }
        (&tiny_http::Method::Post, "/v1/decisions") => {
            decisions::handle(request, gateway, facade, runtime);
        }
        (&tiny_http::Method::Post, "/v1/audio/speech") => {
            audio::speech(request, gateway, facade, runtime);
        }
        (&tiny_http::Method::Post, "/v1/audio/transcriptions") => {
            audio::transcriptions(request, gateway, facade, runtime);
        }
        (&tiny_http::Method::Post, "/v1/messages")
        | (&tiny_http::Method::Post, "/v1/messages/count_tokens") => {
            messages::handle(request, gateway, facade, runtime);
        }
        (&tiny_http::Method::Post, "/v1/responses")
        | (&tiny_http::Method::Post, "/v1/responses/compact") => {
            responses::handle(request, gateway, facade, runtime);
        }
        _ => respond_error(
            request,
            404,
            "unknown /v1 endpoint",
            "invalid_request_error",
        ),
    }
}

/// Whether the key store holds at least one non-revoked downstream key.
/// Remote binds use it to fail `/v1` closed while none exists - without
/// a usable key every request rejects anyway, so the anonymous posture
/// must never reach a public socket.
pub fn has_active_keys() -> bool {
    keys::any_active()
}

/// Dispatch one `/api/gateway/*` request - the admin surface runs under
/// the dashboard's strict same-origin guard. A `None` gateway answers 503
/// like the `/v1` routes.
pub fn handle_admin(request: tiny_http::Request, gateway: Option<&Gateway>) {
    match gateway {
        Some(gateway) => admin::handle(request, gateway),
        None => respond_error(
            request,
            503,
            "the execution surface is unavailable; check `aifuel run` works on this host",
            "server_error",
        ),
    }
}

/// The OpenAI error envelope every `/v1` failure shares, so SDK clients
/// parse it instead of a bare status line.
pub fn respond_error(request: tiny_http::Request, status: u16, message: &str, kind: &str) {
    let body = serde_json::json!({
        "error": {"message": message, "type": kind, "param": null, "code": null}
    });
    respond(
        request,
        status,
        serde_json::to_vec(&body).expect("an error envelope serializes"),
        Some("application/json"),
        cors_headers(),
    );
}

/// CORS headers for preflight answers and `/v1` responses. The gateway's
/// effective cross-origin policy is the loopback guard in the dashboard -
/// these headers only let a permitted caller's browser read the response.
fn cors_headers() -> Vec<(String, String)> {
    vec![
        ("Access-Control-Allow-Origin".to_owned(), "*".to_owned()),
        (
            "Access-Control-Allow-Methods".to_owned(),
            "GET, POST, OPTIONS".to_owned(),
        ),
        (
            "Access-Control-Allow-Headers".to_owned(),
            "Authorization, Content-Type".to_owned(),
        ),
    ]
}

/// One bounded JSON response writer shared by the `/v1` routes.
fn respond(
    request: tiny_http::Request,
    status: u16,
    body: Vec<u8>,
    content_type: Option<&str>,
    headers: Vec<(String, String)>,
) {
    let mut response =
        tiny_http::Response::from_data(body).with_status_code(tiny_http::StatusCode(status));
    if let Some(content_type) = content_type {
        response.add_header(
            tiny_http::Header::from_bytes("Content-Type", content_type)
                .expect("static content type is valid"),
        );
    }
    for (name, value) in headers {
        if let Ok(header) = tiny_http::Header::from_bytes(name, value) {
            response.add_header(header);
        }
    }
    let _ = request.respond(response);
}

/// Read one JSON request body bounded by [`MAX_BODY_BYTES`].
fn read_body(request: &mut tiny_http::Request) -> Result<Vec<u8>, &'static str> {
    use std::io::Read;
    let mut body = Vec::new();
    request
        .as_reader()
        .take(MAX_BODY_BYTES + 1)
        .read_to_end(&mut body)
        .map_err(|_| "could not read the request body")?;
    if body.len() as u64 > MAX_BODY_BYTES {
        return Err("request body exceeds the 4 MiB limit");
    }
    Ok(body)
}
