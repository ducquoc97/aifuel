//! `POST /v1/embeddings`: the OpenAI embeddings contract proxied to an
//! HTTP-capable Provider Integration, the way OmniRoute fronts its
//! OpenAI-compatible providers.
//!
//! The `AgentExecutionAdapter` run contract is prompt-in/text-out and
//! cannot produce embeddings, so this endpoint resolves the inbound
//! `model` selector to an `ExecutionConfig::Http` integration speaking
//! `WireApi::OpenAiChat` and POSTs `{base_url}/embeddings` directly - the
//! sibling of `/chat/completions` under the same OpenAI-compatible root.
//! CLI and `*:oauth` subscription integrations own no endpoint and are
//! refused honestly; `*:web` session bindings stay monitoring surfaces.
//!
//! Selector resolution, candidate ranking, failover, and response mapping
//! are the shared `forward` machinery; this module keeps the embeddings
//! surface declaration (`SURFACE`), input validation, and the send loop.
//! `input` accepts the OpenAI string and string-batch shapes only -
//! token-id arrays and other types are rejected rather than reinterpreted.

use super::{Gateway, forward, read_body, respond_error};
use aifuel_app::MonitoringFacade;
use aifuel_core::{ExecutionConfig, StatusCollector, WireApi};
use aifuel_providers::CredentialStore;
use serde_json::Value;

/// The fallback a bare `openai` provider selection sends: the cheap
/// default-size embedding model OpenAI's own documentation steers `model`
/// to.
const OPENAI_DEFAULT_MODEL: &str = "text-embedding-3-small";

/// The embeddings surface: every `OpenAiChat` endpoint hosts
/// `POST {base_url}/embeddings`, and OpenAI is the only builtin provider
/// with a documented default model.
const SURFACE: forward::Surface = forward::Surface {
    name: "embeddings",
    path: |descriptor| match &descriptor.integration.execution {
        ExecutionConfig::Http {
            protocol: WireApi::OpenAiChat,
            ..
        } => Some("embeddings"),
        _ => None,
    },
    default_model: |provider| (provider == "openai").then_some(OPENAI_DEFAULT_MODEL),
};

/// Handle one `/v1/embeddings` request end to end.
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
                "the embeddings request must be a JSON object",
                "invalid_request_error",
            );
            return;
        }
        Err(error) => {
            respond_error(
                request,
                400,
                &format!("invalid embeddings request: {error}"),
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
    if let Err(message) = valid_input(inbound.get("input")) {
        respond_error(request, 400, message, "invalid_request_error");
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
    // Local-endpoint evidence needs the home-directory context; when it is
    // unavailable the auto chain simply has no local candidates rather
    // than failing requests keyed integrations could have served.
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
    serve(request, model, &inbound, &candidates, &credentials, runtime);
}

/// The OpenAI `input` contract as served here: one string, or a non-empty
/// array of strings. Token-id arrays and other shapes are refused - the
/// gateway never silently reinterprets input it cannot type-check.
fn valid_input(input: Option<&Value>) -> Result<(), &'static str> {
    match input {
        None | Some(Value::Null) => Err("input is required"),
        Some(Value::String(_)) => Ok(()),
        Some(Value::Array(items)) => {
            if items.is_empty() {
                Err("input must not be an empty array")
            } else if items.iter().all(Value::is_string) {
                Ok(())
            } else {
                Err("input array items must be strings; token-id arrays are not supported")
            }
        }
        Some(_) => Err("input must be a string or an array of strings"),
    }
}

/// Try each candidate in rank order through the shared failover loop.
/// The send builds the upstream body per candidate - the caller's fields
/// verbatim with `model` rewritten to the resolved provider-native id.
fn serve(
    request: tiny_http::Request,
    model_echo: &str,
    inbound: &Value,
    candidates: &[forward::Candidate],
    credentials: &CredentialStore,
    runtime: &tokio::runtime::Runtime,
) {
    forward::serve(request, &SURFACE, model_echo, candidates, credentials, &|candidate, auth| {
        let body = forward::upstream_body(inbound, candidate.model.as_deref());
        runtime
            .block_on(aifuel_providers::embeddings(&candidate.endpoint, auth, &body))
            .map(|outcome| match outcome {
                aifuel_providers::EmbeddingsOutcome::Answered { status, body } => {
                    aifuel_providers::ForwardOutcome::Answered {
                        status,
                        content_type: Some("application/json".to_owned()),
                        body,
                    }
                }
                aifuel_providers::EmbeddingsOutcome::Unreachable { reason } => {
                    aifuel_providers::ForwardOutcome::Unreachable { reason }
                }
            })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The request must accept exactly the two OpenAI input shapes - one
    /// string or a non-empty string array - and refuse everything else
    /// without guessing: token-id arrays are valid upstream but carry no
    /// text the gateway can type-check.
    #[test]
    fn input_accepts_the_openai_text_shapes_only() {
        assert!(valid_input(Some(&json!("one text"))).is_ok());
        assert!(valid_input(Some(&json!(["a", "b"]))).is_ok());
        assert!(valid_input(None).is_err());
        assert!(valid_input(Some(&Value::Null)).is_err());
        assert!(valid_input(Some(&json!([]))).is_err());
        assert!(valid_input(Some(&json!([1, 2, 3]))).is_err());
        assert!(valid_input(Some(&json!(["a", 1]))).is_err());
        assert!(valid_input(Some(&json!(5))).is_err());
    }
}
