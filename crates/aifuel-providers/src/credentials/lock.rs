//! Lockfile and atomic-write plumbing for the Credential Store.

use super::CredentialStoreError;
use fs4::{FileExt, TryLockError};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// How long a mutation waits for the store lock before reporting
/// [`CredentialStoreError::LockedTimeout`].
const LOCK_WAIT: Duration = Duration::from_secs(10);
const LOCK_POLL: Duration = Duration::from_millis(10);

static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(0);

/// A held exclusive lock on `credentials.json.lock`. Dropping releases it.
///
/// The lock guards a stable sidecar file, never the data file: atomic
/// replacement swaps the data file's inode, so a lock taken on the data file
/// would not survive a rename. The lock file is created once and reused; it
/// is never truncated or deleted.
pub(crate) struct StoreLock {
    file: File,
}

impl StoreLock {
    /// Acquire the exclusive store lock, retrying until `LOCK_WAIT` elapses.
    pub(crate) fn acquire(path: &Path) -> Result<Self, CredentialStoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(CredentialStoreError::Io)?;
        }
        let file = open_private(path, true)?;
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            match FileExt::try_lock(&file) {
                Ok(()) => return Ok(Self { file }),
                Err(TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(CredentialStoreError::LockedTimeout);
                    }
                    thread::sleep(LOCK_POLL);
                }
                Err(TryLockError::Error(error)) => {
                    return Err(CredentialStoreError::Io(error));
                }
            }
        }
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Open a file with mode 0600 on Unix. `create_or_open` selects `create` for
/// the reusable lock file and `create_new` for one-shot temporary siblings.
fn open_private(path: &Path, create_or_open: bool) -> Result<File, CredentialStoreError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if create_or_open {
        options.create(true);
    } else {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(CredentialStoreError::Io)
}

/// Replace `path` with `contents` atomically: write a temporary sibling,
/// flush it to disk, then rename over the destination. Callers hold the
/// store lock while this runs.
pub(crate) fn atomic_replace(path: &Path, contents: &[u8]) -> Result<(), CredentialStoreError> {
    let parent = path.parent().ok_or_else(|| {
        CredentialStoreError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "store path has no parent directory",
        ))
    })?;
    fs::create_dir_all(parent).map_err(CredentialStoreError::Io)?;
    let temp = write_temp_sibling(parent, path, contents)?;
    fs::rename(temp.path(), path).map_err(CredentialStoreError::Io)?;
    temp.disarm();
    repair_private_permissions(path)?;
    sync_parent(parent);
    Ok(())
}

fn write_temp_sibling(
    parent: &Path,
    destination: &Path,
    contents: &[u8],
) -> Result<TempSibling, CredentialStoreError> {
    let file_name = destination
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("credentials.json"));
    for _ in 0..10 {
        let mut temp_name = file_name.to_os_string();
        temp_name.push(format!(
            ".aifuel.{}.{}.tmp",
            std::process::id(),
            NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let path = parent.join(temp_name);
        match open_private(&path, false) {
            Ok(mut file) => {
                let temp = TempSibling::armed(path);
                file.write_all(contents)
                    .and_then(|()| file.sync_all())
                    .map_err(CredentialStoreError::Io)?;
                return Ok(temp);
            }
            Err(CredentialStoreError::Io(error))
                if error.kind() == io::ErrorKind::AlreadyExists =>
            {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    Err(CredentialStoreError::Io(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not choose a unique temporary file name",
    )))
}

/// Repair the data file toward mode 0600 on Unix after each write.
///
/// Windows has no portable mode bits; the file inherits the profile
/// directory's ACL, which is the documented best-effort protection. An OS
/// credential backend remains a hardening option, not a claim.
fn repair_private_permissions(path: &Path) -> Result<(), CredentialStoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(CredentialStoreError::Io)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn sync_parent(parent: &Path) {
    #[cfg(unix)]
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    #[cfg(not(unix))]
    let _ = parent;
}

/// A temporary sibling file that deletes itself unless `disarm` is called
/// after a successful rename.
struct TempSibling {
    path: PathBuf,
    armed: bool,
}

impl TempSibling {
    fn armed(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for TempSibling {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}
