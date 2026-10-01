//! Named Provider Integration instances: the `instances` map inside
//! `providers.json` (spec `docs/specs/provider-integrations.md`).
//!
//! An instance is a named selection overlay on one base integration - e.g.
//! `claude.work` and `claude.personal` over the `claude` integration. It
//! never changes what the integration is: the serving adapter, declared
//! capabilities, and provider identity all stay the base integration's, so
//! an instance cannot fabricate support the integration does not have.
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "instances": {
//!     "claude.work": {
//!       "integration": "claude",
//!       "env": {
//!         "CLAUDE_CONFIG_DIR": "/home/me/.claude-work",
//!         "ANTHROPIC_AUTH_TOKEN": { "credential": "claude-work-key" }
//!       },
//!       "credential": "claude-work-key"
//!     }
//!   }
//! }
//! ```
//!
//! - `integration` names the base Provider Integration by exact id.
//! - `env` maps variable names to either a literal string or
//!   `{"credential": "ref"}`, a Managed Credential reference resolved from
//!   the Credential Store at provider-process spawn. Resolved values are
//!   never logged, persisted, or echoed in list/show output.
//! - `credential` binds a named Managed Credential to the instance: it is
//!   the destination `aifuel auth set-key <instance>` writes, and it rebinds
//!   the credential slot of an HTTP integration's Authentication Binding.
//!
//! Selection rule: an instance is reachable only by its exact id. A bare
//! base-integration selector always means the base integration itself -
//! instances never redefine what an existing selector resolves to, so a
//! stored `claude` selection cannot silently start running `claude.work`'s
//! environment.

use super::config::ConfigError;
use crate::credentials::{CredentialStore, CredentialStoreError};
use aifuel_core::{ApiKeySource, AuthBinding, CredentialRef, IntegrationId};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

/// Where one instance environment variable's value comes from.
///
/// `Debug` names the source, never the value: a `Literal`'s text and a
/// `Credential`'s resolved material are both environment configuration
/// the user may treat as sensitive.
#[derive(Clone, PartialEq, Eq)]
pub enum InstanceEnvSource {
    /// A literal value written into `providers.json` and copied into the
    /// provider process environment.
    Literal(String),
    /// A Managed Credential reference: the API-key material resolves from
    /// the Credential Store when the provider process spawns.
    Credential(CredentialRef),
}

impl fmt::Debug for InstanceEnvSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(_) => f.write_str("Literal(<redacted>)"),
            Self::Credential(reference) => {
                write!(f, "Credential({})", reference.as_str())
            }
        }
    }
}

impl<'de> serde::Deserialize<'de> for InstanceEnvSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Literal(String),
            Credential { credential: String },
        }
        match Raw::deserialize(deserializer)? {
            Raw::Literal(value) => Ok(Self::Literal(value)),
            Raw::Credential { credential } if credential.trim().is_empty() => Err(
                serde::de::Error::custom("env credential reference must not be empty"),
            ),
            Raw::Credential { credential } => Ok(Self::Credential(CredentialRef::new(credential))),
        }
    }
}

/// One named Provider Integration instance, decoded from `providers.json`
/// and checked against the registry at build time.
///
/// The descriptor is safe to print in full: `Debug` lists environment
/// variable names and credential references, never values.
#[derive(Clone, PartialEq, Eq)]
pub struct InstanceDescriptor {
    /// The instance's selector id (the `instances` map key).
    pub id: IntegrationId,
    /// The base Provider Integration this instance selects.
    pub integration: IntegrationId,
    /// The environment overlay applied to the provider process at spawn.
    pub env: BTreeMap<String, InstanceEnvSource>,
    /// The instance's named Managed Credential binding: the destination
    /// `auth set-key` targets and the credential an HTTP instance injects
    /// into the base Authentication Binding.
    pub credential: Option<CredentialRef>,
}

impl fmt::Debug for InstanceDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstanceDescriptor")
            .field("id", &self.id)
            .field("integration", &self.integration)
            .field("env", &self.env)
            .field("credential", &self.credential)
            .finish()
    }
}

impl InstanceDescriptor {
    /// Resolve the instance's environment overlay to concrete values.
    ///
    /// This is the only place Credential References turn into material, and
    /// it runs at provider-process spawn - never in list/show paths. A
    /// referenced credential must be an API key created for this instance
    /// or unbound; anything else is a [`CredentialStoreError`], which never
    /// carries material.
    pub fn resolve_env(
        &self,
        store: &CredentialStore,
    ) -> Result<BTreeMap<String, String>, CredentialStoreError> {
        let mut resolved = BTreeMap::new();
        for (name, source) in &self.env {
            let value = match source {
                InstanceEnvSource::Literal(value) => value.clone(),
                InstanceEnvSource::Credential(reference) => store
                    .api_key_material(reference, &self.id)?
                    .ok_or_else(|| CredentialStoreError::CredentialAbsent(reference.clone()))?,
            };
            resolved.insert(name.clone(), value);
        }
        Ok(resolved)
    }

    /// The Authentication Binding this instance applies to an HTTP base
    /// integration: the named `credential` replaces the binding's managed
    /// credential slot while preserving its delivery and profile. An
    /// `api-key-env` binding keeps its environment fallback by becoming
    /// `EnvOrStore`; `none` has no slot to rebind and is rejected.
    pub fn bound_auth(&self, auth: &AuthBinding) -> Result<AuthBinding, String> {
        let Some(credential) = &self.credential else {
            return Ok(auth.clone());
        };
        match auth {
            AuthBinding::None => Err(format!(
                "instance {} binds credential {credential} but integration {} declares auth 'none'",
                self.id, self.integration
            )),
            AuthBinding::ApiKey { source, delivery } => {
                let source = match source {
                    ApiKeySource::Store { .. } => ApiKeySource::Store {
                        credential: credential.clone(),
                    },
                    ApiKeySource::Env { var } | ApiKeySource::EnvOrStore { var, .. } => {
                        ApiKeySource::EnvOrStore {
                            var: var.clone(),
                            credential: credential.clone(),
                        }
                    }
                };
                Ok(AuthBinding::ApiKey {
                    source,
                    delivery: delivery.clone(),
                })
            }
            AuthBinding::OAuth { profile, .. } => Ok(AuthBinding::OAuth {
                credential: credential.clone(),
                profile: profile.clone(),
            }),
        }
    }

    /// Every Credential Reference this instance names: the bound
    /// credential plus each environment value sourced from the store.
    /// Used to enforce the same built-in reservation rule config
    /// integrations obey.
    pub(crate) fn credential_refs(&self) -> impl Iterator<Item = &CredentialRef> {
        self.credential
            .iter()
            .chain(self.env.values().filter_map(|source| match source {
                InstanceEnvSource::Credential(reference) => Some(reference),
                InstanceEnvSource::Literal(_) => None,
            }))
    }
}

/// The `instances` map value: every field but `integration` is optional.
#[derive(serde::Deserialize)]
struct ConfigInstance {
    pub integration: String,
    #[serde(default)]
    pub env: BTreeMap<String, InstanceEnvSource>,
    #[serde(default)]
    pub credential: Option<String>,
}

/// Validate one `instances` map entry into an [`InstanceDescriptor`].
/// Shape errors only - references to other registry entries are the
/// registry's job, the same split `build_descriptor` uses.
pub(super) fn build_instance(
    id: &str,
    raw: serde_json::Value,
) -> Result<InstanceDescriptor, ConfigError> {
    let invalid = |reason: String| ConfigError::InvalidInstance {
        id: id.to_owned(),
        reason,
    };
    if id.trim().is_empty() {
        return Err(invalid("instance id must not be empty".to_owned()));
    }
    let entry: ConfigInstance = serde_json::from_value(raw)
        .map_err(|error| invalid(super::config::describe_json_error(&error)))?;
    if entry.integration.trim().is_empty() {
        return Err(invalid("integration must not be empty".to_owned()));
    }
    for name in entry.env.keys() {
        if !crate::valid_env_var_name(name) {
            return Err(invalid(format!(
                "env name {name:?} is not a valid environment variable name"
            )));
        }
    }
    let credential = entry
        .credential
        .filter(|credential| !credential.trim().is_empty())
        .map(CredentialRef::new);
    Ok(InstanceDescriptor {
        id: IntegrationId::new(id),
        integration: IntegrationId::new(entry.integration),
        env: entry.env,
        credential,
    })
}

/// Replace the `instances` map inside `providers.json` atomically.
///
/// The file keeps its schema-version probe and every other top-level key;
/// only `instances` is mutated. A missing file starts from
/// `{"schema_version": 1}`. Writes use the Credential Store's lockfile and
/// atomic-replace helpers against a `providers.json.lock` sibling, so two
/// `aifuel instance` processes cannot tear the file.
pub fn edit_instances(
    path: &Path,
    mutate: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> Result<(), ConfigError>,
) -> Result<(), ConfigError> {
    let lock_path = path.with_extension("json.lock");
    let _guard = crate::credentials::lock::StoreLock::acquire(&lock_path).map_err(|error| {
        ConfigError::InvalidFile {
            detail: format!("could not lock providers config: {error}"),
        }
    })?;
    let mut file: serde_json::Value = match std::fs::read(path) {
        Ok(bytes) => {
            let probe: super::config::VersionProbe =
                serde_json::from_slice(&bytes).map_err(|error| ConfigError::InvalidFile {
                    detail: super::config::describe_json_error(&error),
                })?;
            if probe.schema_version != super::config::PROVIDERS_SCHEMA_VERSION {
                return Err(ConfigError::UnknownSchemaVersion {
                    found: probe.schema_version,
                });
            }
            serde_json::from_slice(&bytes).map_err(|error| ConfigError::InvalidFile {
                detail: super::config::describe_json_error(&error),
            })?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            serde_json::json!({ "schema_version": super::config::PROVIDERS_SCHEMA_VERSION })
        }
        Err(error) => return Err(ConfigError::Io(error)),
    };
    let object = file
        .as_object_mut()
        .ok_or_else(|| ConfigError::InvalidFile {
            detail: "providers config root is not a JSON object".to_owned(),
        })?;
    let instances = object
        .entry("instances")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let map = instances
        .as_object_mut()
        .ok_or_else(|| ConfigError::InvalidFile {
            detail: "providers config 'instances' is not a JSON object".to_owned(),
        })?;
    mutate(map)?;
    let bytes = serde_json::to_vec_pretty(&file)
        .map_err(|error| ConfigError::Io(std::io::Error::other(error)))?;
    crate::credentials::lock::atomic_replace(path, &bytes).map_err(|error| {
        ConfigError::InvalidFile {
            detail: format!("could not write providers config: {error}"),
        }
    })
}

#[cfg(test)]
mod tests;
