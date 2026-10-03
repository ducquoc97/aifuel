//! Provider Integrations: the runtime model that binds one provider identity
//! to one execution configuration (spec `docs/specs/provider-integrations.md`).
//!
//! - [`registry`]: the owned [`IntegrationRegistry`] built at startup from
//!   built-in descriptors plus validated `providers.json` entries, with
//!   deterministic ordering, reserved built-in ids and credentials, and
//!   exact-id / unambiguous-provider selection.
//! - [`config`]: the declarative `providers.json` schema (version 1), with
//!   all validation at the load boundary and the managed-auth trust boundary
//!   enforced.
//! - [`evidence`]: the local, side-effect-free discovery evidence sources
//!   (`EnvVar`, `ManagedEntry`, `ConfiguredEndpoint`) added to the existing
//!   file and directory markers.
//! - [`instances`]: the named Provider Integration instances (`instances`
//!   map in `providers.json`) - selection overlays that carry per-instance
//!   environment and credential bindings without changing the base
//!   integration's identity or capabilities.
//! - [`chains`]: the named fallback chains (`chains` map in
//!   `providers.json`) - user-configured ordered fallback lists over
//!   registered integrations, `--chain NAME`'s target.
//! - [`builtin`]: the compiled [`IntegrationDescriptor`] set - six CLI
//!   integrations, the runtime-adapter integrations, the local OpenAI-
//!   compatible endpoints, and the API-key provider catalog.

mod builtin;
mod chains;
mod config;
mod evidence;
mod instances;
mod registry;

pub use builtin::builtin_integrations;
pub use chains::{ChainDescriptor, ChainStep, ChainStrategy};
pub use config::{
    ConfigError, PROVIDERS_FILE_NAME, PROVIDERS_SCHEMA_VERSION, ProvidersConfig,
    ProvidersConfigParts,
};
pub use evidence::{EvidenceContext, EvidenceSource, inspect_any};
pub use instances::{InstanceDescriptor, InstanceEnvSource, edit_instances};
pub use registry::{
    IntegrationDescriptor, IntegrationOrigin, IntegrationRegistry, RegistryError, ResolveError,
};
