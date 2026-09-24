use fs4::{FileExt, TryLockError};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// A cross-process lock for one canonical workspace-write run.
pub(crate) struct WorkspaceWriteLock {
    file: File,
    #[allow(dead_code)]
    path: PathBuf,
}

impl WorkspaceWriteLock {
    pub(crate) fn acquire(workspace: &Path) -> Result<Self, WorkspaceLockError> {
        let mut digest = Sha256::new();
        digest.update(workspace.as_os_str().to_string_lossy().as_bytes());
        let name = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
            + ".lock";
        let directory = std::env::temp_dir().join("aifuel-workspace-locks");
        fs::create_dir_all(&directory).map_err(WorkspaceLockError::Io)?;
        let path = directory.join(name);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(WorkspaceLockError::Io)?;
        match FileExt::try_lock(&file) {
            Ok(()) => Ok(Self { file, path }),
            Err(TryLockError::WouldBlock) => Err(WorkspaceLockError::Busy),
            Err(TryLockError::Error(error)) => Err(WorkspaceLockError::Io(error)),
        }
    }
}

impl Drop for WorkspaceWriteLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[derive(Debug)]
pub(crate) enum WorkspaceLockError {
    Busy,
    Io(io::Error),
}

impl std::fmt::Display for WorkspaceLockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => {
                formatter.write_str("workspace already has an active workspace-write Agent Run")
            }
            Self::Io(error) => write!(formatter, "workspace lock failed: {error}"),
        }
    }
}

impl std::error::Error for WorkspaceLockError {}
