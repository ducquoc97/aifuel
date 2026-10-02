//! Validation of raw `providers.json` entries into Integration Descriptors.
//!
//! Split from `config.rs`: the decode schema and file loading stay in the
//! parent module; everything past envelope shape - per-entry field rules,
//! the mutually exclusive execution arms, compiled-capability checks, and
//! the managed-auth header collision rule - lives here.

use super::{ConfigAuth, ConfigError, ConfigIntegration, ConfigMonitoring, describe_json_error};
use crate::integrations::evidence::{EvidenceSource, auth_sources};
use crate::integrations::registry::IntegrationDescriptor;
use aifuel_core::{
    ApiKeySource, AuthBinding, CliAdapterId, CollectorId, CredentialRef, EndpointConfig,
    ExecutionConfig, Integration, IntegrationId, KeyDelivery, MonitoringConfig, OAuthProfileId,
    ProviderId, WireApi,
};

/// Validate one raw entry into a descriptor. `ConfigError` carries the
/// entry's index and its id text (when the value even decoded far enough to
/// have one) so the report names the offender.
pub(super) fn build_descriptor(
    index: usize,
    raw: serde_json::Value,
) -> Result<IntegrationDescriptor, ConfigError> {
    let raw_id = raw.get("id").and_then(|id| id.as_str()).map(str::to_owned);
    let entry: ConfigIntegration =
        serde_json::from_value(raw).map_err(|error| ConfigError::InvalidIntegration {
            index,
            id: raw_id.clone(),
            reason: describe_json_error(&error),
        })?;
    let invalid = |reason: String| ConfigError::InvalidIntegration {
        index,
        id: raw_id.clone(),
        reason,
    };

    if entry.id.trim().is_empty() {
        return Err(invalid("integration id must not be empty".to_owned()));
    }
    if entry.provider_id.trim().is_empty() {
        return Err(invalid("provider_id must not be empty".to_owned()));
    }
    // `auto` is the reserved `--provider` alias for automatic routing; a
    // literal integration or provider named `auto` would shadow it.
    if entry.id.trim() == aifuel_core::AUTO_PROVIDER {
        return Err(invalid(format!(
            "integration id '{}' is reserved for automatic Provider routing",
            aifuel_core::AUTO_PROVIDER
        )));
    }
    if entry.provider_id.trim() == aifuel_core::AUTO_PROVIDER {
        return Err(invalid(format!(
            "provider_id '{}' is reserved for automatic Provider routing",
            aifuel_core::AUTO_PROVIDER
        )));
    }

    let monitoring = entry
        .monitoring
        .as_ref()
        .map(|monitoring| validate_monitoring(monitoring, &invalid))
        .transpose()?;

    let (execution, auth) = if let Some(cli) = &entry.cli {
        if entry.endpoint.is_some() || entry.wire_api.is_some() || entry.auth.is_some() {
            return Err(invalid(
                "a cli integration accepts no endpoint, wire_api, or auth; the provider CLI owns its credential"
                    .to_owned(),
            ));
        }
        let adapter = cli.adapter.trim();
        if adapter.is_empty() {
            return Err(invalid("cli.adapter must not be empty".to_owned()));
        }
        let compiled = crate::agent_run_adapters()
            .iter()
            .any(|candidate| candidate.integration().as_str() == adapter);
        if !compiled {
            return Err(invalid(format!(
                "cli.adapter '{adapter}' names no compiled CLI adapter"
            )));
        }
        (
            ExecutionConfig::Cli {
                adapter: CliAdapterId::new(adapter),
            },
            None,
        )
    } else {
        let endpoint = entry.endpoint.as_ref().ok_or_else(|| {
            invalid("an entry needs either 'cli' or 'endpoint' execution config".to_owned())
        })?;
        let wire_api = entry
            .wire_api
            .as_deref()
            .ok_or_else(|| invalid("an endpoint integration requires wire_api".to_owned()))?;
        let protocol = match wire_api {
            "openai-chat" => WireApi::OpenAiChat,
            "openai-responses" => WireApi::OpenAiResponses,
            "anthropic-messages" => WireApi::AnthropicMessages,
            other => {
                return Err(invalid(format!(
                    "wire_api '{other}' is unknown; known values: openai-chat, openai-responses, anthropic-messages"
                )));
            }
        };
        if !crate::wire::serves(protocol) {
            return Err(invalid(format!(
                "wire_api '{wire_api}' names a compiled protocol with no execution engine in this build; serveable: openai-chat, anthropic-messages"
            )));
        }
        let auth = entry
            .auth
            .as_ref()
            .ok_or_else(|| {
                invalid(
                    "an endpoint integration requires auth; declare 'none' explicitly".to_owned(),
                )
            })
            .and_then(|auth| validate_auth(auth).map_err(|reason| invalid(reason)))?;

        let base_url = validate_base_url(&endpoint.base_url, &invalid)?;

        // Custom headers cannot override managed auth: reject a header whose
        // name collides with the header the declared binding applies. The wire
        // layer applies managed auth last as well, but rejecting at the boundary
        // turns a silent override into an actionable config error.
        let headers = endpoint.headers.clone();
        if let Some(managed) = managed_auth_header(&auth)
            && let Some(conflict) = headers
                .keys()
                .find(|name| name.eq_ignore_ascii_case(managed))
        {
            return Err(invalid(format!(
                "endpoint.headers sets '{conflict}', which the auth binding manages; remove it"
            )));
        }
        if let Some(empty) = headers.keys().find(|name| name.trim().is_empty()) {
            return Err(invalid(format!("endpoint.header name '{empty}' is empty")));
        }

        (
            ExecutionConfig::Http {
                endpoint: EndpointConfig {
                    base_url,
                    extra_headers: headers,
                    request_timeout_seconds: endpoint.request_timeout_seconds,
                },
                protocol,
                auth: auth.clone(),
            },
            Some(auth),
        )
    };

    let name = entry
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| entry.id.clone());
    let mut sources = vec![EvidenceSource::ConfiguredEndpoint {
        marker_directories: Vec::new(),
    }];
    if let Some(auth) = &auth {
        sources.extend(auth_sources(auth));
    }

    Ok(IntegrationDescriptor::configured(
        Integration {
            id: IntegrationId::new(entry.id),
            provider: ProviderId::new(entry.provider_id),
            name,
            execution,
            monitoring,
        },
        sources,
    ))
}

/// Validate a `monitoring` object into a Monitoring Collection Contract.
/// The collector names a compiled collector; an unknown id is an explicit
/// error, never a fallback.
fn validate_monitoring(
    monitoring: &ConfigMonitoring,
    invalid: &dyn Fn(String) -> ConfigError,
) -> Result<MonitoringConfig, ConfigError> {
    if monitoring.collector.trim().is_empty() {
        return Err(invalid("monitoring.collector must not be empty".to_owned()));
    }
    if !crate::quota::compiled_collector_ids().contains(&monitoring.collector.as_str()) {
        return Err(invalid(format!(
            "monitoring.collector '{}' names no compiled collector; known values: {}",
            monitoring.collector,
            crate::quota::compiled_collector_ids().join(", ")
        )));
    }
    let credential = monitoring
        .credential
        .as_deref()
        .map(|credential| {
            if credential.trim().is_empty() {
                Err(invalid(
                    "monitoring.credential must not be empty".to_owned(),
                ))
            } else {
                Ok(CredentialRef::new(credential))
            }
        })
        .transpose()?;
    let endpoint = monitoring
        .endpoint
        .as_ref()
        .map(|endpoint| {
            for (name, value) in &endpoint.headers {
                if name.parse::<reqwest::header::HeaderName>().is_err() {
                    return Err(invalid(format!(
                        "monitoring endpoint header {name:?} is not a valid header name"
                    )));
                }
                if value.parse::<reqwest::header::HeaderValue>().is_err() {
                    return Err(invalid(format!(
                        "monitoring endpoint header {name:?} is not a valid header value"
                    )));
                }
            }
            Ok::<_, ConfigError>(EndpointConfig {
                base_url: validate_base_url(&endpoint.base_url, invalid)?,
                extra_headers: endpoint.headers.clone(),
                request_timeout_seconds: endpoint.request_timeout_seconds,
            })
        })
        .transpose()?;
    Ok(MonitoringConfig {
        collector: CollectorId::new(monitoring.collector.clone()),
        credential,
        endpoint,
    })
}

/// A parseable http(s) base URL, or an entry error naming the offender.
fn validate_base_url(
    base_url: &str,
    invalid: &dyn Fn(String) -> ConfigError,
) -> Result<String, ConfigError> {
    match reqwest::Url::parse(base_url) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => Ok(base_url.to_owned()),
        Ok(_) => Err(invalid(format!(
            "endpoint.base_url '{base_url}' must use http or https"
        ))),
        Err(error) => Err(invalid(format!(
            "endpoint.base_url '{base_url}' is not a URL: {error}"
        ))),
    }
}

/// The required `var` field of an env-binding auth kind: non-empty and legal
/// to pass to `std::env::var` (which panics on `=` or NUL), so a malformed
/// entry is a config error here rather than a crash at resolution time.
fn env_var_field(auth: &ConfigAuth, kind: &str) -> Result<String, String> {
    let var = auth
        .var
        .clone()
        .filter(|var| !var.trim().is_empty())
        .ok_or_else(|| format!("auth kind '{kind}' requires a non-empty var"))?;
    if !crate::valid_env_var_name(&var) {
        return Err(format!(
            "auth kind '{kind}' var {var:?} is not a valid environment variable name"
        ));
    }
    Ok(var)
}

/// Validate the flat auth object by `kind`, requiring exactly the fields the
/// kind declares and rejecting extras that would be silently ignored.
fn validate_auth(auth: &ConfigAuth) -> Result<AuthBinding, String> {
    match auth.kind.as_str() {
        "none" => {
            if auth.var.is_some()
                || auth.credential.is_some()
                || auth.profile.is_some()
                || auth.delivery.is_some()
            {
                return Err(
                    "auth kind 'none' accepts no var, credential, profile, or delivery".to_owned(),
                );
            }
            Ok(AuthBinding::None)
        }
        "api-key-env" => {
            if auth.credential.is_some() || auth.profile.is_some() {
                return Err("auth kind 'api-key-env' accepts only var and delivery".to_owned());
            }
            let var = env_var_field(auth, "api-key-env")?;
            Ok(AuthBinding::ApiKey {
                source: ApiKeySource::Env { var },
                delivery: auth.delivery.clone().unwrap_or(KeyDelivery::Bearer),
            })
        }
        "api-key-ref" => {
            if auth.var.is_some() || auth.profile.is_some() {
                return Err(
                    "auth kind 'api-key-ref' accepts only credential and delivery".to_owned(),
                );
            }
            let credential = auth
                .credential
                .clone()
                .filter(|credential| !credential.trim().is_empty())
                .ok_or_else(|| {
                    "auth kind 'api-key-ref' requires a non-empty credential".to_owned()
                })?;
            Ok(AuthBinding::ApiKey {
                source: ApiKeySource::Store {
                    credential: CredentialRef::new(credential),
                },
                delivery: auth.delivery.clone().unwrap_or(KeyDelivery::Bearer),
            })
        }
        "api-key-env-or-store" => {
            if auth.profile.is_some() {
                return Err(
                    "auth kind 'api-key-env-or-store' accepts only var, credential, and delivery"
                        .to_owned(),
                );
            }
            let var = env_var_field(auth, "api-key-env-or-store")?;
            let credential = auth
                .credential
                .clone()
                .filter(|credential| !credential.trim().is_empty())
                .ok_or_else(|| {
                    "auth kind 'api-key-env-or-store' requires a non-empty credential".to_owned()
                })?;
            Ok(AuthBinding::ApiKey {
                source: ApiKeySource::EnvOrStore {
                    var,
                    credential: CredentialRef::new(credential),
                },
                delivery: auth.delivery.clone().unwrap_or(KeyDelivery::Bearer),
            })
        }
        "oauth-ref" => {
            if auth.var.is_some() || auth.delivery.is_some() {
                return Err("auth kind 'oauth-ref' accepts only credential and profile".to_owned());
            }
            let credential = auth
                .credential
                .clone()
                .filter(|credential| !credential.trim().is_empty())
                .ok_or_else(|| {
                    "auth kind 'oauth-ref' requires a non-empty credential".to_owned()
                })?;
            let profile = auth
                .profile
                .clone()
                .filter(|profile| !profile.trim().is_empty())
                .ok_or_else(|| "auth kind 'oauth-ref' requires a non-empty profile".to_owned())?;
            Ok(AuthBinding::OAuth {
                credential: CredentialRef::new(credential),
                profile: OAuthProfileId::new(profile),
            })
        }
        other => Err(format!(
            "auth kind '{other}' is unknown; known values: none, api-key-env, api-key-ref, api-key-env-or-store, oauth-ref"
        )),
    }
}

/// The header a managed auth binding applies last, for the custom-header
/// collision check.
fn managed_auth_header(auth: &AuthBinding) -> Option<&str> {
    match auth {
        AuthBinding::None => None,
        AuthBinding::ApiKey { delivery, .. } => Some(match delivery {
            KeyDelivery::Bearer => "authorization",
            KeyDelivery::Header { name } => name.as_str(),
            KeyDelivery::Cookie { .. } => "cookie",
        }),
        AuthBinding::OAuth { .. } => Some("authorization"),
    }
}
