use super::config::{GLOBAL_SELECTION_SCHEMA_VERSION, GlobalSelectionConfig};
use serde_json::Error as JsonError;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// File-backed access to the private global selection configuration.
///
/// The executable supplies the path, normally its per-user
/// `aifuel/execution.json` location. This type does not derive a path from
/// environment variables and does not know about a user's home directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionStore {
    path: PathBuf,
}

impl SelectionStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load a config from `path`; an absent file is the empty default config.
    pub fn load(path: impl AsRef<Path>) -> Result<GlobalSelectionConfig, SelectionStoreError> {
        let path = path.as_ref();
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(GlobalSelectionConfig::default());
            }
            Err(error) => return Err(SelectionStoreError::Io(error)),
        };
        let config: GlobalSelectionConfig = serde_json::from_slice(&bytes)?;
        if config.schema_version != GLOBAL_SELECTION_SCHEMA_VERSION {
            return Err(SelectionStoreError::UnsupportedSchema(
                config.schema_version,
            ));
        }
        Ok(config)
    }

    pub fn read(&self) -> Result<GlobalSelectionConfig, SelectionStoreError> {
        Self::load(&self.path)
    }

    /// Save an explicitly changed config using a same-directory temporary
    /// file and replacement. The file is private on Unix-like platforms.
    pub fn save(
        path: impl AsRef<Path>,
        config: &GlobalSelectionConfig,
    ) -> Result<(), SelectionStoreError> {
        let path = path.as_ref();
        if config.schema_version != GLOBAL_SELECTION_SCHEMA_VERSION {
            return Err(SelectionStoreError::UnsupportedSchema(
                config.schema_version,
            ));
        }
        let serialized = serde_json::to_vec_pretty(config)?;
        let parent = path
            .parent()
            .filter(|directory| !directory.as_os_str().is_empty());
        if let Some(parent) = parent {
            fs::create_dir_all(parent)?;
        }
        let temporary_path = temporary_path(path);
        let write_result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            let mut file = options.open(&temporary_path)?;
            set_private_permissions(&file)?;
            file.write_all(&serialized)?;
            file.sync_all()?;
            drop(file);
            replace_file(&temporary_path, path)
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        write_result.map_err(SelectionStoreError::Io)
    }

    pub fn write(&self, config: &GlobalSelectionConfig) -> Result<(), SelectionStoreError> {
        Self::save(&self.path, config)
    }
}

#[derive(Debug)]
pub enum SelectionStoreError {
    Io(io::Error),
    Json(JsonError),
    UnsupportedSchema(u32),
}

impl fmt::Display for SelectionStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "selection configuration I/O failed: {error}"),
            Self::Json(error) => write!(f, "invalid selection configuration JSON: {error}"),
            Self::UnsupportedSchema(version) => write!(
                f,
                "unsupported selection configuration schema version {version}"
            ),
        }
    }
}

impl std::error::Error for SelectionStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::UnsupportedSchema(_) => None,
        }
    }
}

impl From<io::Error> for SelectionStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<JsonError> for SelectionStoreError {
    fn from(error: JsonError) -> Self {
        Self::Json(error)
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("execution.json");
    path.with_file_name(format!(".{name}.tmp-{}-{suffix}", std::process::id()))
}

fn set_private_permissions(_file: &std::fs::File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        _file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn replace_file(temporary_path: &Path, path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    if path.exists() {
        // Windows does not replace an existing file with rename. The target
        // is a caller-owned config path, so preserve the same private mode
        // while using the platform's available replacement operation.
        fs::remove_file(path)?;
    }
    fs::rename(temporary_path, path)
}
