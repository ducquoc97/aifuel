//! The dashboard Connect panel's view of Provider Integrations.
//!
//! Lists the `AuthBinding::ApiKey` HTTP integrations a user can attach an
//! API-key Managed Credential to, with the same credential-source
//! description `aifuel auth list` prints, and applies the same Credential
//! Store operations as `aifuel auth set-key` / `aifuel auth remove`. Secret
//! material is only ever accepted for storage - it is never returned.

use aifuel_core::{ApiKeySource, AuthBinding, CredentialRef, ExecutionConfig};
use aifuel_providers::{CredentialMetadata, env_override};
use serde::Serialize;
use std::collections::BTreeMap;

/// One Connect panel row: an API-key Provider Integration plus its live
/// credential-source state.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectEntry {
    /// The Integration Identity the form submits to `store_key`.
    pub id: String,
    /// The integration display name.
    pub name: String,
    /// The credential-source description `aifuel auth list` reports.
    pub source: String,
    /// The Credential Reference the binding declares, when it can resolve a
    /// Managed Credential. The dashboard submits it to `remove_credential`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// The environment variable the binding declares, when it names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_var: Option<String>,
    /// Whether that variable currently carries a value.
    pub env_set: bool,
    /// Whether a Managed Credential is stored at the binding's reference.
    pub stored: bool,
    /// Whether the binding's source can accept `store_key` at all (a
    /// store-capable source, not env-only).
    pub accepts_key: bool,
}

/// Every `AuthBinding::ApiKey` HTTP integration with its live source state.
///
/// The source strings mirror `aifuel auth list` so the dashboard and the CLI
/// describe the same credential posture.
pub fn entries() -> Result<Vec<ConnectEntry>, String> {
    let registry = crate::integration_registry()?;
    let store = crate::auth_cli::credential_store()?;
    let listed = store.list().map_err(|error| error.to_string())?;
    let stored: BTreeMap<&CredentialRef, &CredentialMetadata> = listed
        .iter()
        .map(|(reference, meta)| (reference, meta))
        .collect();

    Ok(registry
        .list()
        .filter(|descriptor| {
            matches!(
                descriptor.integration.execution,
                ExecutionConfig::Http {
                    auth: AuthBinding::ApiKey { .. },
                    ..
                }
            )
        })
        .map(|descriptor| {
            let (env_var, credential) = match &descriptor.integration.execution {
                ExecutionConfig::Http {
                    auth: AuthBinding::ApiKey { source, .. },
                    ..
                } => match source {
                    ApiKeySource::Env { var } => (Some(var.clone()), None),
                    ApiKeySource::Store { credential } => (None, Some(credential.clone())),
                    ApiKeySource::EnvOrStore { var, credential } => {
                        (Some(var.clone()), Some(credential.clone()))
                    }
                },
                _ => (None, None),
            };
            ConnectEntry {
                id: descriptor.integration.id.as_str().to_owned(),
                name: descriptor.integration.name.clone(),
                source: crate::auth_cli::describe_source(descriptor, &stored),
                env_set: env_var.as_deref().and_then(env_override).is_some(),
                stored: credential
                    .as_ref()
                    .is_some_and(|reference| stored.contains_key(reference)),
                accepts_key: credential.is_some(),
                credential: credential.map(|reference| reference.as_str().to_owned()),
                env_var,
            }
        })
        .collect())
}

/// Store `material` as the Managed Credential `integration`'s Authentication
/// Binding declares - the same operation `aifuel auth set-key <integration>`
/// performs, including its refusal of env-only, no-auth, OAuth, and CLI
/// bindings. Unlike the CLI a raw Credential Reference is not accepted: the
/// panel only submits registered Integration Identities.
pub fn store_key(integration: &str, material: &str) -> Result<(), String> {
    let material = material.trim();
    if material.is_empty() {
        return Err("the API key is empty".to_owned());
    }
    let (reference, destination) = crate::auth_cli::resolve_credential_ref(integration)?;
    let Some(destination) = destination else {
        return Err(format!("{integration} is not a registered integration id"));
    };
    crate::auth_cli::credential_store()?
        .set_api_key_for(&reference, material, &destination)
        .map_err(|error| error.to_string())
}

/// Delete the Managed Credential `credential` names - the same operation
/// `aifuel auth remove <credential>` performs, returning its notices: pool
/// members that kept an integration authenticating, and warnings about
/// integrations that still bind the reference or keep an env var active.
pub fn remove_credential(credential: &str) -> Result<Vec<String>, String> {
    let reference = CredentialRef::new(credential);
    let removed = crate::auth_cli::credential_store()?
        .remove(&reference)
        .map_err(|error| error.to_string())?;
    if !removed {
        return Err(format!("no managed credential named {credential}"));
    }
    let (notes, warnings) = crate::auth_cli::removal_warnings(&reference)?;
    Ok(notes.into_iter().chain(warnings).collect())
}
