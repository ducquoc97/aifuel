//! Same-user local approval delivery over a Windows named pipe.

#[path = "approval_ipc_windows/pipe.rs"]
mod pipe;
#[path = "approval_ipc_windows/records.rs"]
mod records;
#[path = "approval_ipc_windows/security.rs"]
mod security;
#[cfg(test)]
#[path = "approval_ipc_windows/tests.rs"]
mod tests;

use crate::approval_ipc_protocol::ApprovalMessage;
pub use crate::approval_ipc_protocol::LocalApprovalDecision;
use std::fs::File;
use std::io;
use std::os::windows::io::OwnedHandle;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

pub(super) const PIPE_PREFIX: &str = r"\\.\pipe\aifuel-approval-";

pub(crate) struct LocalApprovalServer {
    owner_id: String,
    directory: PathBuf,
    pipe_name: String,
    owner_record: PathBuf,
    // Keeping this descriptor open keeps the separate cross-process owner lock.
    owner_lock: Option<File>,
    pipe_handle: Option<Arc<OwnedHandle>>,
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
        let directory = security::prepare_private_directory(directory)?;
        let owner_id = security::next_owner_id()?;
        let pipe_name = records::pipe_name(&owner_id);
        let pipe_handle = pipe::create_server_pipe(&pipe_name)?;
        let owner_record = records::owner_record_path(&directory, &owner_id);
        let owner_lock = records::write_owner_record(
            &owner_record,
            &records::OwnerRecord {
                owner_id: owner_id.clone(),
                pipe_name: pipe_name.clone(),
            },
        )?;

        let stop = Arc::new(AtomicBool::new(false));
        let worker = match pipe::spawn_server(
            Arc::clone(&pipe_handle),
            owner_id.clone(),
            Arc::clone(&stop),
            Arc::new(handler),
        ) {
            Ok(worker) => worker,
            Err(error) => {
                records::remove_private_record_by_path(&owner_record);
                drop(owner_lock);
                records::remove_private_record_by_path(&records::owner_lock_path(&owner_record));
                return Err(error);
            }
        };

        Ok(Self {
            owner_id,
            directory,
            pipe_name,
            owner_record,
            owner_lock: Some(owner_lock),
            pipe_handle: Some(pipe_handle),
            stop,
            worker: Some(worker),
        })
    }

    pub(crate) fn register_pending(&self, run_id: &str, input_id: &str) -> io::Result<()> {
        let path = records::pending_record_path(&self.directory, run_id, input_id);
        records::write_pending_record(
            &path,
            &records::PendingApprovalRecord {
                owner_id: self.owner_id.clone(),
                pipe_name: self.pipe_name.clone(),
                run_id: run_id.to_owned(),
                input_id: input_id.to_owned(),
            },
            &self.owner_id,
        )
    }

    pub(crate) fn remove_pending(&self, run_id: &str, input_id: &str) {
        let path = records::pending_record_path(&self.directory, run_id, input_id);
        let Some((record, identity)) = records::read_pending_record(&path).ok().flatten() else {
            return;
        };
        if record.owner_id == self.owner_id
            && record.run_id == run_id
            && record.input_id == input_id
            && records::valid_pending_record(&path, &self.directory, &record)
        {
            records::remove_private_record(&path, identity);
        }
    }
}

impl Drop for LocalApprovalServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(pipe) = self.pipe_handle.as_ref() {
            pipe::cancel_server(pipe);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        // Close the server handle before removing its discovery records.
        self.pipe_handle.take();
        records::remove_pending_records_for_owner(&self.directory, &self.owner_id);
        records::remove_private_record_by_path(&self.owner_record);
        self.owner_lock.take();
        records::remove_private_record_by_path(&records::owner_lock_path(&self.owner_record));
    }
}

pub fn submit_local_approval(
    directory: &Path,
    run_id: &str,
    input_id: &str,
    decision: LocalApprovalDecision,
) -> Result<(), String> {
    let directory = security::canonical_private_directory(directory)?;
    let path = records::pending_record_path(&directory, run_id, input_id);
    let (pending, pending_identity) = records::read_pending_record(&path)
        .map_err(|error| format!("could not read pending approval mapping: {error}"))?
        .ok_or_else(|| "no pending permission approval matches this run and input".to_owned())?;
    if !records::valid_pending_record(&path, &directory, &pending)
        || pending.run_id != run_id
        || pending.input_id != input_id
    {
        return Err("pending permission approval mapping is invalid".to_owned());
    }

    let owner_path = records::owner_record_path(&directory, &pending.owner_id);
    let owner = match records::active_owner_record(&owner_path, &directory) {
        Ok(Some(owner)) => owner,
        Ok(None) => {
            records::remove_private_record(&path, pending_identity);
            return Err("the execution owner for this approval is no longer live".to_owned());
        }
        Err(error) => return Err(format!("could not validate the approval owner: {error}")),
    };
    if owner.owner_id != pending.owner_id || owner.pipe_name != pending.pipe_name {
        return Err("pending approval owner identity does not match".to_owned());
    }

    let reply = pipe::exchange(
        &owner.pipe_name,
        &ApprovalMessage {
            owner_id: owner.owner_id,
            run_id: run_id.to_owned(),
            input_id: input_id.to_owned(),
            decision,
        },
    )
    .map_err(|error| format!("could not exchange local approval: {error}"))?;
    if !reply.owner_found {
        return Err("the pending permission approval is no longer available".to_owned());
    }
    if reply.accepted {
        Ok(())
    } else {
        Err(reply
            .message
            .unwrap_or_else(|| "local approval was rejected".to_owned()))
    }
}
