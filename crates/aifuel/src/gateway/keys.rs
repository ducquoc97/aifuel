//! Downstream API keys for the `/v1` surface.
//!
//! `gateway-keys.json` inside the AI Fuel configuration directory holds one
//! record per issued key:
//! `{"version":1,"keys":[{"id","name","sha256","prefix","created_at",
//! "models","revoked","last_used_at"}]}`. A raw key is `aifuel-gw-<32 hex>`;
//! it is shown to its creator exactly once and never stored - only its
//! SHA-256 and the `aifuel-gw-<first8>` display prefix persist, so a leaked
//! store file does not hand out working credentials.
//!
//! While the store holds no keys at all, `authorize` answers `anonymous`
//! for every request - the transitional open posture until issue #109 lands
//! the remaining enforcement. The moment any key exists, `/v1` requests
//! need a matching non-revoked key: a missing bearer or an unknown digest
//! rejects with 401. A store that fails to read or parse denies rather
//! than reopens - the closed failure direction.

#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const STORE_FILE_NAME: &str = "gateway-keys.json";
const STORE_VERSION: u32 = 1;
const KEY_PREFIX: &str = "aifuel-gw-";
const ANONYMOUS: &str = "anonymous";

/// Serializes every read-modify-write over the store file - `authorize`,
/// `create`, and `revoke` each load, mutate, and save, and must not
/// interleave or one writer's update would clobber another's.
static STORE_IO: Mutex<()> = Mutex::new(());

/// Unique suffixes for temporary siblings and key material mixing.
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// One persisted key record. `sha256` is the hex SHA-256 of the full raw
/// key; the raw key itself is never written to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredKey {
    /// `k_<hex>` - a random label, independent of the key's digest.
    id: String,
    name: String,
    sha256: String,
    /// `aifuel-gw-<first8>` - the display hint shown in lists.
    prefix: String,
    created_at: u64,
    /// Optional model-selector allowlist: `null` permits every inbound
    /// `model` value, a list permits exactly its members. Stored and
    /// reported through the admin api; `/v1` enforcement lands with the
    /// rest of #109.
    #[serde(default)]
    models: Option<Vec<String>>,
    #[serde(default)]
    revoked: bool,
    #[serde(default)]
    last_used_at: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    #[serde(default)]
    keys: Vec<StoredKey>,
}

/// The key fields the admin api may show: everything but the digest.
/// `Serialize` cannot leak `sha256` or the raw key - neither is here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct KeySummary {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) prefix: String,
    pub(crate) created_at: u64,
    pub(crate) models: Option<Vec<String>>,
    pub(crate) revoked: bool,
    pub(crate) last_used_at: Option<u64>,
}

impl From<&StoredKey> for KeySummary {
    fn from(key: &StoredKey) -> Self {
        Self {
            id: key.id.clone(),
            name: key.name.clone(),
            prefix: key.prefix.clone(),
            created_at: key.created_at,
            models: key.models.clone(),
            revoked: key.revoked,
            last_used_at: key.last_used_at,
        }
    }
}

/// The store rooted at one directory - `aifuel_config_dir` in production,
/// a temporary directory in tests. Every method does synchronous file I/O.
struct KeyStore {
    path: PathBuf,
}

impl KeyStore {
    fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            path: directory.into().join(STORE_FILE_NAME),
        }
    }

    /// Resolve one caller to its identity. An empty store answers
    /// `anonymous` for anything - the transitional open posture. With keys
    /// present, a missing bearer rejects and a presented key must match a
    /// non-revoked record; a match stamps `last_used_at` and returns the
    /// key's name.
    fn authorize(&self, bearer: Option<&str>) -> Result<String, String> {
        let _guard = STORE_IO.lock().expect("gateway key store mutex");
        let mut file = self.read()?;
        if file.keys.is_empty() {
            return Ok(ANONYMOUS.to_owned());
        }
        let Some(raw) = bearer else {
            return Err("missing api key".to_owned());
        };
        let digest = sha256_hex(raw.as_bytes());
        let Some(key) = file
            .keys
            .iter_mut()
            .find(|key| !key.revoked && key.sha256 == digest)
        else {
            return Err("invalid api key".to_owned());
        };
        key.last_used_at = Some(now_unix());
        let name = key.name.clone();
        self.write(&file)?;
        Ok(name)
    }

    /// Issue a key named `name`; the raw key is returned once and only its
    /// digest is stored.
    fn create(
        &self,
        name: &str,
        models: Option<Vec<String>>,
    ) -> Result<(KeySummary, String), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("name is required".to_owned());
        }
        let _guard = STORE_IO.lock().expect("gateway key store mutex");
        let mut file = self.read()?;
        let material = fresh_material();
        let raw = format!("{KEY_PREFIX}{}", hex(&material[..16]));
        let key = StoredKey {
            id: format!("k_{}", hex(&material[16..24])),
            name: name.to_owned(),
            sha256: sha256_hex(raw.as_bytes()),
            prefix: raw[..KEY_PREFIX.len() + 8].to_owned(),
            created_at: now_unix(),
            models,
            revoked: false,
            last_used_at: None,
        };
        let summary = KeySummary::from(&key);
        file.keys.push(key);
        self.write(&file)?;
        Ok((summary, raw))
    }

    /// Mark one key revoked: future `authorize` calls reject it. The row
    /// stays so request logs can still attribute past calls by name.
    fn revoke(&self, id: &str) -> Result<(), String> {
        let _guard = STORE_IO.lock().expect("gateway key store mutex");
        let mut file = self.read()?;
        let Some(key) = file.keys.iter_mut().find(|key| key.id == id) else {
            return Err(format!("unknown key {id:?}"));
        };
        key.revoked = true;
        self.write(&file)
    }

    /// Every issued key for `GET /api/gateway/keys` - summaries only, so
    /// digests never leave the store through the listing.
    fn list(&self) -> Result<Vec<KeySummary>, String> {
        Ok(self.read()?.keys.iter().map(KeySummary::from).collect())
    }

    /// Whether the caller `authorize` resolved to `identity` may use
    /// `model` - the inbound selector verbatim, so `"auto"` and
    /// `"integration/model"` are distinct allowlist entries. The empty
    /// store permits everything (the same transitional posture `authorize`
    /// reports as `anonymous`); a store that fails to read permits
    /// nothing, matching the closed failure mode `authorize` takes.
    fn permits(&self, identity: &str, model: &str) -> bool {
        let Ok(file) = self.read() else {
            return false;
        };
        if file.keys.is_empty() {
            return true;
        }
        file.keys.iter().any(|key| {
            !key.revoked
                && key.name == identity
                && key
                    .models
                    .as_deref()
                    .is_none_or(|models| models.iter().any(|allowed| allowed == model))
        })
    }

    /// Decode the store file: absent is an empty store, malformed is an
    /// error - never a panic and never a silent reset to open.
    fn read(&self) -> Result<StoreFile, String> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(StoreFile {
                    version: STORE_VERSION,
                    keys: Vec::new(),
                });
            }
            Err(error) => {
                return Err(format!("could not read {}: {error}", self.path.display()));
            }
        };
        let file: StoreFile = serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "{} is malformed: {}",
                self.path.display(),
                corrupt_detail(&error)
            )
        })?;
        if file.version != STORE_VERSION {
            return Err(format!(
                "{} declares unsupported store version {}",
                self.path.display(),
                file.version
            ));
        }
        Ok(file)
    }

    /// Replace the store file atomically: serialize to a unique temporary
    /// sibling, then rename over the destination so a concurrent reader
    /// never sees a torn file. The file is owner-only - it holds key
    /// digests.
    fn write(&self, file: &StoreFile) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(file)
            .map_err(|error| format!("could not serialize the gateway key store: {error}"))?;
        let Some(parent) = self.path.parent() else {
            return Err("the gateway key store path has no parent directory".to_owned());
        };
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        let temp = parent.join(format!(
            ".{}.{}.{}.tmp",
            self.path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(STORE_FILE_NAME),
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed),
        ));
        write_private(&temp, &bytes).inspect_err(|_| {
            let _ = fs::remove_file(&temp);
        })?;
        fs::rename(&temp, &self.path).map_err(|error| {
            let _ = fs::remove_file(&temp);
            format!("could not replace {}: {error}", self.path.display())
        })
    }
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(bytes))
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    fs::write(path, bytes).map_err(|error| format!("could not write {}: {error}", path.display()))
}

fn default_store() -> Result<KeyStore, String> {
    Ok(KeyStore::new(crate::aifuel_config_dir()?))
}

/// The outcome of checking an inbound `/v1` request's `Authorization`
/// header. `Ok` carries the caller identity for logging (`"anonymous"`
/// while the store holds no keys); `Err` is the rejection message.
pub(crate) fn authorize(request: &tiny_http::Request) -> Result<String, String> {
    default_store()?.authorize(bearer_token(request.headers()))
}

/// Every issued key for `GET /api/gateway/keys`.
pub(crate) fn list() -> Result<Vec<KeySummary>, String> {
    default_store()?.list()
}

/// Issue a key for `POST /api/gateway/keys`. The returned raw key is the
/// only time it is ever exposed - the store keeps only its digest.
pub(crate) fn create(
    name: &str,
    models: Option<Vec<String>>,
) -> Result<(KeySummary, String), String> {
    default_store()?.create(name, models)
}

/// Revoke one key for `POST /api/gateway/keys/revoke`.
pub(crate) fn revoke(id: &str) -> Result<(), String> {
    default_store()?.revoke(id)
}

/// Whether the caller resolved to `identity` may use `model`, honoring the
/// stored `models` allowlist. `/v1` enforcement is deferred to the rest of
/// #109; this is the check the wire will call.
#[allow(dead_code)] // exposed for #109; no caller wires it yet
pub(crate) fn permits(identity: &str, model: &str) -> bool {
    match default_store() {
        Ok(store) => store.permits(identity, model),
        Err(_) => false,
    }
}

/// `Authorization: Bearer <key>` → the presented key, trimmed. Anything
/// else - absent, another scheme, an empty token - is `None`.
fn bearer_token(headers: &[tiny_http::Header]) -> Option<&str> {
    let value = headers
        .iter()
        .find(|header| header.field.equiv("Authorization"))?
        .value
        .as_str()
        .trim();
    let (scheme, token) = value.split_once(char::is_whitespace)?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|token| !token.is_empty())
}

/// 32 bytes of best-effort key material: `/dev/urandom` folded through
/// SHA-256 with process, sequence, time, and an ASLR-influenced marker -
/// the same mix `aifuel-mcp` session ids use, so a degraded entropy source
/// still yields unguessable material.
fn fresh_material() -> [u8; 32] {
    let mut digest = Sha256::new();
    let mut entropy = [0_u8; 32];
    if let Ok(mut file) = fs::File::open("/dev/urandom") {
        let _ = file.read_exact(&mut entropy);
    }
    digest.update(entropy);
    digest.update(std::process::id().to_be_bytes());
    digest.update(NEXT_ID.fetch_add(1, Ordering::Relaxed).to_be_bytes());
    digest.update(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_be_bytes(),
    );
    let marker = 0_u8;
    digest.update((&marker as *const u8 as usize).to_be_bytes());
    digest.finalize().into()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(out, "{byte:02x}").expect("writing to a string cannot fail");
    }
    out
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// A sanitized corruption report: serde error text can echo stored
/// strings, so only the error class and position surface, never content.
fn corrupt_detail(error: &serde_json::Error) -> String {
    let class = match error.classify() {
        serde_json::error::Category::Io => "unreadable data",
        serde_json::error::Category::Syntax => "invalid JSON",
        serde_json::error::Category::Data => "data does not match the key store schema",
        serde_json::error::Category::Eof => "truncated JSON",
    };
    format!("{class} at line {} column {}", error.line(), error.column())
}
