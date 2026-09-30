//! Model selection and descriptors.
//!
//! Selection is by [`IntegrationId`], never a `provider:mode` compound parsed
//! for routing. `models.list` merges the Advertised Model catalog with
//! Account Entitlement, Execution Availability, and Quota Pool observations
//! for that integration's account.

use crate::{CapabilityState, IntegrationId, ProviderId};
use serde::{Deserialize, Serialize};

/// The model a host selects for a session or run: one Integration Identity,
/// a model id, and an optional effort.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelection {
    pub integration_id: IntegrationId,
    pub model: String,
    /// Requested model-specific effort. `None` means the provider's default;
    /// it does not claim a concrete effective value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
}

/// A model-specific effort level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    High,
    Max,
}

impl Effort {
    /// The stable serialized spelling for this effort.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
        }
    }

    /// Parse the serialized spelling written by [`Effort::as_str`].
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "max" => Self::Max,
            _ => return None,
        })
    }
}

/// One model a Provider Integration can select, merging Advertised Model,
/// Account Entitlement, Execution Availability, and Quota Pool evidence.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelDescriptor {
    /// The upstream provider this model belongs to.
    pub provider: ProviderId,
    pub model: String,
    /// The user-facing display label.
    pub label: String,
    /// Selectable effort levels. Empty means the model has fixed effort.
    pub efforts: Vec<Effort>,
    /// Evidence identifies this model as advertised by the provider.
    /// Advertisement alone does not establish entitlement or availability.
    pub advertised: bool,
    /// The observed Account Entitlement for this integration's account;
    /// `unknown` stays unknown independently of advertisement and
    /// availability.
    pub entitled: CapabilityState,
    /// Readiness to attempt this model in the current local context.
    pub availability: ExecutionAvailability,
    /// Quota Pool headroom when the integration's monitoring reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<QuotaSummary>,
}

/// An observation of readiness to attempt a selected model through an Agent
/// Integration. Readiness does not guarantee the provider accepts the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionAvailability {
    Ready,
    NeedsAuth,
    Unsupported,
    Unknown,
}

impl ExecutionAvailability {
    /// The stable serialized spelling for this availability.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::NeedsAuth => "needs_auth",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }

    /// Parse the serialized spelling written by [`ExecutionAvailability::as_str`].
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "ready" => Self::Ready,
            "needs_auth" => Self::NeedsAuth,
            "unsupported" => Self::Unsupported,
            "unknown" => Self::Unknown,
            _ => return None,
        })
    }
}

/// A compact Quota Pool headroom observation attached to a model descriptor
/// or emitted by `quota.observed`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct QuotaSummary {
    /// Remaining allowance percent, when the provider reports one. Missing
    /// values stay unknown and are never reported as zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_pct: Option<f64>,
    /// Seconds since the Unix epoch when the pool resets, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<f64>,
    /// The pool is observed exhausted.
    pub depleted: bool,
}
