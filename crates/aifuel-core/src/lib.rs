//! Stable domain values shared by AI Fuel applications and provider adapters.

use serde::Serialize;
use std::fmt;

/// The schema version for the initial Rust discovery output.
pub const DISCOVERY_SCHEMA_VERSION: u32 = 1;

/// A provider represented in the built-in Catalog Provider catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKey {
    Claude,
    Codex,
    Copilot,
    Gemini,
    Antigravity,
}

impl ProviderKey {
    /// The keys in the stable catalog order used by the current application.
    pub const ALL: [Self; 5] = [
        Self::Claude,
        Self::Codex,
        Self::Copilot,
        Self::Gemini,
        Self::Antigravity,
    ];

    /// The stable serialized key for this provider.
    pub const fn as_str(self) -> &'static str {
        self.metadata().key
    }

    /// The user-facing provider name.
    pub const fn display_name(self) -> &'static str {
        self.metadata().name
    }

    const fn metadata(self) -> ProviderMetadata {
        match self {
            Self::Claude => ProviderMetadata {
                key: "claude",
                name: "Claude Code",
            },
            Self::Codex => ProviderMetadata {
                key: "codex",
                name: "Codex CLI",
            },
            Self::Copilot => ProviderMetadata {
                key: "copilot",
                name: "GitHub Copilot",
            },
            Self::Gemini => ProviderMetadata {
                key: "gemini",
                name: "Gemini CLI",
            },
            Self::Antigravity => ProviderMetadata {
                key: "antigravity",
                name: "Antigravity CLI",
            },
        }
    }
}

struct ProviderMetadata {
    key: &'static str,
    name: &'static str,
}

impl fmt::Display for ProviderKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The stable identity and display name of a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ProviderDescriptor {
    pub key: ProviderKey,
    pub name: &'static str,
}

impl ProviderDescriptor {
    /// Construct the canonical descriptor for a provider key.
    pub const fn for_key(key: ProviderKey) -> Self {
        Self {
            key,
            name: key.display_name(),
        }
    }
}

/// The result of a local Provider Discovery check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryState {
    /// The provider-specific credential source exists locally.
    Present,
    /// The provider-specific credential source does not exist locally.
    Absent,
}

/// A safe, application-level reason that Provider Discovery could not finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryError {
    /// The provider-specific source could not be inspected.
    SourceUnavailable,
    /// A source exists but is not the expected filesystem shape.
    UnexpectedSourceType,
}

impl DiscoveryError {
    /// A user-facing explanation that contains no path or credential data.
    pub const fn detail(self) -> &'static str {
        match self {
            Self::SourceUnavailable => "provider credential source could not be inspected",
            Self::UnexpectedSourceType => {
                "provider credential source has an unexpected filesystem type"
            }
        }
    }
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.detail())
    }
}

impl std::error::Error for DiscoveryError {}

/// A discovery check that could not determine whether a provider is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DiscoveryFailure {
    pub provider: ProviderDescriptor,
    pub detail: &'static str,
}

impl DiscoveryFailure {
    /// Convert an internal discovery error into the safe public result form.
    pub const fn from_error(provider: ProviderDescriptor, error: DiscoveryError) -> Self {
        Self {
            provider,
            detail: error.detail(),
        }
    }
}

/// The provider set and independent discovery diagnostics for one collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscoveryReport {
    /// Only providers whose local source was present and initialized.
    pub providers: Vec<ProviderDescriptor>,
    /// Providers whose source could not be inspected. They are excluded above.
    pub discovery_errors: Vec<DiscoveryFailure>,
}

impl DiscoveryReport {
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
            discovery_errors: Vec::new(),
        }
    }
}

impl Default for DiscoveryReport {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_keys_have_stable_identity_and_names() {
        assert_eq!(ProviderKey::ALL.len(), 5);
        assert_eq!(ProviderKey::Claude.as_str(), "claude");
        assert_eq!(ProviderKey::Copilot.display_name(), "GitHub Copilot");
        assert_eq!(
            ProviderDescriptor::for_key(ProviderKey::Gemini),
            ProviderDescriptor {
                key: ProviderKey::Gemini,
                name: "Gemini CLI",
            }
        );
    }

    #[test]
    fn discovery_failure_has_safe_detail() {
        let failure = DiscoveryFailure::from_error(
            ProviderDescriptor::for_key(ProviderKey::Claude),
            DiscoveryError::SourceUnavailable,
        );

        assert_eq!(failure.provider.key, ProviderKey::Claude);
        assert_eq!(
            failure.detail,
            "provider credential source could not be inspected"
        );
    }
}
