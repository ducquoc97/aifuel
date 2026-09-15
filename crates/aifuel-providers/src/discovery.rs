use aifuel_core::{DiscoveryError, DiscoveryState};
use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Explicit local context for provider discovery.
///
/// Library callers provide the home directory themselves. The executable may
/// use [`Self::from_environment`] at its process boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryContext {
    home_dir: PathBuf,
}

impl DiscoveryContext {
    pub fn new(home_dir: impl Into<PathBuf>) -> Self {
        Self {
            home_dir: home_dir.into(),
        }
    }

    pub fn home_dir(&self) -> &Path {
        &self.home_dir
    }

    /// Resolve the platform home directory for the executable boundary.
    pub fn from_environment() -> Result<Self, DiscoveryContextError> {
        #[cfg(windows)]
        let home = env::var_os("USERPROFILE").or_else(|| {
            let drive = env::var_os("HOMEDRIVE")?;
            let path = env::var_os("HOMEPATH")?;
            Some(PathBuf::from(drive).join(path))
        });

        #[cfg(not(windows))]
        let home = env::var_os("HOME");

        home.map(Self::new)
            .ok_or(DiscoveryContextError::HomeDirectoryUnavailable)
    }

    /// Inspect one provider-owned relative source without reading its content.
    pub(crate) fn inspect_source(&self, relative: &str) -> Result<DiscoveryState, DiscoveryError> {
        match fs::metadata(self.home_dir.join(relative)) {
            Ok(_) => Ok(DiscoveryState::Present),
            Err(error) if is_absent(&error) => Ok(DiscoveryState::Absent),
            Err(_) => Err(DiscoveryError::SourceUnavailable),
        }
    }

    /// Inspect alternative provider-owned sources.
    pub(crate) fn inspect_any(&self, sources: &[&str]) -> Result<DiscoveryState, DiscoveryError> {
        let mut unavailable = false;
        for source in sources {
            match self.inspect_source(source) {
                Ok(DiscoveryState::Present) => return Ok(DiscoveryState::Present),
                Ok(DiscoveryState::Absent) => {}
                Err(DiscoveryError::SourceUnavailable) => unavailable = true,
            }
        }

        if unavailable {
            Err(DiscoveryError::SourceUnavailable)
        } else {
            Ok(DiscoveryState::Absent)
        }
    }
}

fn is_absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryContextError {
    HomeDirectoryUnavailable,
}

impl fmt::Display for DiscoveryContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HomeDirectoryUnavailable => f.write_str(
                "could not determine the home directory; set the platform home environment variable",
            ),
        }
    }
}

impl std::error::Error for DiscoveryContextError {}
