//! Opaque identities carried by the agent runtime contract.
//!
//! Like [`IntegrationId`](crate::IntegrationId), these are never parsed for
//! semantics; each names one contract object across commands, events,
//! receipts, and snapshots.

use serde::{Deserialize, Serialize};
use std::fmt;

/// The monotonic sequence number the Session Event Log assigns to each
/// [`AgentEvent`](crate::AgentEvent) before delivery to any consumer.
///
/// Consumers persist their last seen `Seq` per session so resubscription
/// replays only newer events.
pub type Seq = u64;

/// The opaque identity of one Agent Session.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Construct an opaque session identity.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The opaque identity string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The opaque identity of one Agent Run within an Agent Session.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    /// Construct an opaque run identity.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The opaque identity string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The opaque identity of one pending Approval Request.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(String);

impl RequestId {
    /// Construct an opaque approval request identity.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The opaque identity string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The opaque identity of one Checkpoint: the hidden git ref recorded for a
/// workspace-mutating Agent Run.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CheckpointId(String);

impl CheckpointId {
    /// Construct an opaque checkpoint identity.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The opaque identity string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CheckpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The opaque identity of one submitted [`AgentCommand`](crate::AgentCommand),
/// echoed by its [`Receipt`](crate::Receipt) so retries stay idempotent.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommandId(String);

impl CommandId {
    /// Construct an opaque command identity.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The opaque identity string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
