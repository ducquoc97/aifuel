use super::config::{GlobalSelectionConfig, SelectionSettings};
use aifuel_core::{AccessMode, IntegrationId};
use serde::Serialize;
use std::fmt;
use std::time::Duration;

/// Inputs supplied by a CLI, picker, or execution MCP request.
///
/// `explicit` contains only values supplied by that invocation. A profile is
/// selected by name and is resolved against the global configuration. The
/// prompt and repository path deliberately do not belong here: this contract
/// only resolves selection and policy metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectionInputs {
    pub explicit: SelectionSettings,
    pub profile: Option<String>,
    pub interactive: bool,
    /// `Some(None)` explicitly requests no overall deadline. This is distinct
    /// from an omitted deadline, which may inherit a profile/default value.
    pub deadline_override: Option<Option<u64>>,
}

impl SelectionInputs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_explicit(explicit: SelectionSettings) -> Self {
        Self {
            explicit,
            ..Self::default()
        }
    }

    pub fn named_profile(profile: impl Into<String>) -> Self {
        Self {
            profile: Some(profile.into()),
            ..Self::default()
        }
    }

    pub fn with_deadline_seconds(mut self, seconds: Option<u64>) -> Self {
        self.deadline_override = Some(seconds);
        self
    }
}

/// Metadata retained for a native Agent Session. It contains no prompt,
/// answer, tool argument, or diagnostic content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSession {
    pub session_id: String,
    /// The configured integration the session was created through.
    pub integration: IntegrationId,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub access: Option<AccessMode>,
    pub overall_deadline_seconds: Option<u64>,
    pub account_id: Option<String>,
}

impl StoredSession {
    pub fn new(session_id: impl Into<String>, integration: IntegrationId) -> Self {
        Self {
            session_id: session_id.into(),
            integration,
            model: None,
            effort: None,
            access: None,
            overall_deadline_seconds: None,
            account_id: None,
        }
    }

    fn settings(&self) -> SelectionSettings {
        SelectionSettings {
            integration: Some(self.integration.clone()),
            model: self.model.clone(),
            effort: self.effort.clone(),
            // Access and deadline are deliberately not inherited on resume.
            // They are re-resolved under the current policy for each run.
            access: None,
            overall_deadline_seconds: None,
        }
    }
}

/// The source that supplied one resolved setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionSource {
    Explicit,
    Profile(String),
    GlobalDefault,
    StoredSession,
    NativeDefault,
}

/// Sources for each field in a resolved selection. Keeping this explicit lets
/// callers explain precedence without inspecting the configuration themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SelectionSources {
    pub integration: SelectionSource,
    pub model: SelectionSource,
    pub effort: SelectionSource,
    pub access: SelectionSource,
    pub overall_deadline: SelectionSource,
}

/// Fully resolved settings used to build an Agent Run request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedSelection {
    pub integration: IntegrationId,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub access: AccessMode,
    pub overall_deadline: Option<Duration>,
    pub sources: SelectionSources,
    pub profile: Option<String>,
    pub resumed_session: Option<String>,
}

impl ResolvedSelection {
    pub const fn is_resume(&self) -> bool {
        self.resumed_session.is_some()
    }

    pub fn settings(&self) -> SelectionSettings {
        SelectionSettings {
            integration: Some(self.integration.clone()),
            model: self.model.clone(),
            effort: self.effort.clone(),
            access: Some(self.access),
            overall_deadline_seconds: self.overall_deadline.map(|duration| duration.as_secs()),
        }
    }
}

/// Selection validation and precedence failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionError {
    ProfileNotFound(String),
    MissingIntegration {
        interactive: bool,
    },
    IntegrationConflict {
        session_integration: IntegrationId,
        requested_integration: IntegrationId,
    },
    InvalidDeadline,
}

impl fmt::Display for SelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProfileNotFound(name) => write!(f, "selection profile {name:?} was not found"),
            Self::MissingIntegration { interactive } if *interactive => f.write_str(
                "integration selection is required; choose an integration in the terminal picker",
            ),
            Self::MissingIntegration { .. } => f.write_str(
                "integration selection is required; pass an integration id or profile explicitly",
            ),
            Self::IntegrationConflict {
                session_integration,
                requested_integration,
            } => write!(
                f,
                "resume integration conflict: session uses {session_integration}, requested {requested_integration}"
            ),
            Self::InvalidDeadline => f.write_str("overall deadline must be at least one second"),
        }
    }
}

impl std::error::Error for SelectionError {}

/// Resolves explicit settings, named profiles, and global defaults.
pub struct SelectionResolver<'a> {
    config: &'a GlobalSelectionConfig,
}

impl<'a> SelectionResolver<'a> {
    pub const fn new(config: &'a GlobalSelectionConfig) -> Self {
        Self { config }
    }

    pub fn resolve(
        &self,
        inputs: &SelectionInputs,
        session: Option<&StoredSession>,
    ) -> Result<ResolvedSelection, SelectionError> {
        let profile = inputs
            .profile
            .as_deref()
            .map(|name| {
                self.config
                    .profiles
                    .get(name)
                    .ok_or_else(|| SelectionError::ProfileNotFound(name.to_owned()))
            })
            .transpose()?;

        // A resume is intentionally based on its stored session. Global
        // defaults are not consulted: changing defaults must not silently
        // change an existing provider context or model/effort choice.
        let (base, base_is_session) = if let Some(session) = session {
            (session.settings(), true)
        } else {
            (self.config.defaults.clone(), false)
        };

        let empty_profile = SelectionSettings::default();
        let profile_settings = profile.unwrap_or(&empty_profile);
        if let Some(session) = session {
            // Integration identity is not a preference that may be hidden by a
            // higher-precedence field. Any explicitly supplied or selected
            // profile integration must agree with the native session
            // integration.
            for requested_integration in [
                inputs.explicit.integration.as_ref(),
                profile_settings.integration.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                if *requested_integration != session.integration {
                    return Err(SelectionError::IntegrationConflict {
                        session_integration: session.integration.clone(),
                        requested_integration: requested_integration.clone(),
                    });
                }
            }
        }
        let mut merged = inputs
            .explicit
            .merge_over(&profile_settings.merge_over(&base));
        if let Some(deadline_override) = inputs.deadline_override {
            merged.overall_deadline_seconds = deadline_override;
        }
        let integration = merged
            .integration
            .clone()
            .ok_or(SelectionError::MissingIntegration {
                interactive: inputs.interactive,
            })?;

        if let Some(session) = session
            && integration != session.integration
        {
            return Err(SelectionError::IntegrationConflict {
                session_integration: session.integration.clone(),
                requested_integration: integration,
            });
        }

        let overall_deadline = match merged.overall_deadline_seconds {
            Some(0) => return Err(SelectionError::InvalidDeadline),
            Some(seconds) => Some(Duration::from_secs(seconds)),
            None => None,
        };

        let access = merged.access.unwrap_or(AccessMode::ReadOnly);
        let sources = SelectionSources {
            integration: source_for(
                inputs.explicit.integration.is_some(),
                profile_settings.integration.is_some(),
                base.integration.is_some(),
                inputs.profile.as_deref(),
                base_is_session,
            ),
            model: source_for(
                inputs.explicit.model.is_some(),
                profile_settings.model.is_some(),
                base.model.is_some(),
                inputs.profile.as_deref(),
                base_is_session,
            ),
            effort: source_for(
                inputs.explicit.effort.is_some(),
                profile_settings.effort.is_some(),
                base.effort.is_some(),
                inputs.profile.as_deref(),
                base_is_session,
            ),
            access: source_for(
                inputs.explicit.access.is_some(),
                profile_settings.access.is_some(),
                // A session's access is intentionally absent from `base`.
                base.access.is_some(),
                inputs.profile.as_deref(),
                base_is_session,
            ),
            overall_deadline: source_for(
                inputs.explicit.overall_deadline_seconds.is_some()
                    || inputs.deadline_override.is_some(),
                profile_settings.overall_deadline_seconds.is_some(),
                base.overall_deadline_seconds.is_some(),
                inputs.profile.as_deref(),
                base_is_session,
            ),
        };

        Ok(ResolvedSelection {
            integration,
            model: merged.model,
            effort: merged.effort,
            access,
            overall_deadline,
            sources,
            profile: inputs.profile.clone(),
            resumed_session: session.map(|value| value.session_id.clone()),
        })
    }
}

impl GlobalSelectionConfig {
    pub fn resolver(&self) -> SelectionResolver<'_> {
        SelectionResolver::new(self)
    }

    pub fn resolve(
        &self,
        inputs: &SelectionInputs,
        session: Option<&StoredSession>,
    ) -> Result<ResolvedSelection, SelectionError> {
        self.resolver().resolve(inputs, session)
    }
}

fn source_for(
    explicit: bool,
    profile: bool,
    fallback_value: bool,
    profile_name: Option<&str>,
    session: bool,
) -> SelectionSource {
    if explicit {
        SelectionSource::Explicit
    } else if profile {
        SelectionSource::Profile(
            profile_name
                .expect("profile value cannot exist without a selected profile")
                .to_owned(),
        )
    } else if session {
        if fallback_value {
            SelectionSource::StoredSession
        } else {
            SelectionSource::NativeDefault
        }
    } else if fallback_value {
        SelectionSource::GlobalDefault
    } else {
        SelectionSource::NativeDefault
    }
}
