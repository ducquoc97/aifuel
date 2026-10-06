//! The shared resolution-and-relay half of the gateway's forwarding
//! surfaces: `/v1/embeddings`, `/v1/decisions`, and `/v1/audio/*` all
//! resolve the inbound `model` selector to a ranked chain of HTTP
//! endpoints, then try each candidate in order. What a surface means -
//! its upstream path, which integrations are eligible, the provider's
//! default model - is declared once in a [`Surface`] table so the
//! endpoint modules keep only request parsing and the send itself.
//!
//! Addressing mirrors the `crate::gateway` convention, narrowed to the
//! surface's eligible set:
//!
//! - `auto` and `auto/<model>` rank candidates by evidence, never by a
//!   live probe: keyed API-key integrations with a present credential
//!   (documented free tiers first, the route planner's partition), then
//!   OAuth-bound HTTP endpoints whose grant is stored, then local
//!   `AuthBinding::None` endpoints with present discovery evidence.
//! - `<integration>/<model>` pins one target; `<model>` may carry its own
//!   slashes because the split is on the first `/`.
//! - A bare `<integration>` sends the provider's documented default model
//!   when the surface declares one, else omits `model` and lets the
//!   endpoint's own contract answer rather than guessing.
//! - Any other bare string is treated as a model id and routed to the
//!   ranked chain verbatim, matching how the chat surface treats catalog
//!   model ids. Named aliases and combos from `gateway.json` resolve the
//!   same way the other `/v1` endpoints resolve them.
//!
//! The candidate chain advances only when no usable response arrived: an
//! endpoint that answered owns its response, because the request may
//! already have been billed. A 2xx answer passes through verbatim with
//! the upstream's own `Content-Type`; anything else folds into the shared
//! OpenAI error envelope carrying the upstream's own message when it sent
//! one.

use super::{cors_headers, logs, respond, respond_error};
use aifuel_core::{
    ApiKeySource, AuthBinding, EndpointConfig, ExecutionConfig, IntegrationId, KeyDelivery,
};
use aifuel_providers::{CredentialStore, EvidenceContext, IntegrationDescriptor, IntegrationRegistry};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Deepest configured-name expansion `resolve_chain` follows: an alias may
/// point at a combo or another alias, so cycles resolve as a loud 400 -
/// the same bound `execute` applies.
const MAX_ROUTE_DEPTH: usize = 8;

/// What makes one forwarding surface distinct from another: the upstream
/// path a candidate POSTs to, and the provider's documented default model
/// for bare selectors. Eligibility is folded into `path` - a descriptor
/// the surface cannot serve simply maps to `None`.
pub(crate) struct Surface {
    /// The surface's name in failover notes and refusal messages.
    pub name: &'static str,
    /// The path suffix joined onto `endpoint.base_url`, or `None` when
    /// this descriptor cannot serve the surface at all.
    pub path: fn(&IntegrationDescriptor) -> Option<&'static str>,
    /// The model a bare `<integration>` selector sends when the provider
    /// documents one; `None` leaves `model` to the endpoint's contract.
    pub default_model: fn(&str) -> Option<&'static str>,
}

/// One forwarding target: the endpoint and (instance-rebound) auth to
/// send, the identity credential resolution runs under, the instance's
/// resolved environment overlay, the model override the selector pinned
/// when any, and the upstream path this surface resolved for it.
pub(crate) struct Candidate {
    /// The selector id reported in logs and failover notes: the instance
    /// id for an instance target, else the integration id.
    pub serving_id: IntegrationId,
    /// The catalog provider id the target belongs to - surfaces use it
    /// for provider-level defaults when the selector pinned no model.
    pub provider: aifuel_core::ProviderId,
    pub endpoint: EndpointConfig,
    pub auth: AuthBinding,
    pub auth_identity: IntegrationId,
    pub env: BTreeMap<String, String>,
    pub model: Option<String>,
    pub path: &'static str,
}

/// How a pinned selector supplies `model` upstream.
pub(crate) enum ModelPin {
    /// `<integration>/<model>`: send exactly this model id.
    Pinned(String),
    /// Bare `<integration>`: the provider default when documented, else
    /// omit `model` for the endpoint's own contract to decide.
    Default,
}

/// Resolve the inbound `model` string to the ranked candidate chain - see
/// the module docs for the addressing convention. Configured aliases and
/// combos expand first, exactly as `execute::resolve_attempts` does.
pub(crate) fn resolve_chain(
    surface: &Surface,
    selector: &str,
    registry: &IntegrationRegistry,
    credentials: &CredentialStore,
    evidence: Option<&EvidenceContext<'_>>,
    depth: usize,
) -> Result<Vec<Candidate>, (u16, String)> {
    if let Some(resolution) = super::routes::resolve(selector).map_err(|error| (500, error))? {
        if depth >= MAX_ROUTE_DEPTH {
            return Err((
                400,
                format!(
                    "route {selector:?} resolves too deeply; check gateway.json for alias cycles"
                ),
            ));
        }
        return match resolution {
            super::routes::Resolution::Alias(substituted) => {
                resolve_chain(surface, &substituted, registry, credentials, evidence, depth + 1)
            }
            super::routes::Resolution::Combo(selectors) => {
                let mut candidates = Vec::new();
                for selector in &selectors {
                    candidates.extend(resolve_chain(
                        surface,
                        selector,
                        registry,
                        credentials,
                        evidence,
                        depth + 1,
                    )?);
                }
                if candidates.is_empty() {
                    Err((404, format!("combo {selector:?} resolved no candidates")))
                } else {
                    Ok(candidates)
                }
            }
        };
    }
    if selector == aifuel_core::AUTO_PROVIDER {
        return auto_candidates(surface, registry, credentials, evidence, None);
    }
    if let Some(filter) = selector.strip_prefix("auto/") {
        return auto_candidates(surface, registry, credentials, evidence, Some(filter.to_owned()));
    }
    if let Some((name, pinned)) = selector.split_once('/') {
        return pinned_candidate(
            surface,
            registry,
            credentials,
            name,
            ModelPin::Pinned(pinned.to_owned()),
        )?
        .map(|candidate| vec![candidate])
        .ok_or_else(|| {
            (
                404,
                format!("unknown integration or provider {name:?} in model {selector:?}"),
            )
        });
    }
    match pinned_candidate(surface, registry, credentials, selector, ModelPin::Default)? {
        Some(candidate) => Ok(vec![candidate]),
        // Not a selector at all: the string is a bare model id, sent
        // verbatim through the ranked chain - the same convention the
        // chat surface applies to catalog model ids.
        None => auto_candidates(surface, registry, credentials, evidence, Some(selector.to_owned())),
    }
}

/// Resolve one selector - an Integration Identity, a Provider Integration
/// instance id, or a bare Provider Id - to its single candidate, or
/// `Ok(None)` when the selector names nothing registered. Resolving a
/// target that cannot serve the surface is an `Err`, never a silent skip:
/// a pin must report the refusal honestly.
fn pinned_candidate(
    surface: &Surface,
    registry: &IntegrationRegistry,
    credentials: &CredentialStore,
    selector: &str,
    model: ModelPin,
) -> Result<Option<Candidate>, (u16, String)> {
    // An exact instance id serves through its base integration under the
    // instance overlay; an exact integration id serves directly. Bare
    // provider names fall through to the registry's selector rule.
    let (descriptor, instance) = match registry.serving(&IntegrationId::new(selector)) {
        Some(pair) => pair,
        None => match registry.resolve(selector) {
            Ok(descriptor) => (descriptor, None),
            Err(aifuel_providers::ResolveError::Unknown { .. }) => return Ok(None),
            Err(aifuel_providers::ResolveError::Ambiguous {
                provider,
                candidates,
            }) => {
                return Err((
                    400,
                    format!(
                        "provider {provider} maps to multiple integrations ({}); name an integration id explicitly",
                        candidates
                            .iter()
                            .map(IntegrationId::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ));
            }
        },
    };
    let ExecutionConfig::Http { endpoint, auth, .. } = &descriptor.integration.execution else {
        return Err((
            400,
            format!(
                "integration {selector:?} cannot serve {}: CLI and OAuth subscription integrations expose no OpenAI-compatible endpoint",
                surface.name
            ),
        ));
    };
    let Some(path) = (surface.path)(descriptor) else {
        return Err((
            400,
            format!(
                "integration {selector:?} has no {} surface",
                surface.name
            ),
        ));
    };
    if matches!(
        auth,
        AuthBinding::ApiKey {
            delivery: KeyDelivery::Cookie { .. },
            ..
        }
    ) {
        return Err((
            400,
            format!(
                "integration {selector:?} binds a browser-session credential for monitoring; it cannot serve {}",
                surface.name
            ),
        ));
    }
    // The instance overlay rebinds the binding's credential slot and
    // resolves its env spec - the same transform
    // `WireExecutionAdapter::for_instance` performs, so an instance id
    // means the same target here as on the run surface.
    let (auth, auth_identity, env) = match instance {
        Some(instance) => {
            let auth = instance.bound_auth(auth).map_err(|reason| (400, reason))?;
            let env = instance
                .resolve_env(credentials)
                .map_err(|error| (400, format!("instance {}: {error}", instance.id)))?;
            let identity = if instance.credential.is_some() {
                instance.id.clone()
            } else {
                descriptor.id().clone()
            };
            (auth, identity, env)
        }
        None => (auth.clone(), descriptor.id().clone(), BTreeMap::new()),
    };
    let model = match model {
        ModelPin::Pinned(model) => Some(model),
        ModelPin::Default => (surface.default_model)(descriptor.provider().as_str())
            .map(str::to_owned)
            .or_else(|| {
                super::route_config::default_declared_model(descriptor.id().as_str())
            }),
    };
    let serving_id = instance.map_or_else(|| descriptor.id().clone(), |i| i.id.clone());
    Ok(Some(Candidate {
        serving_id,
        provider: descriptor.provider().clone(),
        endpoint: endpoint.clone(),
        auth,
        auth_identity,
        env,
        model,
        path,
    }))
}

/// The `auto` chain over surface-eligible descriptors, ranked by
/// evidence - no live probe; the request itself is the reachability
/// check:
///
/// 1. keyed API-key integrations whose credential is present, documented
///    free-tier providers ahead of paid keys (the route planner's
///    partition);
/// 2. OAuth-bound HTTP endpoints whose grant is stored;
/// 3. local `AuthBinding::None` endpoints with present discovery
///    evidence - a `providers.json` entry or a provider-owned marker
///    directory such as `~/.ollama`.
fn auto_candidates(
    surface: &Surface,
    registry: &IntegrationRegistry,
    credentials: &CredentialStore,
    evidence: Option<&EvidenceContext<'_>>,
    model: Option<String>,
) -> Result<Vec<Candidate>, (u16, String)> {
    let mut ranked: Vec<(u8, Candidate)> = Vec::new();
    for descriptor in registry.list() {
        let ExecutionConfig::Http { endpoint, auth, .. } = &descriptor.integration.execution else {
            continue;
        };
        let Some(path) = (surface.path)(descriptor) else {
            continue;
        };
        let rank = match auth {
            AuthBinding::ApiKey { source, delivery } => {
                // A cookie-delivered binding is a `*:web` session surface:
                // present material or not, it is a monitoring credential,
                // never a forwarding target.
                if matches!(delivery, KeyDelivery::Cookie { .. })
                    || !credential_present(source, credentials)
                {
                    continue;
                }
                if aifuel_providers::free_tier_note(descriptor.provider().as_str()).is_some() {
                    0
                } else {
                    1
                }
            }
            AuthBinding::OAuth { credential, .. } => {
                if !credentials.contains_credential(credential).unwrap_or(false) {
                    continue;
                }
                2
            }
            AuthBinding::None => {
                let present = evidence.is_some_and(|context| {
                    matches!(
                        descriptor.discover(context),
                        Ok(aifuel_core::DiscoveryState::Present)
                    )
                });
                if !present {
                    continue;
                }
                3
            }
        };
        ranked.push((
            rank,
            Candidate {
                serving_id: descriptor.id().clone(),
                provider: descriptor.provider().clone(),
                endpoint: endpoint.clone(),
                auth: auth.clone(),
                auth_identity: descriptor.id().clone(),
                env: BTreeMap::new(),
                // `auto` without a model filter sends the integration's
                // first declared model so endpoints that require one can
                // serve the candidate.
                model: model.clone().or_else(|| {
                    super::route_config::default_declared_model(descriptor.id().as_str())
                }),
                path,
            },
        ));
    }
    // A stable sort keeps registry order inside each evidence tier.
    ranked.sort_by_key(|(rank, _)| *rank);
    let candidates: Vec<Candidate> = ranked.into_iter().map(|(_, candidate)| candidate).collect();
    if candidates.is_empty() {
        return Err((
            404,
            format!(
                "no {}-capable integration has a credential or local evidence present; \
                 `aifuel auth set-key` or a running local server is required",
                surface.name
            ),
        ));
    }
    Ok(candidates)
}

/// Whether an API-key source resolves to present material without reading
/// it - the route planner's candidacy check restated: the declared
/// environment variable set, or a managed credential (or any pool member
/// under its Credential Reference) stored. Store errors read as absent.
fn credential_present(source: &ApiKeySource, credentials: &CredentialStore) -> bool {
    let stored = |credential: &aifuel_core::CredentialRef| {
        credentials.contains_credential(credential).unwrap_or(false)
    };
    match source {
        ApiKeySource::Env { var } => aifuel_providers::env_override(var).is_some(),
        ApiKeySource::Store { credential } => stored(credential),
        ApiKeySource::EnvOrStore { var, credential } => {
            aifuel_providers::env_override(var).is_some() || stored(credential)
        }
    }
}

/// The JSON body a forwarding endpoint receives: the caller's fields
/// verbatim, with `model` rewritten to the resolved provider-native id -
/// or removed when a bare selector left the choice to the endpoint's own
/// contract.
pub(crate) fn upstream_body(inbound: &Value, model: Option<&str>) -> Value {
    let mut body = inbound.clone();
    match model {
        Some(model) => body["model"] = Value::String(model.to_owned()),
        None => {
            if let Some(object) = body.as_object_mut() {
                object.remove("model");
            }
        }
    }
    body
}

/// Try each candidate in rank order. Only a transport failure advances
/// the chain - an endpoint that answered owns its response because the
/// request may already have been billed. `send` produces the request for
/// one candidate and returns the forward outcome; the surface name and
/// inbound `model` echo label the failover notes and the request log.
pub(crate) fn serve(
    request: tiny_http::Request,
    surface: &Surface,
    model_echo: &str,
    candidates: &[Candidate],
    credentials: &CredentialStore,
    send: &dyn Fn(&Candidate, &aifuel_providers::ResolvedAuth) -> Result<aifuel_providers::ForwardOutcome, String>,
) {
    let mut failures: Vec<String> = Vec::new();
    for candidate in candidates {
        if let Some(last_failure) = failures.last() {
            eprintln!(
                "aifuel: gateway {} failing over to {} ({last_failure})",
                surface.name, candidate.serving_id
            );
        } else {
            eprintln!(
                "aifuel: gateway {} selected {}",
                surface.name, candidate.serving_id
            );
        }
        // Credential resolution is blocking store I/O; the request thread
        // is the permitted synchronous context, so resolution happens
        // before the async send and material crosses by value inside the
        // outcome. A credential gone since candidacy ranked simply demotes
        // the candidate.
        let auth = match aifuel_providers::oauth::resolve_ready_with_env(
            credentials,
            &candidate.auth,
            &candidate.auth_identity,
            &candidate.env,
        ) {
            Ok(auth) => auth,
            Err(error) => {
                failures.push(format!("{}: {error}", candidate.serving_id));
                continue;
            }
        };
        match send(candidate, &auth) {
            Ok(aifuel_providers::ForwardOutcome::Answered {
                status,
                content_type,
                body,
            }) => {
                answer(
                    request,
                    model_echo,
                    &candidate.serving_id,
                    status,
                    content_type.as_deref(),
                    body,
                );
                return;
            }
            Ok(aifuel_providers::ForwardOutcome::Unreachable { reason }) => {
                failures.push(format!("{}: {reason}", candidate.serving_id));
            }
            // A malformed endpoint config is a failure of that candidate
            // alone - rank order continues to the next.
            Err(error) => {
                failures.push(format!("{}: {error}", candidate.serving_id));
            }
        }
    }
    let message = failures.join("; ");
    record(model_echo, 502, None, None, Some(message.clone()));
    respond_error(request, 502, &message, "server_error");
}

/// Map one answered upstream response onto the gateway contract: a 2xx
/// passes through verbatim under the upstream's own `Content-Type`;
/// anything else folds into the shared OpenAI error envelope, carrying
/// the upstream's own `error.message` when it sent one and degrading 5xx
/// to a 502 the client can act on.
pub(crate) fn answer(
    request: tiny_http::Request,
    model_echo: &str,
    serving_id: &IntegrationId,
    status: u16,
    content_type: Option<&str>,
    body: Vec<u8>,
) {
    if (200..300).contains(&status) {
        let usage = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|json| json.get("usage").cloned());
        respond(request, status, body, content_type, cors_headers());
        record(
            model_echo,
            status,
            Some(serving_id.as_str().to_owned()),
            usage,
            None,
        );
        return;
    }
    let message = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|json| {
            json.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| format!("the endpoint returned HTTP {status}"));
    let status = if status >= 500 { 502 } else { status };
    record(
        model_echo,
        status,
        Some(serving_id.as_str().to_owned()),
        None,
        Some(message.clone()),
    );
    respond_error(request, status, &message, error_kind(status));
}

/// The OpenAI error `type` matching an HTTP status - the vocabulary the
/// Anthropic surface's `error_type` table uses, minus its Anthropic-only
/// names.
fn error_kind(status: u16) -> &'static str {
    match status {
        401 => "authentication_error",
        403 => "permission_error",
        429 => "rate_limit_error",
        400..=499 => "invalid_request_error",
        _ => "server_error",
    }
}

/// Record the terminal outcome in the request log, mirroring `chat`.
pub(crate) fn record(
    model_echo: &str,
    status: u16,
    integration: Option<String>,
    usage: Option<Value>,
    error: Option<String>,
) {
    logs::record(logs::Entry {
        ts_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        model: model_echo.to_owned(),
        integration,
        status,
        stream: false,
        usage,
        error,
    });
}


#[cfg(test)]
mod tests {
    use super::*;
    use aifuel_core::{Integration, ProviderId, WireApi};
    use serde_json::json;

    /// The test surface mirrors `embeddings`: every OpenAiChat endpoint
    /// hosts it, and `openai` documents a default model.
    const TEST_SURFACE: Surface = Surface {
        name: "embeddings",
        path: |descriptor| match &descriptor.integration.execution {
            ExecutionConfig::Http {
                protocol: WireApi::OpenAiChat,
                ..
            } => Some("embeddings"),
            _ => None,
        },
        default_model: |provider| (provider == "openai").then_some("text-embedding-3-small"),
    };

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aifuel-forward-test-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("test dir should be creatable");
        dir
    }

    fn http_descriptor(id: &str, provider: &str, auth: AuthBinding) -> IntegrationDescriptor {
        IntegrationDescriptor::builtin(
            Integration {
                id: IntegrationId::new(id),
                provider: ProviderId::new(provider),
                name: id.to_owned(),
                execution: ExecutionConfig::Http {
                    endpoint: EndpointConfig {
                        base_url: "https://endpoint.test/v1".to_owned(),
                        extra_headers: BTreeMap::new(),
                        request_timeout_seconds: None,
                    },
                    protocol: WireApi::OpenAiChat,
                    auth,
                },
                monitoring: None,
            },
            Vec::new(),
        )
    }

    fn api_key_auth(reference: &str) -> AuthBinding {
        AuthBinding::ApiKey {
            source: ApiKeySource::Store {
                credential: aifuel_core::CredentialRef::new(reference),
            },
            delivery: KeyDelivery::Bearer,
        }
    }

    /// `model` is the routing selector inbound, never the provider's model
    /// id: the upstream body must carry the resolved model, and a bare
    /// selector with no documented default must strip the field rather
    /// than leak the gateway's addressing vocabulary upstream.
    #[test]
    fn upstream_body_rewrites_or_removes_the_selector() {
        let inbound = json!({"model": "auto", "input": ["x"], "encoding_format": "float"});
        let pinned = upstream_body(&inbound, Some("text-embedding-3-small"));
        assert_eq!(pinned["model"], "text-embedding-3-small");
        assert_eq!(pinned["input"], json!(["x"]));
        assert_eq!(pinned["encoding_format"], "float");

        let defaulted = upstream_body(&inbound, None);
        assert!(defaulted.get("model").is_none());
        assert_eq!(defaulted["input"], json!(["x"]));
    }

    /// The error `type` must stay in the OpenAI vocabulary SDKs switch
    /// on: auth failures, permission, rate limit, client errors, and a
    /// server-error catch-all.
    #[test]
    fn error_kind_stays_in_openai_vocabulary() {
        assert_eq!(error_kind(401), "authentication_error");
        assert_eq!(error_kind(403), "permission_error");
        assert_eq!(error_kind(429), "rate_limit_error");
        assert_eq!(error_kind(400), "invalid_request_error");
        assert_eq!(error_kind(404), "invalid_request_error");
        assert_eq!(error_kind(502), "server_error");
    }

    /// The auto chain must rank by the same evidence tiers the route
    /// planner uses: keyed free-tier keys before paid keys, OAuth grants
    /// next, unkeyed local endpoints last - and skip every descriptor
    /// that cannot serve the surface or shows no credential evidence.
    #[test]
    fn auto_candidates_rank_by_credential_and_presence_evidence() {
        let dir = test_dir("auto-rank");
        let credentials = CredentialStore::new(&dir);
        for reference in ["groq:api-key", "openai:api-key", "oauth-grant"] {
            credentials
                .set_api_key(&aifuel_core::CredentialRef::new(reference), "sk")
                .expect("a key stores");
        }
        let registry = IntegrationRegistry::build(
            vec![
                http_descriptor("groq:api-key", "groq", api_key_auth("groq:api-key")),
                http_descriptor("openai:api-key", "openai", api_key_auth("openai:api-key")),
                http_descriptor(
                    "unkeyed:api-key",
                    "unkeyed",
                    api_key_auth("unkeyed:api-key"),
                ),
                http_descriptor(
                    "custom:oauth-http",
                    "custom",
                    AuthBinding::OAuth {
                        credential: aifuel_core::CredentialRef::new("oauth-grant"),
                        profile: aifuel_core::OAuthProfileId::new("custom"),
                    },
                ),
                http_descriptor("local:local", "local", AuthBinding::None),
                IntegrationDescriptor::builtin(
                    Integration {
                        id: IntegrationId::new("claude"),
                        provider: ProviderId::new("claude"),
                        name: "claude".to_owned(),
                        execution: ExecutionConfig::Cli {
                            adapter: aifuel_core::CliAdapterId::new("claude"),
                        },
                        monitoring: None,
                    },
                    Vec::new(),
                ),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            aifuel_core::OptimizePlan::default(),
        )
        .expect("the registry builds");

        let candidates = auto_candidates(
            &TEST_SURFACE,
            &registry,
            &credentials,
            None,
            Some("embed-model".to_owned()),
        )
        .expect("keyed integrations produce candidates");
        let order: Vec<&str> = candidates
            .iter()
            .map(|candidate| candidate.serving_id.as_str())
            .collect();
        // groq has a documented free tier, openai is paid, and the OAuth
        // endpoint follows. `unkeyed:api-key`, the CLI integration, and
        // the unevidenced local endpoint are never candidates - absence
        // of evidence is not presence.
        assert_eq!(
            order,
            ["groq:api-key", "openai:api-key", "custom:oauth-http"]
        );
        assert!(
            candidates
                .iter()
                .all(|c| c.model.as_deref() == Some("embed-model"))
        );
        std::fs::remove_dir_all(&dir).expect("test dir should be removable");
    }

    /// A local endpoint with no discovery evidence - no config entry and
    /// no marker directory - is not an `auto` candidate: absence of
    /// evidence must never be read as presence.
    #[test]
    fn auto_skips_unevidenced_locals_and_empty_chains_are_a_404() {
        let dir = test_dir("auto-empty");
        let credentials = CredentialStore::new(&dir);
        let registry = IntegrationRegistry::build(
            vec![http_descriptor("local:local", "local", AuthBinding::None)],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            aifuel_core::OptimizePlan::default(),
        )
        .expect("the registry builds");
        // No evidence context: local candidacy cannot be established.
        assert!(auto_candidates(&TEST_SURFACE, &registry, &credentials, None, None).is_err());
        std::fs::remove_dir_all(&dir).expect("test dir should be removable");
    }

    /// A pinned selector must refuse honestly: a CLI integration and an
    /// Anthropic-protocol endpoint both own no `/embeddings` surface, and
    /// an unknown name is `None` for the bare-model fallback, not a pin.
    #[test]
    fn pinned_resolution_refuses_ineligible_targets() {
        let dir = test_dir("pinned");
        let credentials = CredentialStore::new(&dir);
        let mut anthropic = http_descriptor("anthropic:api-key", "anthropic", api_key_auth("a"));
        let ExecutionConfig::Http { protocol, .. } = &mut anthropic.integration.execution else {
            panic!("the fixture builds an Http integration");
        };
        *protocol = WireApi::AnthropicMessages;
        let registry = IntegrationRegistry::build(
            vec![
                anthropic,
                http_descriptor("openai:api-key", "openai", api_key_auth("openai:api-key")),
                IntegrationDescriptor::builtin(
                    Integration {
                        id: IntegrationId::new("claude"),
                        provider: ProviderId::new("claude"),
                        name: "claude".to_owned(),
                        execution: ExecutionConfig::Cli {
                            adapter: aifuel_core::CliAdapterId::new("claude"),
                        },
                        monitoring: None,
                    },
                    Vec::new(),
                ),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            aifuel_core::OptimizePlan::default(),
        )
        .expect("the registry builds");

        assert!(matches!(
            pinned_candidate(&TEST_SURFACE, &registry, &credentials, "claude", ModelPin::Default),
            Err((400, _))
        ));
        assert!(matches!(
            pinned_candidate(
                &TEST_SURFACE,
                &registry,
                &credentials,
                "anthropic:api-key",
                ModelPin::Default
            ),
            Err((400, _))
        ));
        assert!(
            pinned_candidate(&TEST_SURFACE, &registry, &credentials, "nobody", ModelPin::Default)
                .expect("an unknown selector is not an error")
                .is_none()
        );

        let pinned = pinned_candidate(
            &TEST_SURFACE,
            &registry,
            &credentials,
            "openai:api-key",
            ModelPin::Pinned("text-embedding-3-large".to_owned()),
        )
        .expect("an eligible integration resolves")
        .expect("the selector names a target");
        assert_eq!(pinned.model.as_deref(), Some("text-embedding-3-large"));
        // A bare openai pin falls back to the documented default model.
        let bare = pinned_candidate(
            &TEST_SURFACE,
            &registry,
            &credentials,
            "openai:api-key",
            ModelPin::Default,
        )
        .expect("an eligible integration resolves")
        .expect("the selector names a target");
        assert_eq!(bare.model.as_deref(), Some("text-embedding-3-small"));
        std::fs::remove_dir_all(&dir).expect("test dir should be removable");
    }
}
