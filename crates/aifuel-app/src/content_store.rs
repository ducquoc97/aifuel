use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct PersistedContent {
    pub output: Option<String>,
    pub error: Option<String>,
    pub diagnostics: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ContentStore {
    directory: PathBuf,
}

impl ContentStore {
    pub(crate) fn new(directory: impl Into<PathBuf>) -> Result<Self, ContentStoreError> {
        let directory = directory.into();
        fs::create_dir_all(&directory)?;
        Ok(Self { directory })
    }

    pub(crate) fn load(&self, run_id: &str) -> Result<Option<PersistedContent>, ContentStoreError> {
        let path = self.path(run_id);
        match fs::read(path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(ContentStoreError::Io(error)),
        }
    }

    pub(crate) fn save(
        &self,
        run_id: &str,
        content: &PersistedContent,
    ) -> Result<(), ContentStoreError> {
        let path = self.path(run_id);
        let temporary = path.with_extension("tmp");
        let bytes = serde_json::to_vec(content)?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        std::io::Write::write_all(&mut file, &bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(temporary, path)?;
        Ok(())
    }

    fn path(&self, run_id: &str) -> PathBuf {
        let safe = run_id
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        self.directory.join(format!("{safe}.json"))
    }
}

#[derive(Debug)]
pub(crate) enum ContentStoreError {
    Io(io::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for ContentStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "content store I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "invalid content store JSON: {error}"),
        }
    }
}

impl std::error::Error for ContentStoreError {}

impl From<io::Error> for ContentStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for ContentStoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}
