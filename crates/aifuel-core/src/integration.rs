//! The Provider Integration model: configured bindings of a provider identity
//! to one execution configuration and one optional monitoring configuration.
//!
//! These types describe configuration only. No secret material appears here;
//! integrations bind credentials by [`CredentialRef`] identity so secrets never
//! enter config, run records, or reports.

use crate::ProviderKey;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// The opaque identity of an upstream provider service.
///
/// Built-in ids equal the pinned catalog ids exposed by
/// [`ProviderKey::as_str`]. A `ProviderId` is never parsed for semantics.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(String);

impl ProviderId {
    /// Construct an opaque provider identity.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The opaque identity string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<ProviderKey> for ProviderId {
    fn from(key: ProviderKey) -> Self {
        Self(key.as_str().to_owned())
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The opaque identity of one configured Provider Integration.
///
/// `aifuel run` selects an `IntegrationId`. It is never parsed; there is no
/// `provider:mode` splitting for routing. Built-in integration ids equal the
/// catalog provider ids so existing stored selections resolve without
/// aliasing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IntegrationId(String);

impl IntegrationId {
    /// Construct an opaque integration identity.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The opaque identity string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<ProviderKey> for IntegrationId {
    fn from(key: ProviderKey) -> Self {
        Self(key.as_str().to_owned())
    }
}

impl fmt::Display for IntegrationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The opaque identity of one Managed Credential in the Credential Store.
///
/// Integrations bind credentials by reference so secrets never appear in
/// config, run records, or reports. A `CredentialRef` names the credential
/// slot, not the credential material.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CredentialRef(String);

impl CredentialRef {
    /// Construct an opaque credential reference.
    pub fn new(reference: impl Into<String>) -> Self {
        Self(reference.into())
    }

    /// The opaque reference string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CredentialRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The identity of a compiled OAuth flow specification.
///
/// OAuth profiles are a closed compile-time set, so config can select a
/// profile but cannot invent flow behavior.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OAuthProfileId(String);

impl OAuthProfileId {
    /// Construct an opaque OAuth profile identity.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The opaque identity string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OAuthProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The identity of a compiled CLI adapter in the provider registry.
///
/// CLI adapters are a closed compile-time set, so config can select an
/// adapter but cannot invent one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CliAdapterId(String);

impl CliAdapterId {
    /// Construct an opaque CLI adapter identity.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The opaque identity string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CliAdapterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The identity of a compiled monitoring collector.
///
/// Collectors are a closed compile-time set, so config can select a collector
/// but cannot invent one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CollectorId(String);

impl CollectorId {
    /// Construct an opaque collector identity.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The opaque identity string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CollectorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The HTTP request and response protocol an endpoint speaks. A Wire Api is
/// protocol evidence, not a provider identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireApi {
    OpenAiChat,
    OpenAiResponses,
    AnthropicMessages,
    /// The claude.ai browser conversation surface. No compiled engine
    /// serves it: the `*:web` integrations that declare it exist to bind a
    /// session credential and a Monitoring Collection Contract, not to
    /// execute prompts.
    ClaudeWeb,
    /// A structured decision surface - TypeSafe AI's System One and
    /// compatible endpoints that answer typed `state` + `questions`
    /// payloads with calibrated choices rather than chat text. No Agent
    /// Run adapter serves it; it exists so integrations can declare the
    /// endpoint honestly for the gateway's `/v1/decisions` forwarding.
    Decisions,
}

/// The connection configuration for one HTTP endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointConfig {
    /// The endpoint base URL.
    pub base_url: String,
    /// Extra headers applied to requests. A managed authentication binding is
    /// applied last, so these headers cannot override managed auth.
    #[serde(default)]
    pub extra_headers: BTreeMap<String, String>,
    /// The request timeout in seconds.
    pub request_timeout_seconds: Option<u64>,
}

/// Where an API-key Authentication Binding sources its material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeySource {
    /// A declared environment variable. The variable is named in config; its
    /// value is never stored or reported.
    Env { var: String },
    /// A Managed Credential in the Credential Store, named by reference.
    Store { credential: CredentialRef },
    /// A Managed Credential when one is stored, otherwise a declared
    /// environment variable. `aifuel auth set-key` binds this integration by
    /// writing the named reference; `aifuel auth remove` on it warns when the
    /// environment variable remains set, because that credential stays
    /// active.
    EnvOrStore {
        var: String,
        credential: CredentialRef,
    },
}

/// How an API key reaches the wire. Header conventions differ across
/// OpenAI-compatible services, so delivery is an explicit choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyDelivery {
    /// The `Authorization: Bearer` header.
    Bearer,
    /// A named header such as `x-api-key`.
    Header { name: String },
    /// The `Cookie` header for browser-session credentials. A bare session
    /// value is sent as `name=value`; material that already carries cookie
    /// pairs - a pasted `Cookie:` header line or `name=value; ...` - is
    /// sent unchanged so a copied browser header works as pasted.
    Cookie { name: String },
}

/// The `Cookie` header value a [`KeyDelivery::Cookie`] delivery produces
/// for `material`. The normalization mirrors OmniRoute's session-cookie
/// contract: a bare session value becomes `name=value`, while material
/// already carrying cookie pairs passes through unchanged.
pub fn cookie_header_value(name: &str, material: &str) -> String {
    let material = material.trim();
    // A pasted `Cookie: ...` header line keeps only its value.
    let material = material
        .get(..7)
        .filter(|prefix| prefix.eq_ignore_ascii_case("cookie:"))
        .map(|_| material[7..].trim_start())
        .unwrap_or(material);
    if material.contains('=') {
        material.to_owned()
    } else {
        format!("{name}={material}")
    }
}

/// The association between an execution configuration and the credential it
/// applies to requests: none, an API key, or a managed OAuth credential. A
/// configured endpoint does not imply one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthBinding {
    /// The endpoint requires no credential.
    None,
    /// An API key sourced from the environment or the Credential Store.
    ApiKey {
        source: ApiKeySource,
        delivery: KeyDelivery,
    },
    /// A managed OAuth grant applied through a compiled flow profile.
    #[serde(rename = "oauth")]
    OAuth {
        credential: CredentialRef,
        profile: OAuthProfileId,
    },
}

impl AuthBinding {
    /// The Managed Credential Reference this binding can resolve, when it
    /// binds one. Env-only sources and `None` yield `None`.
    pub fn credential_ref(&self) -> Option<&CredentialRef> {
        match self {
            Self::ApiKey { source, .. } => match source {
                ApiKeySource::Store { credential }
                | ApiKeySource::EnvOrStore { credential, .. } => Some(credential),
                ApiKeySource::Env { .. } => None,
            },
            Self::OAuth { credential, .. } => Some(credential),
            Self::None => None,
        }
    }
}

/// The execution path of a Provider Integration. `Cli` carries no endpoint or
/// credential fields because the provider CLI owns its credential; `Http`
/// always carries an explicit [`AuthBinding`], including an explicit
/// [`AuthBinding::None`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionConfig {
    /// A compiled CLI adapter; the provider CLI owns the credential.
    Cli { adapter: CliAdapterId },
    /// A direct HTTP endpoint speaking one Wire Api.
    Http {
        endpoint: EndpointConfig,
        protocol: WireApi,
        auth: AuthBinding,
    },
}

/// The optional per-integration Monitoring Collection Contract. The inference
/// Wire Api says nothing about quota endpoints, so monitoring binds its own
/// collector and optional credential separately from execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitoringConfig {
    /// The compiled collector that produces the observations.
    pub collector: CollectorId,
    /// The monitoring credential binding, independent of the execution one.
    pub credential: Option<CredentialRef>,
    /// The collector endpoint, when it differs from the execution endpoint.
    pub endpoint: Option<EndpointConfig>,
}

/// A configured Provider Integration: one provider identity bound to one
/// execution configuration and one optional monitoring configuration. It is
/// the unit a user selects for an Agent Run or a monitoring collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Integration {
    /// The opaque Integration Identity of this configured binding.
    pub id: IntegrationId,
    /// The upstream provider service this integration binds to.
    pub provider: ProviderId,
    /// The user-facing display name.
    pub name: String,
    /// The execution configuration selected for runs.
    pub execution: ExecutionConfig,
    /// The optional monitoring collection contract.
    pub monitoring: Option<MonitoringConfig>,
}

/// How one selector resolves against a set of `(IntegrationId, ProviderId)`
/// candidates.
///
/// This is the single implementation of the selection rule both the
/// Integration Registry and run-time adapter resolution share: an exact
/// Integration Identity wins; otherwise a bare Provider Identity resolves
/// only when exactly one integration uses it, and several candidates are an
/// ambiguity, never a silent pick between subscription and billed execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectorMatch {
    /// The selector is an Integration Identity.
    Exact(IntegrationId),
    /// The selector is a bare Provider Identity used by one integration.
    Unique(IntegrationId),
    /// The selector is a bare Provider Identity used by several integrations.
    Ambiguous {
        provider: ProviderId,
        candidates: Vec<IntegrationId>,
    },
    /// Nothing matches.
    Unknown,
}

/// Match `selector` against `(IntegrationId, ProviderId)` pairs under the
/// shared selection rule.
pub fn match_selector<'a>(
    selector: &str,
    integrations: impl Iterator<Item = (&'a IntegrationId, &'a ProviderId)>,
) -> SelectorMatch {
    let mut provider_candidates = Vec::new();
    for (id, provider) in integrations {
        if id.as_str() == selector {
            return SelectorMatch::Exact(id.clone());
        }
        if provider.as_str() == selector {
            provider_candidates.push(id.clone());
        }
    }
    match provider_candidates.as_slice() {
        [only] => SelectorMatch::Unique(only.clone()),
        [] => SelectorMatch::Unknown,
        _ => SelectorMatch::Ambiguous {
            provider: ProviderId::new(selector),
            candidates: provider_candidates,
        },
    }
}
