use fs4::{FileExt, TryLockError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
    GetFileInformationByHandle,
};

use super::PIPE_PREFIX;
use super::security;

const MAX_FRAME_BYTES: usize = 64 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct OwnerRecord {
    pub(super) owner_id: String,
    pub(super) pipe_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct PendingApprovalRecord {
    pub(super) owner_id: String,
    pub(super) pipe_name: String,
    pub(super) run_id: String,
    pub(super) input_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FileIdentity {
    volume_serial: u32,
    file_index: u64,
}

pub(super) fn pipe_name(owner_id: &str) -> String {
    format!("{PIPE_PREFIX}{owner_id}")
}

pub(super) fn owner_record_path(directory: &Path, owner_id: &str) -> PathBuf {
    directory.join(format!("{owner_id}.json"))
}

pub(super) fn owner_lock_path(owner_record: &Path) -> PathBuf {
    owner_record.with_extension("lock")
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

pub(super) fn write_owner_record(path: &Path, record: &OwnerRecord) -> io::Result<File> {
    let lock_path = owner_lock_path(path);
    let temporary = path.with_extension("tmp");
    let lock_file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    let result = (|| {
        security::set_private_dacl(&lock_path, false)?;
        FileExt::lock(&lock_file)?;

        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(&temporary)?;
            security::set_private_dacl(&temporary, false)?;
            serde_json::to_writer(&mut file, record).map_err(io::Error::other)?;
            file.sync_all()?;
            fs::hard_link(&temporary, path)?;
            fs::remove_file(&temporary)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    })();
    if let Err(error) = result {
        drop(lock_file);
        let _ = fs::remove_file(&lock_path);
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(lock_file)
}

pub(super) fn write_pending_record(
    path: &Path,
    record: &PendingApprovalRecord,
    owner_id: &str,
) -> io::Result<()> {
    let temporary = path.with_extension(format!("tmp-{owner_id}"));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        security::set_private_dacl(&temporary, false)?;
        serde_json::to_writer(&mut file, record).map_err(io::Error::other)?;
        file.sync_all()?;
        fs::hard_link(&temporary, path)?;
        fs::remove_file(&temporary)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(super) fn active_owner_record(
    path: &Path,
    directory: &Path,
) -> Result<Option<OwnerRecord>, String> {
    let lock_path = owner_lock_path(path);
    let lock_file = match open_checked_file(&lock_path, true, true) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let opened_lock_identity = file_identity(&lock_file).map_err(|error| error.to_string())?;

    match FileExt::try_lock(&lock_file) {
        Ok(()) => {
            if let Some(opened_record_identity) =
                current_file_identity(path).map_err(|error| error.to_string())?
            {
                remove_stale_owner_record(path, directory, opened_record_identity);
            } else if let Some(owner_id) = owner_id_for_record_path(path, directory) {
                remove_pending_records_for_owner(directory, &owner_id);
            }
            drop(lock_file);
            remove_private_record(&lock_path, opened_lock_identity);
            Ok(None)
        }
        Err(TryLockError::WouldBlock) => {
            let mut file = match open_checked_file(path, true, false) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.to_string()),
            };
            let opened_record_identity = file_identity(&file).map_err(|error| error.to_string())?;
            let record = read_owner_record(&mut file).map_err(|error| error.to_string())?;
            if current_file_identity(&lock_path).ok().flatten() == Some(opened_lock_identity)
                && current_file_identity(path).ok().flatten() == Some(opened_record_identity)
                && valid_owner_record(path, directory, &record)
            {
                Ok(Some(record))
            } else {
                Ok(None)
            }
        }
        Err(TryLockError::Error(error)) => Err(error.to_string()),
    }
}

pub(super) fn read_pending_record(
    path: &Path,
) -> io::Result<Option<(PendingApprovalRecord, FileIdentity)>> {
    let file = match open_checked_file(path, true, false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let opened_identity = file_identity(&file)?;
    let mut bytes = Vec::new();
    file.take((MAX_FRAME_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pending approval mapping is too large",
        ));
    }
    if current_file_identity(path)? != Some(opened_identity) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pending approval mapping changed while it was read",
        ));
    }
    let record = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    Ok(Some((record, opened_identity)))
}

pub(super) fn valid_owner_record(path: &Path, directory: &Path, record: &OwnerRecord) -> bool {
    valid_owner_id(&record.owner_id)
        && path == owner_record_path(directory, &record.owner_id)
        && record.pipe_name == pipe_name(&record.owner_id)
}

pub(super) fn valid_pending_record(
    path: &Path,
    directory: &Path,
    record: &PendingApprovalRecord,
) -> bool {
    valid_owner_id(&record.owner_id)
        && path == pending_record_path(directory, &record.run_id, &record.input_id)
        && record.pipe_name == pipe_name(&record.owner_id)
}

fn read_owner_record(file: &mut File) -> io::Result<OwnerRecord> {
    let mut bytes = Vec::new();
    file.take((MAX_FRAME_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "local approval owner record is too large",
        ));
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

fn remove_stale_owner_record(path: &Path, directory: &Path, opened: FileIdentity) {
    if current_file_identity(path).ok().flatten() != Some(opened) {
        return;
    }
    if let Some(owner_id) = owner_id_for_record_path(path, directory) {
        remove_pending_records_for_owner(directory, &owner_id);
    }
    let _ = fs::remove_file(path);
}

fn owner_id_for_record_path(path: &Path, directory: &Path) -> Option<String> {
    let owner_id = path.file_stem()?.to_str()?;
    (valid_owner_id(owner_id) && path == owner_record_path(directory, owner_id))
        .then(|| owner_id.to_owned())
}

pub(super) fn remove_pending_records_for_owner(directory: &Path, owner_id: &str) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("pending-") || !name.ends_with(".json") {
            continue;
        }
        let Ok(Some((record, identity))) = read_pending_record(&path) else {
            continue;
        };
        if record.owner_id == owner_id && valid_pending_record(&path, directory, &record) {
            remove_private_record(&path, identity);
        }
    }
}

pub(super) fn remove_private_record_by_path(path: &Path) {
    let Ok(Some(identity)) = current_file_identity(path) else {
        return;
    };
    remove_private_record(path, identity);
}

pub(super) fn remove_private_record(path: &Path, opened: FileIdentity) {
    if current_file_identity(path).ok().flatten() == Some(opened) {
        let _ = fs::remove_file(path);
    }
}

fn open_checked_file(path: &Path, read: bool, write: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(read)
        .write(write)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    if !private_regular_file(&file.metadata()?) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "local approval record is not a regular file",
        ));
    }
    Ok(file)
}

fn current_file_identity(path: &Path) -> io::Result<Option<FileIdentity>> {
    match open_checked_file(path, true, false) {
        Ok(file) => file_identity(&file).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn file_identity(file: &File) -> io::Result<FileIdentity> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the file handle is valid and the output structure is writable.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileIdentity {
        volume_serial: information.dwVolumeSerialNumber,
        file_index: (u64::from(information.nFileIndexHigh) << 32)
            | u64::from(information.nFileIndexLow),
    })
}

fn private_regular_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
}

fn valid_owner_id(owner_id: &str) -> bool {
    owner_id.starts_with("o-")
        && owner_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}
