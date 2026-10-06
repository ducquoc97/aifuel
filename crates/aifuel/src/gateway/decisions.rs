//! `POST /v1/decisions`: the structured decision surface proxied to an
//! HTTP-capable Provider Integration - TypeSafe AI's System One (Jev)
//! contract, where a request ships `state` plus typed `questions` and the
//! endpoint answers calibrated choices instead of generating text.
//!
//! The `AgentExecutionAdapter` run contract is prompt-in/text-out and
//! cannot answer a decision, so this endpoint resolves the inbound `model`
//! selector through the shared `forward` machinery and POSTs to the
//! provider's documented decision path. Two provider families serve the
//! contract today:
//!
//! - `typesafe:api-key` - `POST {base_url}/systemone`, the native
//!   System One surface (`WireApi::Decisions`, never a chat engine);
//! - `openrouter:api-key` and `openai:api-key` - OpenAI-compatible roots
//!   that expose the same shape at `/systemone` and `/decisions`
//!   respectively, so a keyed chat integration doubles as a decisions
//!   target.
//!
//! The same surface powers the planner's `decide` selector: `reorder`
//! asks the ranked decision chain which candidate should serve a prompt,
//! and the winner leads the attempt chain while ranked order survives as
//! failover.

use super::{Gateway, forward, read_body, respond_error};
use aifuel_app::MonitoringFacade;
use aifuel_core::{ExecutionConfig, StatusCollector, WireApi};
use aifuel_providers::CredentialStore;
use serde_json::Value;
use serde_json::json;

/// The name the planner's `decide` selector and its failover notes use.
const SURFACE: forward::Surface = forward::Surface {
    name: "decisions",
    path: |descriptor| {
        let protocol = match &descriptor.integration.execution {
            ExecutionConfig::Http { protocol, .. } => *protocol,
            _ => return None,
        };
        match descriptor.provider().as_str() {
            // TypeSafe AI documents `POST /v1/systemone`; OpenRouter
            // relays the same surface for `typesafe/jev-latest`.
            "typesafe" | "openrouter" if matches!(protocol, WireApi::Decisions | WireApi::OpenAiChat) => {
                Some("systemone")
            }
            // OpenAI's Decisions API (Luna) mounts under the
            // OpenAI-compatible root.
            "openai" if protocol == WireApi::OpenAiChat => Some("decisions"),
            _ => None,
        }
    },
    default_model: |provider| match provider {
        "typesafe" => Some("jev-latest"),
        "openrouter" => Some("typesafe/jev-latest"),
        _ => None,
    },
};

/// The decisions hop is a routing probe, not the request itself: a slow
/// endpoint may stall a client call no longer than this budget.
const DECIDE_TIMEOUT_SECONDS: u64 = 10;

/// The `state` payload a routing decision carries - a bounded prefix of
/// the prompt, never the whole transcript.
const DECIDE_STATE_CHARS: usize = 4000;

/// Handle one `/v1/decisions` request end to end.
pub(crate) fn handle<C: StatusCollector>(
    mut request: tiny_http::Request,
    _gateway: &Gateway,
    _facade: &MonitoringFacade<C>,
    runtime: &tokio::runtime::Runtime,
) {
    let body = match read_body(&mut request) {
        Ok(body) => body,
        Err(message) => {
            respond_error(request, 400, message, "invalid_request_error");
            return;
        }
    };
    let inbound: Value = match serde_json::from_slice::<Value>(&body) {
        Ok(value) if value.is_object() => value,
        Ok(_) => {
            respond_error(
                request,
                400,
                "the decisions request must be a JSON object",
                "invalid_request_error",
            );
            return;
        }
        Err(error) => {
            respond_error(
                request,
                400,
                &format!("invalid decisions request: {error}"),
                "invalid_request_error",
            );
            return;
        }
    };
    let model = inbound["model"].as_str().unwrap_or_default().trim();
    if model.is_empty() {
        respond_error(request, 400, "model is required", "invalid_request_error");
        return;
    }
    if let Err(reason) = super::keys::require_permits(&request, model) {
        respond_error(request, 403, &reason, "permission_error");
        return;
    }
    if inbound.get("state").is_none() && inbound.get("questions").is_none() {
        respond_error(
            request,
            400,
            "a decisions request needs `state` and `questions`",
            "invalid_request_error",
        );
        return;
    }
    let registry = match crate::integration_registry() {
        Ok(registry) => registry,
        Err(error) => {
            respond_error(request, 500, &error, "server_error");
            return;
        }
    };
    let config_dir = match crate::aifuel_config_dir() {
        Ok(config_dir) => config_dir,
        Err(error) => {
            respond_error(request, 500, &error, "server_error");
            return;
        }
    };
    let credentials = CredentialStore::new(config_dir);
    let discovery = aifuel_providers::DiscoveryContext::from_environment().ok();
    let evidence = discovery
        .as_ref()
        .map(|discovery| registry.evidence_context(discovery, &credentials));
    let candidates = match forward::resolve_chain(
        &SURFACE,
        model,
        &registry,
        &credentials,
        evidence.as_ref(),
        0,
    ) {
        Ok(candidates) => candidates,
        Err((status, message)) => {
            respond_error(request, status, &message, "invalid_request_error");
            return;
        }
    };
    forward::serve(
        request,
        &SURFACE,
        model,
        &candidates,
        &credentials,
        &|candidate, auth| {
            let body = forward::upstream_body(&inbound, candidate.model.as_deref());
            runtime.block_on(aifuel_providers::post_json(
                &candidate.endpoint,
                auth,
                candidate.path,
                &body,
            ))
        },
    );
}

/// Ask the ranked decision chain which of `options` should serve `state`,
/// then move the winner to the front of `attempts`. This is an advisory
/// hop: no decisions-capable integration, an unreachable chain, or an
/// unparseable answer all keep the evidence-ranked order rather than
/// failing a servable request - the planner notes the skip on stderr.
///
/// `label` turns one attempt into the criterion text the decision model
/// sees; `options` order must equal `attempts` order.
pub(crate) fn reorder(
    runtime: &tokio::runtime::Runtime,
    state: &str,
    attempts: &mut Vec<super::execute::Attempt>,
) {
    let options: Vec<String> = attempts
        .iter()
        .map(|attempt| {
            let mut label = format!("{}", attempt.integration.as_str());
            if let Some(model) = &attempt.model {
                label.push_str(&format!(" serving model {model}"));
            }
            if let Some(effort) = &attempt.effort {
                label.push_str(&format!(" at effort {effort}"));
            }
            label
        })
        .collect();
    let state: String = state.chars().take(DECIDE_STATE_CHARS).collect();
    match pick(runtime, &state, &options) {
        Ok(Some(winner)) if winner > 0 && winner < attempts.len() => {
            eprintln!(
                "aifuel: gateway decide picked {}",
                attempts[winner].integration
            );
            let attempt = attempts.remove(winner);
            attempts.insert(0, attempt);
        }
        Ok(Some(_)) | Ok(None) => {}
        Err(reason) => eprintln!("aifuel: gateway decide unavailable ({reason}) - ranked order stands"),
    }
}

/// Ask the first reachable decisions candidate to choose among `options`.
/// `Ok(None)` means every candidate failed or none answered with a usable
/// choice - the caller keeps its own ranking.
fn pick(
    runtime: &tokio::runtime::Runtime,
    state: &str,
    options: &[String],
) -> Result<Option<usize>, String> {
    let registry = crate::integration_registry()?;
    let credentials = CredentialStore::new(crate::aifuel_config_dir()?);
    let discovery = aifuel_providers::DiscoveryContext::from_environment().ok();
    let evidence = discovery
        .as_ref()
        .map(|discovery| registry.evidence_context(discovery, &credentials));
    let candidates = forward::resolve_chain(
        &SURFACE,
        "auto",
        &registry,
        &credentials,
        evidence.as_ref(),
        0,
    )
    .map_err(|(_, message)| message)?;
    for candidate in &candidates {
        let auth = match credentials.resolve_with_env(
            &candidate.auth,
            &candidate.auth_identity,
            &candidate.env,
        ) {
            Ok(auth) => auth,
            Err(error) => {
                eprintln!("aifuel: gateway decide skipping {} ({error})", candidate.serving_id);
                continue;
            }
        };
        let mut endpoint = candidate.endpoint.clone();
        endpoint.request_timeout_seconds = Some(DECIDE_TIMEOUT_SECONDS);
        let body = question_body(
            state,
            options,
            candidate
                .model
                .as_deref()
                .or_else(|| (SURFACE.default_model)(candidate.provider.as_str())),
        );
        match runtime.block_on(aifuel_providers::post_json(
            &endpoint,
            &auth,
            candidate.path,
            &body,
        )) {
            Ok(aifuel_providers::ForwardOutcome::Answered { status, body, .. })
                if (200..300).contains(&status) =>
            {
                return Ok(parse_choice(&body, options.len()));
            }
            Ok(aifuel_providers::ForwardOutcome::Answered { status, .. }) => {
                eprintln!(
                    "aifuel: gateway decide: {} answered HTTP {status}",
                    candidate.serving_id
                );
            }
            Ok(aifuel_providers::ForwardOutcome::Unreachable { reason }) => {
                eprintln!(
                    "aifuel: gateway decide: {} unreachable ({reason})",
                    candidate.serving_id
                );
            }
            Err(error) => {
                eprintln!(
                    "aifuel: gateway decide: {} misconfigured ({error})",
                    candidate.serving_id
                );
            }
        }
    }
    Ok(None)
}

/// The System One question shape: one `choice` question whose criteria
/// map candidate indices to their labels. `model` falls back to the
/// provider's documented decision model when the selector pinned none.
fn question_body(state: &str, options: &[String], model: Option<&str>) -> Value {
    let criteria: serde_json::Map<String, Value> = options
        .iter()
        .enumerate()
        .map(|(index, label)| (index.to_string(), Value::String(label.clone())))
        .collect();
    let mut body = json!({
        "state": state,
        "questions": {
            "route": {
                "type": "choice",
                "instructions": "Choose which integration should serve this request. Prefer the candidate most likely to answer it well; break ties toward the cheaper or faster option.",
                "criteria": criteria,
            }
        }
    });
    if let Some(model) = model {
        body["model"] = Value::String(model.to_owned());
    }
    body
}

/// Read `answers.route.choice` from a System One answer: the criterion
/// key may arrive as a string or a number. A malformed or out-of-range
/// answer is no decision at all - `None` keeps the ranked order.
fn parse_choice(body: &[u8], option_count: usize) -> Option<usize> {
    let json: Value = serde_json::from_slice(body).ok()?;
    let choice = json.pointer("/answers/route/choice")?;
    let index = choice
        .as_str()
        .and_then(|text| text.parse::<usize>().ok())
        .or_else(|| choice.as_u64().map(|value| value as usize))?;
    (index < option_count).then_some(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decision body's System One shape: a bounded state, one choice
    /// question, criteria keyed by candidate index, `model` present only
    /// when resolved.
    #[test]
    fn question_body_carries_state_criteria_and_model() {
        let body = question_body(
            "what is x",
            &["a/integration serving m".to_owned(), "b".to_owned()],
            Some("jev-latest"),
        );
        assert_eq!(body["state"], "what is x");
        assert_eq!(body["questions"]["route"]["type"], "choice");
        assert_eq!(body["questions"]["route"]["criteria"]["0"], "a/integration serving m");
        assert_eq!(body["questions"]["route"]["criteria"]["1"], "b");
        assert_eq!(body["model"], "jev-latest");

        let bare = question_body("s", &["x".to_owned()], None);
        assert!(bare.get("model").is_none());
    }

    /// `answers.route.choice` accepts a string or numeric key inside the
    /// option range; anything else is not a decision.
    #[test]
    fn parse_choice_reads_the_criterion_key_only() {
        let body = serde_json::to_vec(&json!({
            "answers": {"route": {"type": "choice", "choice": "1"}}
        }))
        .unwrap();
        assert_eq!(parse_choice(&body, 3), Some(1));

        let numeric = serde_json::to_vec(&json!({
            "answers": {"route": {"choice": 0}}
        }))
        .unwrap();
        assert_eq!(parse_choice(&numeric, 3), Some(0));

        // Out of range, missing, and malformed answers decide nothing.
        let high = serde_json::to_vec(&json!({"answers": {"route": {"choice": "9"}}})).unwrap();
        assert_eq!(parse_choice(&high, 3), None);
        let none = serde_json::to_vec(&json!({"answers": {}})).unwrap();
        assert_eq!(parse_choice(&none, 3), None);
        assert_eq!(parse_choice(b"not json", 3), None);
    }

    /// The surface declares the documented path only where the provider
    /// has a decisions surface: typesafe and openrouter relay System One,
    /// openai hosts the Decisions API, everything else answers `None`.
    #[test]
    fn surface_maps_only_providers_with_a_decision_endpoint() {
        use aifuel_core::{AuthBinding, EndpointConfig, Integration, IntegrationId, ProviderId};
        use aifuel_providers::IntegrationDescriptor;

        let descriptor = |provider: &str, protocol: WireApi| {
            IntegrationDescriptor::builtin(
                Integration {
                    id: IntegrationId::new(provider),
                    provider: ProviderId::new(provider),
                    name: provider.to_owned(),
                    execution: ExecutionConfig::Http {
                        endpoint: EndpointConfig {
                            base_url: "https://e.test/v1".to_owned(),
                            extra_headers: Default::default(),
                            request_timeout_seconds: None,
                        },
                        protocol,
                        auth: AuthBinding::None,
                    },
                    monitoring: None,
                },
                Vec::new(),
            )
        };
        assert_eq!((SURFACE.path)(&descriptor("typesafe", WireApi::Decisions)), Some("systemone"));
        assert_eq!((SURFACE.path)(&descriptor("typesafe", WireApi::OpenAiChat)), Some("systemone"));
        assert_eq!((SURFACE.path)(&descriptor("openrouter", WireApi::OpenAiChat)), Some("systemone"));
        assert_eq!((SURFACE.path)(&descriptor("openai", WireApi::OpenAiChat)), Some("decisions"));
        assert_eq!((SURFACE.path)(&descriptor("openai", WireApi::Decisions)), None);
        assert_eq!((SURFACE.path)(&descriptor("groq", WireApi::OpenAiChat)), None);
    }
}
