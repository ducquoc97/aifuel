//! The declarative `providers.json` config: user-defined Provider
//! Integrations alongside the compiled built-ins.
//!
//! The file lives at `providers.json` inside the AI Fuel config directory
//! (`user_config_dir(home)/aifuel/`, alongside `credentials.json` and
//! `execution.json`). JSON matches the repo's other machine-managed config
//! files. The caller supplies the path; this module never derives paths from
//! environment variables.
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "integrations": [
//!     {
//!       "id": "work-openai",
//!       "provider_id": "openai",
//!       "name": "Work OpenAI",
//!       "endpoint": {
//!         "base_url": "https://api.openai.com/v1",
//!         "headers": { "x-custom": "value" },
//!         "request_timeout_seconds": 30
//!       },
//!       "wire_api": "openai-chat",
//!       "auth": { "kind": "api-key-env", "var": "OPENAI_API_KEY" },
//!       "monitoring": { "collector": "openrouter-key" }
//!     },
//!     {
//!       "id": "work-claude",
//!       "provider_id": "anthropic",
//!       "cli": { "adapter": "claude" }
//!     }
//!   ]
//! }
//! ```
//!
//! An entry picks exactly one execution arm: `cli` selects a compiled CLI
//! adapter by id, or `endpoint` plus `wire_api` plus `auth` configure an
//! HTTP integration. `name`, `endpoint.headers`,
//! `endpoint.request_timeout_seconds`, and `monitoring` are optional;
//! `name` defaults to the id. `wire_api` is `openai-chat` - the other
//! compiled protocol names are recognized but rejected because no execution
//! engine serves them in this build. `auth.kind` is one of:
//!
//! - `"none"` - no credential; the field list must end there.
//! - `"api-key-env"` - resolve the key from the named `var` at run time. The
//!   key itself is never stored in this file.
//! - `"api-key-ref"` - `credential` names a Managed Credential in the
//!   Credential Store. Both api-key kinds accept optional `delivery`:
//!   `"bearer"` (default) or `{"header": {"name": "x-api-key"}}`.
//! - `"api-key-env-or-store"` - a managed `credential` when one is stored,
//!   otherwise the named `var`. This is the binding `aifuel auth set-key`
//!   targets for a configured integration.
//! - `"oauth-ref"` - `credential` plus `profile` naming a compiled OAuth
//!   flow.
//!
//! `monitoring` attaches a Monitoring Collection Contract: `collector`
//! names a compiled collector, `credential` optionally binds a dedicated
//! Managed Credential (otherwise the execution binding is reused), and
//! `endpoint` overrides the collector's default URL for development.
//!
//! Validation happens at this load boundary only: ids must be non-empty,
//! `base_url` must parse as an `http`/`https` URL, `wire_api` must name a
//! compiled and serveable protocol, `cli.adapter` must name a compiled CLI
//! adapter, `monitoring.collector` must name a compiled collector, and
//! `auth` fields must be consistent with `kind`. A malformed file is an
//! explicit error, never a silently empty registry.
//!
//! Trust boundary: config `auth` may use env vars or Credential References
//! the user owns. References a built-in integration already holds are
//! reserved - [`crate::integrations::IntegrationRegistry::build`] rejects a
//! config entry naming one, so a config endpoint cannot smuggle a built-in's
//! Managed Credential to a different host. And an `endpoint.headers` key
//! colliding with the header the managed auth binding applies is rejected,
//! so custom headers cannot override managed auth (the wire layer also
//! applies managed auth last, as a second line of defense).

use super::registry::IntegrationDescriptor;
use aifuel_core::KeyDelivery;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

/// The only `providers.json` schema version this build reads.
pub const PROVIDERS_SCHEMA_VERSION: u32 = 1;

/// The config file name inside the AI Fuel config directory.
pub const PROVIDERS_FILE_NAME: &str = "providers.json";

/// A decoded `providers.json`: one result per `integrations` entry and one
/// per `instances` entry so a single malformed entry reports its own error
/// while file-level failures stay hard errors.
#[derive(Debug)]
pub struct ProvidersConfig {
    entries: Vec<Result<IntegrationDescriptor, ConfigError>>,
    instances: Vec<Result<super::instances::InstanceDescriptor, ConfigError>>,
}

impl ProvidersConfig {
    /// Load and validate `providers.json` from `path`.
    ///
    /// An absent file is an empty config, matching `SelectionStore`
    /// conventions. A malformed file or an unknown `schema_version` is an
    /// explicit [`ConfigError`], never a silently empty registry.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    entries: Vec::new(),
                    instances: Vec::new(),
                });
            }
            Err(error) => return Err(ConfigError::Io(error)),
        };
        let probe: VersionProbe =
            serde_json::from_slice(&bytes).map_err(|error| ConfigError::InvalidFile {
                detail: describe_json_error(&error),
            })?;
        if probe.schema_version != PROVIDERS_SCHEMA_VERSION {
            return Err(ConfigError::UnknownSchemaVersion {
                found: probe.schema_version,
            });
        }
        let file: ProvidersFile =
            serde_json::from_slice(&bytes).map_err(|error| ConfigError::InvalidFile {
                detail: describe_json_error(&error),
            })?;
        let entries = file
            .integrations
            .into_iter()
            .enumerate()
            .map(|(index, raw)| validate::build_descriptor(index, raw))
            .collect();
        let instances = file
            .instances
            .into_iter()
            .map(|(id, raw)| super::instances::build_instance(&id, raw))
            .collect();
        Ok(Self { entries, instances })
    }

    /// One result per `integrations` entry, in file order.
    pub fn entries(&self) -> &[Result<IntegrationDescriptor, ConfigError>] {
        &self.entries
    }

    /// One result per `instances` entry, in file order.
    pub fn instances(&self) -> &[Result<super::instances::InstanceDescriptor, ConfigError>] {
        &self.instances
    }

    /// Consume both entry lists for [`super::IntegrationRegistry::build`].
    pub fn into_parts(self) -> ProvidersConfigParts {
        ProvidersConfigParts {
            integrations: self.entries,
            instances: self.instances,
        }
    }
}

/// The decoded `providers.json` entry lists `ProvidersConfig` hands to
/// [`super::IntegrationRegistry::build`]: per-entry results so one malformed
/// entry fails alone.
pub struct ProvidersConfigParts {
    /// One result per `integrations` entry, in file order.
    pub integrations: Vec<Result<IntegrationDescriptor, ConfigError>>,
    /// One result per `instances` entry, in file order.
    pub instances: Vec<Result<super::instances::InstanceDescriptor, ConfigError>>,
}

/// A minimal decode of the version field, checked before the full parse so a
/// newer file reports its schema version rather than a misleading shape
/// error - the same probe pattern `credentials.json` uses.
#[derive(Deserialize)]
pub(crate) struct VersionProbe {
    pub(crate) schema_version: u32,
}

/// The file envelope. Entries stay as raw values so one malformed entry is
/// an entry error, not a whole-file parse failure. `schema_version` is
/// already enforced by `VersionProbe` before this decode.
#[derive(Deserialize)]
struct ProvidersFile {
    #[serde(default)]
    integrations: Vec<serde_json::Value>,
    /// The named Provider Integration instances: the map key is the
    /// instance's selector id and the value names its base integration plus
    /// the environment overlay and credential binding it applies.
    #[serde(default)]
    instances: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct ConfigIntegration {
    id: String,
    provider_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    cli: Option<ConfigCli>,
    #[serde(default)]
    endpoint: Option<ConfigEndpoint>,
    #[serde(default)]
    wire_api: Option<String>,
    #[serde(default)]
    auth: Option<ConfigAuth>,
    #[serde(default)]
    monitoring: Option<ConfigMonitoring>,
}

#[derive(Deserialize)]
struct ConfigCli {
    adapter: String,
}

#[derive(Deserialize)]
struct ConfigEndpoint {
    base_url: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    request_timeout_seconds: Option<u64>,
}

#[derive(Deserialize)]
struct ConfigMonitoring {
    collector: String,
    #[serde(default)]
    credential: Option<String>,
    #[serde(default)]
    endpoint: Option<ConfigEndpoint>,
}

/// The auth object is decoded flat and validated by `kind`, because which
/// fields are required - or forbidden - depends on it.
#[derive(Deserialize)]
struct ConfigAuth {
    kind: String,
    #[serde(default)]
    var: Option<String>,
    #[serde(default)]
    credential: Option<String>,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    delivery: Option<KeyDelivery>,
}

/// A serde error plus its position, so a hand-edited file can be repaired.
/// This file never contains credential material by schema design (auth binds
/// env vars or Credential References), so error text is safe to surface.
pub(crate) fn describe_json_error(error: &serde_json::Error) -> String {
    format!("{error} (line {}, column {})", error.line(), error.column())
}

/// The failures `providers.json` loading and validation can report.
#[derive(Debug)]
pub enum ConfigError {
    /// The file could not be read (a missing file is not an error).
    Io(io::Error),
    /// The file is not valid JSON or does not match the config envelope.
    /// It is never treated as empty.
    InvalidFile { detail: String },
    /// The file declares a schema version this build does not know.
    UnknownSchemaVersion { found: u32 },
    /// One `integrations` entry failed validation.
    InvalidIntegration {
        index: usize,
        id: Option<String>,
        reason: String,
    },
    /// One `instances` entry failed validation.
    InvalidInstance { id: String, reason: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "providers config I/O failed: {error}"),
            Self::InvalidFile { detail } => {
                write!(f, "providers config is malformed: {detail}")
            }
            Self::UnknownSchemaVersion { found } => write!(
                f,
                "providers config schema version {found} is unknown to this build (supports {PROVIDERS_SCHEMA_VERSION})"
            ),
            Self::InvalidIntegration { index, id, reason } => match id {
                Some(id) => write!(
                    f,
                    "providers config integrations[{index}] ('{id}'): {reason}"
                ),
                None => write!(f, "providers config integrations[{index}]: {reason}"),
            },
            Self::InvalidInstance { id, reason } => {
                write!(f, "providers config instance '{id}': {reason}")
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ConfigError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

mod validate;

#[cfg(test)]
mod tests;
