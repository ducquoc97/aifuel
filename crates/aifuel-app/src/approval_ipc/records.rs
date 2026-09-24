use super::protocol::{OwnerRecord, PendingApprovalRecord};
use fs4::{FileExt, TryLockError};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_OWNER_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(super) fn canonical_private_directory(directory: &Path) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(directory)
        .map_err(|error| format!("could not inspect local approval owners: {error}"))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err("local approval directory must be a private directory".to_owned());
    }
    fs::canonicalize(directory)
        .map_err(|error| format!("could not resolve local approval owners: {error}"))
}

pub(super) fn active_owner_record(
    path: &Path,
    directory: &Path,
    socket_root: &Path,
) -> Result<Option<OwnerRecord>, String> {
    let before = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if !private_regular_file(&before) {
        return Ok(None);
    }
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let opened = file.metadata().map_err(|error| error.to_string())?;
    if !private_regular_file(&opened) || !same_file(&before, &opened) {
        return Ok(None);
    }

    match FileExt::try_lock(&file) {
        Ok(()) => {
            remove_stale_owner_record(path, directory, socket_root, &opened);
            Ok(None)
        }
        Err(TryLockError::WouldBlock) => {
            let record = read_owner_record(&mut file).map_err(|error| error.to_string())?;
            if valid_owner_record(path, directory, socket_root, &record) {
                Ok(Some(record))
            } else {
                Ok(None)
            }
        }
        Err(TryLockError::Error(error)) => Err(error.to_string()),
    }
}

fn read_owner_record(file: &mut File) -> io::Result<OwnerRecord> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

fn valid_owner_record(
    path: &Path,
    directory: &Path,
    socket_root: &Path,
    record: &OwnerRecord,
) -> bool {
    valid_owner_id(&record.owner_id)
        && path == owner_record_path(directory, &record.owner_id)
        && record.socket == owner_socket_path(socket_root, &record.owner_id)
}

pub(super) fn read_pending_record(
    path: &Path,
) -> io::Result<Option<(PendingApprovalRecord, fs::Metadata)>> {
    let before = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !private_regular_file(&before) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pending approval mapping is not a private regular file",
        ));
    }
    let mut file = OpenOptions::new().read(true).open(path)?;
    let opened = file.metadata()?;
    if !private_regular_file(&opened) || !same_file(&before, &opened) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pending approval mapping changed while it was read",
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let record = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    Ok(Some((record, opened)))
}

pub(super) fn valid_pending_record(
    path: &Path,
    directory: &Path,
    socket_root: &Path,
    record: &PendingApprovalRecord,
) -> bool {
    valid_owner_id(&record.owner_id)
        && path == pending_record_path(directory, &record.run_id, &record.input_id)
        && record.socket == owner_socket_path(socket_root, &record.owner_id)
}

fn remove_stale_owner_record(
    path: &Path,
    directory: &Path,
    socket_root: &Path,
    opened_metadata: &fs::Metadata,
) {
    if !fs::symlink_metadata(path).is_ok_and(|metadata| {
        private_regular_file(&metadata) && same_file(opened_metadata, &metadata)
    }) {
        return;
    }
    if let Some(owner_id) = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|owner_id| {
            valid_owner_id(owner_id) && path == owner_record_path(directory, owner_id)
        })
    {
        // The owner lock is held by this caller. Pending mappings are removed
        // before unlinking the lock file so another process cannot mistake a
        // still-live owner's entries for stale ones.
        remove_pending_records_for_owner(directory, socket_root, owner_id);
        remove_private_socket(
            &owner_socket_path(socket_root, owner_id),
            owner_id,
            socket_root,
        );
    }
    let _ = fs::remove_file(path);
}

pub(super) fn remove_pending_records_for_owner(
    directory: &Path,
    socket_root: &Path,
    owner_id: &str,
) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_none_or(|name| !name.starts_with("pending-") || !name.ends_with(".json"))
        {
            continue;
        }
        let Ok(Some((record, metadata))) = read_pending_record(&path) else {
            continue;
        };
        if record.owner_id == owner_id
            && valid_pending_record(&path, directory, socket_root, &record)
        {
            remove_private_record(&path, &metadata);
        }
    }
}

pub(super) fn remove_private_record(path: &Path, opened_metadata: &fs::Metadata) {
    if fs::symlink_metadata(path).is_ok_and(|metadata| {
        private_regular_file(&metadata) && same_file(opened_metadata, &metadata)
    }) {
        let _ = fs::remove_file(path);
    }
}

fn remove_private_socket(path: &Path, owner_id: &str, socket_root: &Path) {
    if path != owner_socket_path(socket_root, owner_id) {
        return;
    }
    let socket_directory = owner_socket_directory(socket_root, owner_id);
    let Ok(directory_metadata) = fs::symlink_metadata(&socket_directory) else {
        return;
    };
    if directory_metadata.file_type().is_symlink()
        || !directory_metadata.is_dir()
        || directory_metadata.permissions().mode() & 0o077 != 0
    {
        return;
    }
    if valid_socket_file(path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_dir(socket_directory);
    }
}

pub(super) fn valid_socket_file(path: &Path) -> bool {
    let private_parent = path.parent().is_some_and(|parent| {
        fs::symlink_metadata(parent).is_ok_and(|metadata| {
            !metadata.file_type().is_symlink()
                && metadata.is_dir()
                && metadata.permissions().mode() & 0o077 == 0
        })
    });
    private_parent
        && fs::symlink_metadata(path).is_ok_and(|metadata| {
            metadata.file_type().is_socket() && metadata.permissions().mode() & 0o077 == 0
        })
}

fn private_regular_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file() && metadata.permissions().mode() & 0o077 == 0
}

fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

pub(super) fn owner_record_path(directory: &Path, owner_id: &str) -> PathBuf {
    directory.join(format!("{owner_id}.json"))
}

pub(super) fn pending_record_path(directory: &Path, run_id: &str, input_id: &str) -> PathBuf {
    let mut digest = Sha256::new();
    digest.update((run_id.len() as u64).to_be_bytes());
    digest.update(run_id.as_bytes());
    digest.update((input_id.len() as u64).to_be_bytes());
    digest.update(input_id.as_bytes());
    let hash = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    directory.join(format!("pending-{hash}.json"))
}

pub(super) fn owner_socket_directory(socket_root: &Path, owner_id: &str) -> PathBuf {
    socket_root.join(format!("af-{owner_id}"))
}

pub(super) fn owner_socket_path(socket_root: &Path, owner_id: &str) -> PathBuf {
    owner_socket_directory(socket_root, owner_id).join("s")
}

pub(super) fn canonical_socket_root() -> io::Result<PathBuf> {
    let root = fs::canonicalize("/tmp")?;
    if !fs::metadata(&root)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "canonical /tmp is not a directory",
        ));
    }
    Ok(root)
}

pub(super) fn socket_path_fits(path: &Path) -> bool {
    let limit = if cfg!(target_os = "linux") { 108 } else { 104 };
    path.as_os_str().as_bytes().len() < limit
}

pub(super) fn next_owner_id() -> io::Result<String> {
    let mut nonce = [0_u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut nonce)?;
    let sequence = NEXT_OWNER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nonce = nonce
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("o-{:x}-{sequence:08x}-{nonce}", std::process::id()))
}

fn valid_owner_id(owner_id: &str) -> bool {
    owner_id.starts_with("o-")
        && owner_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

pub(super) fn write_owner_record(path: &Path, record: &OwnerRecord) -> io::Result<File> {
    let temporary = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&temporary)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    FileExt::lock(&file)?;
    serde_json::to_writer(&mut file, record).map_err(io::Error::other)?;
    file.sync_all()?;
    fs::hard_link(&temporary, path)?;
    fs::remove_file(&temporary)?;
    Ok(file)
}

pub(super) fn write_pending_record(
    path: &Path,
    record: &PendingApprovalRecord,
    owner_id: &str,
) -> io::Result<()> {
    let temporary = path.with_extension(format!("tmp-{owner_id}"));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    serde_json::to_writer(&mut file, record).map_err(io::Error::other)?;
    file.sync_all()?;
    fs::hard_link(&temporary, path)?;
    fs::remove_file(temporary)
}
