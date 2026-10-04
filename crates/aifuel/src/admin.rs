//! The dashboard Admin Credential: the operator password that gates
//! management pages and `/api/*` routes once configured.
//!
//! `admin.json` in the AI Fuel configuration directory stores one PBKDF2
//! verifier (`v1$<iterations>$<salt_hex>$<key_hex>`) - the password itself
//! is never written, so a leaked store file does not hand out a working
//! credential. `AIFUEL_ADMIN_PASSWORD` supplies a bootstrap credential for
//! containers and first-run setup; the stored file wins when both exist,
//! so `aifuel auth set-admin` always takes over from the environment.
//!
//! A successful sign-in issues a Dashboard Admin Session - an HttpOnly
//! cookie the dashboard tracks in memory. `/v1` client traffic never uses
//! it; that surface authenticates with `aifuel-gw-*` Gateway Keys instead.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const STORE_FILE_NAME: &str = "admin.json";
const STORE_VERSION: u32 = 1;
const RECORD_VERSION: &str = "v1";
const ITERATIONS: u32 = 210_000;
const MIN_PASSWORD_LEN: usize = 8;

/// The bootstrap credential environment variable - the OmniRoute
/// INITIAL_PASSWORD equivalent for container first-run.
pub const ADMIN_PASSWORD_ENV: &str = "AIFUEL_ADMIN_PASSWORD";

/// Serializes read-modify-write over the admin file, mirroring the
/// gateway key store's store-level mutex.
static STORE_IO: Mutex<()> = Mutex::new(());

/// Unique suffixes for temporary siblings and token material mixing.
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    /// `v1$<iterations>$<salt_hex>$<key_hex>` - the PBKDF2 verifier.
    password: String,
}

/// Whether an admin credential is in force: a stored verifier, or the
/// bootstrap environment variable when no file was ever written.
pub fn configured() -> bool {
    match stored_verifier() {
        Ok(Some(_)) => true,
        Ok(None) => env_password().is_some(),
        // A store that cannot be read or parsed still counts as
        // configured - the closed failure direction, matching the gateway
        // key store: management stays gated rather than silently opening.
        Err(_) => true,
    }
}

/// Check one presented password against the effective credential. An
/// unconfigured store verifies nothing.
pub fn verify(password: &str) -> bool {
    match stored_verifier() {
        Ok(Some(record)) => verify_record(&record, password),
        Ok(None) => match env_password() {
            Some(expected) => digest_eq(password.as_bytes(), expected.as_bytes()),
            None => false,
        },
        Err(_) => false,
    }
}

/// Store a new admin password, replacing any existing verifier. Also the
/// way off the env bootstrap: once the file exists the variable is
/// ignored.
pub fn set_password(password: &str) -> Result<(), String> {
    if password.len() < MIN_PASSWORD_LEN {
        return Err(format!(
            "the admin password must be at least {MIN_PASSWORD_LEN} characters"
        ));
    }
    let _guard = STORE_IO.lock().expect("admin store mutex");
    let salt = fresh_material();
    let key = pbkdf2_sha256(password.as_bytes(), &salt, ITERATIONS);
    let file = StoreFile {
        version: STORE_VERSION,
        password: format!("{RECORD_VERSION}${ITERATIONS}${}${}", hex(&salt), hex(&key)),
    };
    write(&file)
}

/// Delete the stored verifier. The env bootstrap, when set, becomes the
/// effective credential again on the next check.
pub fn remove() -> Result<(), String> {
    let _guard = STORE_IO.lock().expect("admin store mutex");
    let path = store_path()?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err("no admin credential is stored".to_owned())
        }
        Err(error) => Err(format!("could not remove {}: {error}", path.display())),
    }
}

/// One verifier record out of `admin.json`: `Ok(None)` when absent or
/// empty, `Err` when present but unreadable or malformed.
fn stored_verifier() -> Result<Option<String>, String> {
    let path = store_path()?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!("could not read {}: {error}", path.display()));
        }
    };
    let file: StoreFile = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{} is malformed: {error}", path.display()))?;
    if file.version != STORE_VERSION {
        return Err(format!(
            "{} declares unsupported store version {}",
            path.display(),
            file.version
        ));
    }
    Ok(Some(file.password))
}

/// Whether `password` satisfies one stored `v1$iterations$salt$key`
/// record. A record that does not parse verifies nothing.
fn verify_record(record: &str, password: &str) -> bool {
    let mut parts = record.split('$');
    let (Some(version), Some(iterations), Some(salt), Some(key), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return false;
    };
    if version != RECORD_VERSION {
        return false;
    }
    let (Ok(iterations), Ok(salt), Ok(key)) = (iterations.parse::<u32>(), unhex(salt), unhex(key))
    else {
        return false;
    };
    let candidate = pbkdf2_sha256(password.as_bytes(), &salt, iterations);
    candidate.len() == key.len() && digest_eq(&candidate, &key)
}

fn env_password() -> Option<String> {
    aifuel_providers::env_override(ADMIN_PASSWORD_ENV)
}

fn store_path() -> Result<PathBuf, String> {
    Ok(crate::aifuel_config_dir()?.join(STORE_FILE_NAME))
}

/// Replace `admin.json` atomically and owner-only - the same write the
/// gateway key store performs, since the file carries a verifier.
fn write(file: &StoreFile) -> Result<(), String> {
    let path = store_path()?;
    let bytes = serde_json::to_vec_pretty(file)
        .map_err(|error| format!("could not serialize the admin store: {error}"))?;
    let Some(parent) = path.parent() else {
        return Err("the admin store path has no parent directory".to_owned());
    };
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    let temp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(STORE_FILE_NAME),
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed),
    ));
    write_private(&temp, &bytes).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })?;
    fs::rename(&temp, &path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("could not replace {}: {error}", path.display())
    })
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

/// 32 bytes of session/token material: `/dev/urandom` folded through
/// SHA-256 with process, sequence, time, and an ASLR-influenced marker -
/// the same mix gateway keys and MCP session ids use.
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

/// A random session token - hex of fresh material. Used by the
/// dashboard's Admin Session issuer.
pub fn fresh_token() -> String {
    hex(&fresh_material())
}

/// PBKDF2-HMAC-SHA256 with a one-block (32-byte) derived key.
fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut block = salt.to_vec();
    block.extend_from_slice(&1_u32.to_be_bytes());
    let mut u = hmac_sha256(password, &block);
    let mut out = u;
    for _ in 1..iterations {
        u = hmac_sha256(password, &u);
        for (byte, mixed) in out.iter_mut().zip(u.iter()) {
            *byte ^= mixed;
        }
    }
    out
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key_block = [0_u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Vec::with_capacity(BLOCK + message.len());
    inner.extend(key_block.iter().map(|byte| byte ^ 0x36));
    inner.extend_from_slice(message);
    let inner = Sha256::digest(&inner);
    let mut outer = Vec::with_capacity(BLOCK + inner.len());
    outer.extend(key_block.iter().map(|byte| byte ^ 0x5c));
    outer.extend_from_slice(&inner);
    Sha256::digest(&outer).into()
}

/// Length-agnostic constant-time compare: both sides are hashed first so
/// neither the verifier length nor the candidate length leaks.
fn digest_eq(a: &[u8], b: &[u8]) -> bool {
    let (a, b) = (Sha256::digest(a), Sha256::digest(b));
    a.iter()
        .zip(b.iter())
        .fold(0_u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(out, "{byte:02x}").expect("writing to a string cannot fail");
    }
    out
}

fn unhex(text: &str) -> Result<Vec<u8>, ()> {
    if text.len() % 2 != 0 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(());
    }
    Ok((0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("hex digits parse"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The widely published PBKDF2-HMAC-SHA256 vectors for
    /// ("password", "salt") - the same input pair RFC 6070 uses for SHA-1.
    #[test]
    fn pbkdf2_matches_published_vectors() {
        assert_eq!(
            hex(&pbkdf2_sha256(b"password", b"salt", 1)),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        assert_eq!(
            hex(&pbkdf2_sha256(b"password", b"salt", 2)),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
        assert_eq!(
            hex(&pbkdf2_sha256(b"password", b"salt", 4096)),
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
        );
    }

    /// A record written by `set_password` must verify the same password
    /// and reject a wrong one - the round trip every login depends on.
    /// The record is built inline with a cheap iteration count so the
    /// test never touches the real config dir or pays the production
    /// work factor; `verify_record` reads its iterations from the record.
    #[test]
    fn a_record_verifies_its_password_and_rejects_others() {
        let salt = [7_u8; 16];
        let key = pbkdf2_sha256(b"s3cret-operator", &salt, 1_000);
        let record = format!("v1$1000${}${}", hex(&salt), hex(&key));
        assert!(verify_record(&record, "s3cret-operator"));
        assert!(!verify_record(&record, "s3cret-operatoR"));
        assert!(!verify_record(&record, ""));
    }

    /// Malformed or foreign-format records verify nothing - the closed
    /// failure direction, so a hand-edited or truncated admin.json can
    /// only lock operators out, never let anyone in.
    #[test]
    fn unparseable_records_verify_nothing() {
        assert!(!verify_record("", "x"));
        assert!(!verify_record("v2$1$00$00", "x"));
        assert!(!verify_record("v1$abc$00$00", "x"));
        assert!(!verify_record("v1$1$zz$00", "x"));
        assert!(!verify_record("v1$1$00", "x"));
        assert!(!verify_record("v1$1$00$00$extra", "x"));
    }

    #[test]
    fn unhex_rejects_odd_or_non_hex_input() {
        assert!(unhex("0").is_err());
        assert!(unhex("0g").is_err());
        assert_eq!(unhex("00ff").expect("valid hex"), vec![0x00, 0xff]);
    }
}
