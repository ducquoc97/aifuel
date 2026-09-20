use super::AgentMcpSetupError;
use fs4::FileExt;
use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(0);

pub(super) struct RegistrationLock(File);

impl Drop for RegistrationLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

#[derive(Clone)]
pub(super) struct FileSnapshot {
    pub(super) contents: Option<Vec<u8>>,
    pub(super) permissions: Option<Permissions>,
    permission_state: Option<PermissionState>,
}

impl FileSnapshot {
    fn missing() -> Self {
        Self {
            contents: None,
            permissions: None,
            permission_state: None,
        }
    }

    fn matches(&self, other: &Self) -> bool {
        self.contents == other.contents && self.permission_state == other.permission_state
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct PermissionState(u32);

#[cfg(not(unix))]
#[derive(Clone, Copy, PartialEq, Eq)]
struct PermissionState(bool);

pub(super) fn acquire_lock(path: &Path) -> Result<RegistrationLock, AgentMcpSetupError> {
    let file = open_private_file(path, true)
        .map_err(|error| io_error("could not open the Agent MCP Registration lock", error))?;
    FileExt::lock(&file)
        .map_err(|error| io_error("could not lock the Agent MCP Registration", error))?;
    Ok(RegistrationLock(file))
}

pub(super) fn read_snapshot(path: &Path, label: &str) -> Result<FileSnapshot, AgentMcpSetupError> {
    let before = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(FileSnapshot::missing()),
        Err(error) => return Err(io_error(&format!("could not inspect {label}"), error)),
    };
    if before.file_type().is_symlink() || !before.is_file() {
        return Err(AgentMcpSetupError::new(format!(
            "{label} must be a regular file; configuration was left untouched"
        )));
    }
    let contents =
        fs::read(path).map_err(|error| io_error(&format!("could not read {label}"), error))?;
    let after = fs::symlink_metadata(path)
        .map_err(|error| io_error(&format!("could not recheck {label}"), error))?;
    if after.file_type().is_symlink() || !after.is_file() {
        return Err(AgentMcpSetupError::new(format!(
            "{label} changed shape while it was read; configuration was left untouched"
        )));
    }
    Ok(snapshot_from_metadata(contents, after))
}

pub(super) fn ensure_snapshot_matches(
    path: &Path,
    snapshot: &FileSnapshot,
    label: &str,
) -> Result<(), AgentMcpSetupError> {
    let current = read_snapshot(path, label)?;
    if !snapshot.matches(&current) {
        return Err(AgentMcpSetupError::new(format!(
            "{label} changed during setup; no replacement was committed"
        )));
    }
    Ok(())
}

pub(super) fn create_private_dir_all(path: &Path) -> Result<(), AgentMcpSetupError> {
    create_directory_all(path, true)?;
    protect_private_directory_permissions(path)
        .map_err(|error| io_error("could not protect AI Fuel registration state", error))
}

fn create_directory_all(path: &Path, private: bool) -> Result<(), AgentMcpSetupError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    if private {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
    }
    builder
        .create(path)
        .or_else(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() {
                Ok(())
            } else {
                Err(error)
            }
        })
        .map_err(|error| io_error("could not create configuration directory", error))
}

fn protect_private_directory_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)?.permissions().mode();
        if mode & 0o077 == 0 {
            return Ok(());
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    }
    #[cfg(windows)]
    {
        let mut permissions = fs::metadata(path)?.permissions();
        #[expect(
            clippy::permissions_set_readonly_false,
            reason = "On Windows this clears FILE_ATTRIBUTE_READONLY; Unix permissions use the PermissionsExt branch above."
        )]
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)
    }
}

fn open_private_file(path: &Path, create_or_open: bool) -> io::Result<File> {
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
    options.open(path)
}

fn write_temp_file(
    destination: &Path,
    contents: &[u8],
    permissions: Option<Permissions>,
) -> Result<TempFile, AgentMcpSetupError> {
    let parent = destination
        .parent()
        .ok_or_else(|| AgentMcpSetupError::new("configuration path has no parent directory"))?;
    let file_name = destination
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("config"));
    for _ in 0..10 {
        let mut temp_name = file_name.to_os_string();
        temp_name.push(format!(
            ".aifuel.{}.{}.tmp",
            std::process::id(),
            NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let path = parent.join(temp_name);
        match open_private_file(&path, false) {
            Ok(mut file) => {
                let temp = TempFile::new(path);
                file.write_all(contents)
                    .and_then(|()| file.sync_all())
                    .map_err(|error| {
                        io_error("could not write replacement configuration", error)
                    })?;
                if let Some(permissions) = permissions {
                    fs::set_permissions(temp.path(), permissions).map_err(|error| {
                        io_error(
                            "could not preserve MCP Host configuration permissions",
                            error,
                        )
                    })?;
                }
                return Ok(temp);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(io_error(
                    "could not create replacement configuration",
                    error,
                ));
            }
        }
    }
    Err(AgentMcpSetupError::new(
        "could not choose a unique temporary configuration file name",
    ))
}

pub(super) fn replace_config(
    config_file: &Path,
    backup_dir: &Path,
    snapshot: &FileSnapshot,
    updated: &[u8],
) -> Result<Option<PathBuf>, AgentMcpSetupError> {
    let backup = write_backup(config_file, backup_dir, snapshot)?;
    replace_config_with_backup(config_file, snapshot, updated, backup.as_deref())?;
    Ok(backup)
}

pub(super) fn replace_config_with_backup(
    config_file: &Path,
    snapshot: &FileSnapshot,
    updated: &[u8],
    backup: Option<&Path>,
) -> Result<(), AgentMcpSetupError> {
    let parent = config_file.parent().ok_or_else(|| {
        AgentMcpSetupError::new("MCP Host configuration path has no parent directory")
    })?;
    create_directory_all(parent, true)?;
    let temp = write_temp_file(config_file, updated, snapshot.permissions.clone())?;
    if let Err(error) = ensure_snapshot_matches(config_file, snapshot, "MCP Host configuration") {
        return Err(with_backup(error, backup));
    }
    if let Err(error) = fs::rename(temp.path(), config_file) {
        return Err(with_backup(
            io_error("could not atomically replace MCP Host configuration", error),
            backup,
        ));
    }
    temp.disarm();
    sync_parent(parent);
    Ok(())
}

pub(super) fn write_backup(
    config_file: &Path,
    backup_dir: &Path,
    snapshot: &FileSnapshot,
) -> Result<Option<PathBuf>, AgentMcpSetupError> {
    let Some(original) = snapshot.contents.as_deref() else {
        return Ok(None);
    };
    create_private_dir_all(backup_dir)?;
    let path = backup_path(config_file, backup_dir);
    let previous = read_snapshot(&path, "private MCP Host configuration backup")?;
    let temp = write_temp_file(&path, original, None)?;
    ensure_snapshot_matches(&path, &previous, "private MCP Host configuration backup")?;
    fs::rename(temp.path(), &path).map_err(|error| {
        io_error(
            "could not atomically save the private MCP Host configuration backup",
            error,
        )
    })?;
    temp.disarm();
    sync_parent(backup_dir);
    Ok(Some(path))
}

pub(super) fn backup_path(config_file: &Path, backup_dir: &Path) -> PathBuf {
    let file_name = config_file
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("config.toml"));
    let mut name = file_name.to_os_string();
    name.push(".backup");
    backup_dir.join(name)
}

pub(super) fn atomic_write(
    destination: &Path,
    contents: &[u8],
    expected: &FileSnapshot,
    permissions: Option<Permissions>,
) -> Result<(), AgentMcpSetupError> {
    let parent = destination.parent().ok_or_else(|| {
        AgentMcpSetupError::new("registration state path has no parent directory")
    })?;
    let temp = write_temp_file(destination, contents, permissions)?;
    ensure_snapshot_matches(destination, expected, "Agent MCP Registration receipt")?;
    fs::rename(temp.path(), destination)
        .map_err(|error| io_error("could not atomically save the ownership receipt", error))?;
    temp.disarm();
    sync_parent(parent);
    Ok(())
}

struct TempFile {
    path: PathBuf,
    active: bool,
}

impl TempFile {
    fn new(path: PathBuf) -> Self {
        Self { path, active: true }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(mut self) {
        self.active = false;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.active {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn snapshot_from_metadata(contents: Vec<u8>, metadata: Metadata) -> FileSnapshot {
    let permissions = metadata.permissions();
    let permission_state = permission_state(&permissions);
    FileSnapshot {
        contents: Some(contents),
        permissions: Some(permissions),
        permission_state: Some(permission_state),
    }
}

fn permission_state(permissions: &Permissions) -> PermissionState {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        PermissionState(permissions.mode())
    }
    #[cfg(not(unix))]
    {
        PermissionState(permissions.readonly())
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

pub(super) fn with_backup(error: AgentMcpSetupError, backup: Option<&Path>) -> AgentMcpSetupError {
    match backup {
        Some(path) => AgentMcpSetupError::new(format!(
            "{error}. The original MCP Host configuration backup is at {}",
            path.display()
        )),
        None => error,
    }
}

pub(super) fn io_error(action: &str, error: io::Error) -> AgentMcpSetupError {
    AgentMcpSetupError::new(format!("{action}: {error}"))
}
