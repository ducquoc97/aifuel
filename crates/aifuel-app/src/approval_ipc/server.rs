use super::protocol::{OwnerRecord, PendingApprovalRecord};
use super::records::{
    canonical_socket_root, next_owner_id, owner_record_path, owner_socket_directory,
    owner_socket_path, pending_record_path, read_pending_record, remove_pending_records_for_owner,
    remove_private_record, socket_path_fits, valid_pending_record, write_owner_record,
    write_pending_record,
};
use crate::approval_ipc_protocol::{ApprovalMessage, ApprovalReply, LocalApprovalDecision};
use std::fs::{self, File};
use std::io::{self, BufRead, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

pub(crate) struct LocalApprovalServer {
    owner_id: String,
    directory: PathBuf,
    socket_root: PathBuf,
    pub(super) socket_path: PathBuf,
    owner_record: PathBuf,
    // Keeping this descriptor open keeps the cross-process owner lock held.
    _owner_file: File,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl LocalApprovalServer {
    pub(crate) fn start(
        directory: &Path,
        handler: impl Fn(String, String, LocalApprovalDecision) -> Result<bool, String>
        + Send
        + Sync
        + 'static,
    ) -> io::Result<Self> {
        use std::os::unix::net::UnixListener;

        fs::create_dir_all(directory)?;
        let directory_metadata = fs::symlink_metadata(directory)?;
        if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "local approval directory must be a real directory",
            ));
        }
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        let directory = fs::canonicalize(directory)?;
        let socket_root = canonical_socket_root()?;
        let owner_id = next_owner_id()?;
        let socket_directory = owner_socket_directory(&socket_root, &owner_id);
        let socket_path = owner_socket_path(&socket_root, &owner_id);
        if !socket_path_fits(&socket_path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "local approval socket path exceeds the platform Unix-socket limit",
            ));
        }
        fs::create_dir(&socket_directory)?;
        fs::set_permissions(&socket_directory, fs::Permissions::from_mode(0o700))?;
        let owner_record = owner_record_path(&directory, &owner_id);
        let listener = match UnixListener::bind(&socket_path) {
            Ok(listener) => listener,
            Err(error) => {
                let _ = fs::remove_dir(&socket_directory);
                return Err(error);
            }
        };
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        let owner_file = write_owner_record(
            &owner_record,
            &OwnerRecord {
                owner_id: owner_id.clone(),
                socket: socket_path.clone(),
            },
        )?;
        listener.set_nonblocking(true)?;

        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let owner_for_worker = owner_id.clone();
        let handler = Arc::new(handler);
        let worker = thread::Builder::new()
            .name("aifuel-local-approval".to_owned())
            .spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => handle_connection(stream, &owner_for_worker, &handler),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(std::time::Duration::from_millis(20));
                        }
                        Err(_) => break,
                    }
                }
            })?;
        Ok(Self {
            owner_id,
            directory,
            socket_root,
            socket_path,
            owner_record,
            _owner_file: owner_file,
            stop,
            worker: Some(worker),
        })
    }

    pub(crate) fn register_pending(&self, run_id: &str, input_id: &str) -> io::Result<()> {
        let path = pending_record_path(&self.directory, run_id, input_id);
        if path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a local approval is already pending for this run input",
            ));
        }
        write_pending_record(
            &path,
            &PendingApprovalRecord {
                owner_id: self.owner_id.clone(),
                socket: self.socket_path.clone(),
                run_id: run_id.to_owned(),
                input_id: input_id.to_owned(),
            },
            &self.owner_id,
        )
    }

    pub(crate) fn remove_pending(&self, run_id: &str, input_id: &str) {
        let path = pending_record_path(&self.directory, run_id, input_id);
        let Some((record, metadata)) = read_pending_record(&path).ok().flatten() else {
            return;
        };
        if record.owner_id == self.owner_id
            && record.run_id == run_id
            && record.input_id == input_id
            && valid_pending_record(&path, &self.directory, &self.socket_root, &record)
        {
            remove_private_record(&path, &metadata);
        }
    }
}

impl Drop for LocalApprovalServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        // The record stays locked until this value is dropped, so another
        // process cannot mistake a shutting-down owner for a stale one.
        remove_pending_records_for_owner(&self.directory, &self.socket_root, &self.owner_id);
        let _ = fs::remove_file(&self.owner_record);
        let _ = fs::remove_file(&self.socket_path);
        if let Some(socket_directory) = self.socket_path.parent() {
            let _ = fs::remove_dir(socket_directory);
        }
    }
}

fn handle_connection(
    stream: std::os::unix::net::UnixStream,
    owner_id: &str,
    handler: &Arc<impl Fn(String, String, LocalApprovalDecision) -> Result<bool, String>>,
) {
    let timeout = Some(std::time::Duration::from_secs(10));
    if stream.set_read_timeout(timeout).is_err() || stream.set_write_timeout(timeout).is_err() {
        return;
    }
    let mut reader = io::BufReader::new(stream);
    let mut line = String::new();
    let reply = match (&mut reader).take(64 * 1024).read_line(&mut line) {
        Ok(count) if count > 0 && count < 64 * 1024 => {
            match serde_json::from_str::<ApprovalMessage>(&line) {
                Ok(message) if message.owner_id == owner_id => {
                    match handler(message.run_id, message.input_id, message.decision) {
                        Ok(false) => ApprovalReply {
                            owner_found: false,
                            accepted: false,
                            message: None,
                        },
                        Ok(true) => ApprovalReply {
                            owner_found: true,
                            accepted: true,
                            message: None,
                        },
                        Err(error) => ApprovalReply {
                            owner_found: true,
                            accepted: false,
                            message: Some(error),
                        },
                    }
                }
                Ok(_) => ApprovalReply {
                    owner_found: false,
                    accepted: false,
                    message: None,
                },
                Err(_) => ApprovalReply {
                    owner_found: false,
                    accepted: false,
                    message: Some("invalid local approval request".to_owned()),
                },
            }
        }
        _ => ApprovalReply {
            owner_found: false,
            accepted: false,
            message: Some("local approval request was empty or too large".to_owned()),
        },
    };
    let _ = serde_json::to_writer(reader.get_mut(), &reply);
    let _ = reader.get_mut().write_all(b"\n");
    let _ = reader.get_mut().flush();
}
