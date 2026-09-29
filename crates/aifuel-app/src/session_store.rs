use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::PathBuf;

const SESSION_STORE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PersistedSession {
    /// The configured integration that created this native session.
    pub integration: aifuel_core::IntegrationId,
    /// The upstream provider that integration executed against.
    pub provider: aifuel_core::ProviderId,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub working_directory: PathBuf,
}

impl<'de> Deserialize<'de> for PersistedSession {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct PersistedSessionJson {
            integration: Option<String>,
            provider: Option<String>,
            model: Option<String>,
            effort: Option<String>,
            working_directory: PathBuf,
        }
        let json = PersistedSessionJson::deserialize(deserializer)?;
        // Version-1 rows stored only the provider key. Built-in integration
        // ids equal the catalog provider ids, so that value serves both
        // identities when a legacy row is read.
        let integration = json
            .integration
            .or_else(|| json.provider.clone())
            .ok_or_else(|| serde::de::Error::missing_field("integration"))?;
        Ok(Self {
            provider: aifuel_core::ProviderId::new(
                json.provider.unwrap_or_else(|| integration.clone()),
            ),
            integration: aifuel_core::IntegrationId::new(integration),
            model: json.model,
            effort: json.effort,
            working_directory: json.working_directory,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SessionStore {
    path: PathBuf,
    sessions: BTreeMap<String, PersistedSession>,
}

impl SessionStore {
    pub(crate) fn load(path: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        let path = path.into();
        let contents = match fs::read(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(SessionStoreError::Io(error)),
        };
        if contents.is_empty() {
            return Ok(Self {
                path,
                sessions: BTreeMap::new(),
            });
        }
        let persisted: PersistedSessions = serde_json::from_slice(&contents)?;
        if persisted.schema_version != SESSION_STORE_SCHEMA_VERSION {
            return Err(SessionStoreError::UnsupportedSchema(
                persisted.schema_version,
            ));
        }
        Ok(Self {
            path,
            sessions: persisted.sessions,
        })
    }

    pub(crate) fn get(&self, session_id: &str) -> Option<PersistedSession> {
        self.sessions.get(session_id).cloned()
    }

    pub(crate) fn sessions(&self) -> &BTreeMap<String, PersistedSession> {
        &self.sessions
    }

    pub(crate) fn insert(
        &mut self,
        session_id: String,
        session: PersistedSession,
    ) -> Result<(), SessionStoreError> {
        self.sessions.insert(session_id, session);
        self.save()
    }

    fn save(&self) -> Result<(), SessionStoreError> {
        let Some(parent) = self.path.parent() else {
            return Err(SessionStoreError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session store has no parent directory",
            )));
        };
        fs::create_dir_all(parent)?;
        let bytes = serde_json::to_vec_pretty(&PersistedSessions {
            schema_version: SESSION_STORE_SCHEMA_VERSION,
            sessions: self.sessions.clone(),
        })?;
        let temporary = self.path.with_extension("tmp");
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
        fs::rename(temporary, &self.path)?;
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedSessions {
    schema_version: u32,
    sessions: BTreeMap<String, PersistedSession>,
}

#[derive(Debug)]
pub(crate) enum SessionStoreError {
    Io(io::Error),
    Json(serde_json::Error),
    UnsupportedSchema(u32),
}

impl std::fmt::Display for SessionStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "session store I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "invalid session store JSON: {error}"),
            Self::UnsupportedSchema(version) => {
                write!(
                    formatter,
                    "unsupported session store schema version {version}"
                )
            }
        }
    }
}

impl std::error::Error for SessionStoreError {}

impl From<io::Error> for SessionStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for SessionStoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}
