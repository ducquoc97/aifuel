//! Serializable descriptors returned by `integrations.list`.
//!
//! One [`IntegrationSummary`] describes a configured Provider Integration to
//! a Host Application. It names the credential's Authentication Binding kind
//! only; credential material and Credential References never appear here.

use crate::{AdapterCapabilities, IntegrationId, ProviderId};
use serde::Serialize;

/// The Authentication Binding kind an integration's execution configuration
/// applies, reported as an opaque category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationAuthKind {
    /// The provider CLI owns its credential (`ExecutionConfig::Cli`).
    ProviderCli,
    /// A Managed Credential bound by reference: an API key or a managed
    /// OAuth grant. The Credential Reference identity is not exposed.
    Managed,
    /// The endpoint requires no credential (`AuthBinding::None`).
    None,
}

/// The observed readiness of one integration's execution path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationStatus {
    /// Local evidence supports attempting a run.
    Ready,
    /// Authentication or credential evidence is absent; the user must set up
    /// or renew the credential before runs can succeed.
    NeedsAuth,
    /// Presence or credential evidence could not be established. The
    /// integration may still work; nothing about readiness is claimed.
    Degraded,
    /// The integration cannot execute: no serving adapter exists, or the
    /// native executable is absent.
    Unavailable,
}

impl IntegrationStatus {
    /// The stable serialized spelling for this status.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::NeedsAuth => "needs_auth",
            Self::Degraded => "degraded",
            Self::Unavailable => "unavailable",
        }
    }

    /// Parse the serialized spelling written by [`IntegrationStatus::as_str`].
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "ready" => Self::Ready,
            "needs_auth" => Self::NeedsAuth,
            "degraded" => Self::Degraded,
            "unavailable" => Self::Unavailable,
            _ => return None,
        })
    }
}

/// One entry of the `integrations.list` payload: the configured binding's
/// identity, auth binding kind, observed status, and the serving adapter's
/// honest capability declarations.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IntegrationSummary {
    /// The opaque Integration Identity.
    pub integration_id: IntegrationId,
    /// The upstream provider this integration binds to.
    pub provider: ProviderId,
    /// The user-facing display name.
    pub label: String,
    /// The Authentication Binding kind, as an opaque category.
    pub auth: IntegrationAuthKind,
    /// The observed readiness of the execution path.
    pub status: IntegrationStatus,
    /// The serving adapter's declarations; every flag false when no adapter
    /// can serve the integration.
    pub capabilities: AdapterCapabilities,
}
