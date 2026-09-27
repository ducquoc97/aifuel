//! Store failure surface. Errors are surfaced to callers at the executable
//! boundary; inside the manager, persistence failures degrade to in-memory
//! operation rather than aborting runs.

use std::io;

#[derive(Debug)]
pub enum RunStoreError {
    Io(io::Error),
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    Legacy(String),
    UnsupportedSchema(String),
}

impl std::fmt::Display for RunStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "run store I/O failed: {error}"),
            Self::Sqlite(error) => write!(formatter, "run store query failed: {error}"),
            Self::Json(error) => write!(formatter, "run store JSON failed: {error}"),
            Self::Legacy(error) => write!(formatter, "legacy session import failed: {error}"),
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported run store schema version {version}")
            }
        }
    }
}

impl std::error::Error for RunStoreError {}

impl From<io::Error> for RunStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for RunStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<serde_json::Error> for RunStoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}
