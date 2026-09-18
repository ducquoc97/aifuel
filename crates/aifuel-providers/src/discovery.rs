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
        let home = env::var_os("AIFUEL_HOME")
            .or_else(|| env::var_os("USERPROFILE"))
            .or_else(|| {
                let drive = env::var_os("HOMEDRIVE")?;
                let path = env::var_os("HOMEPATH")?;
                Some(PathBuf::from(drive).join(path).into())
            });

        #[cfg(not(windows))]
        let home = env::var_os("AIFUEL_HOME").or_else(|| env::var_os("HOME"));

        home.map(Self::new)
            .ok_or(DiscoveryContextError::HomeDirectoryUnavailable)
    }

    /// Inspect one provider-owned relative source without reading its content.
    pub(crate) fn inspect_source(
        &self,
        relative: &str,
        expected: SourceKind,
    ) -> Result<DiscoveryState, DiscoveryError> {
        match fs::metadata(self.home_dir.join(relative)) {
            Ok(metadata) if expected.matches(&metadata) => Ok(DiscoveryState::Present),
            Ok(_) => Err(DiscoveryError::UnexpectedSourceType),
            Err(error) if is_absent(&error) => Ok(DiscoveryState::Absent),
            Err(_) => Err(DiscoveryError::SourceUnavailable),
        }
    }

    /// Inspect alternative provider-owned sources.
    pub(crate) fn inspect_any<'a, I>(&self, sources: I) -> Result<DiscoveryState, DiscoveryError>
    where
        I: IntoIterator<Item = (&'a str, SourceKind)>,
    {
        let mut failure = None;
        for (source, expected) in sources {
            match self.inspect_source(source, expected) {
                Ok(DiscoveryState::Present) => return Ok(DiscoveryState::Present),
                Ok(DiscoveryState::Absent) => {}
                Err(error) => failure = Some(failure.unwrap_or(error)),
            }
        }

        match failure {
            Some(error) => Err(error),
            None => Ok(DiscoveryState::Absent),
        }
    }
}

fn is_absent(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKind {
    File,
    Directory,
}

impl SourceKind {
    fn matches(self, metadata: &fs::Metadata) -> bool {
        match self {
            Self::File => metadata.is_file(),
            Self::Directory => metadata.is_dir(),
        }
    }
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
