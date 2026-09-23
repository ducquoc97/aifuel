use super::records::{
    active_owner_record, canonical_private_directory, canonical_socket_root, owner_record_path,
    pending_record_path, read_pending_record, remove_private_record, valid_pending_record,
    valid_socket_file,
};
use crate::approval_ipc_protocol::{ApprovalMessage, ApprovalReply, LocalApprovalDecision};
use std::io::{self, BufRead, Write};
use std::path::Path;

pub fn submit_local_approval(
    directory: &Path,
    run_id: &str,
    input_id: &str,
    decision: LocalApprovalDecision,
) -> Result<(), String> {
    use std::os::unix::net::UnixStream;

    let directory = canonical_private_directory(directory)?;
    let socket_root = canonical_socket_root()
        .map_err(|error| format!("could not resolve local approval socket directory: {error}"))?;
    let path = pending_record_path(&directory, run_id, input_id);
    let (pending, pending_metadata) = read_pending_record(&path)
        .map_err(|error| format!("could not read pending approval mapping: {error}"))?
        .ok_or_else(|| "no pending permission approval matches this run and input".to_owned())?;
    if !valid_pending_record(&path, &directory, &socket_root, &pending)
        || pending.run_id != run_id
        || pending.input_id != input_id
    {
        return Err("pending permission approval mapping is invalid".to_owned());
    }

    let owner_path = owner_record_path(&directory, &pending.owner_id);
    let owner = match active_owner_record(&owner_path, &directory, &socket_root) {
        Ok(Some(owner)) => owner,
        Ok(None) => {
            remove_private_record(&path, &pending_metadata);
            return Err("the execution owner for this approval is no longer live".to_owned());
        }
        Err(error) => return Err(format!("could not validate the approval owner: {error}")),
    };
    if owner.owner_id != pending.owner_id || owner.socket != pending.socket {
        return Err("pending approval owner identity does not match".to_owned());
    }
    if !valid_socket_file(&owner.socket) {
        return Err("owner socket is missing or is not a private Unix socket".to_owned());
    }

    let mut stream = UnixStream::connect(&owner.socket)
        .map_err(|error| format!("could not connect to the approval owner: {error}"))?;
    let timeout = Some(std::time::Duration::from_secs(10));
    stream
        .set_read_timeout(timeout)
        .map_err(|error| format!("could not bound approval response wait: {error}"))?;
    stream
        .set_write_timeout(timeout)
        .map_err(|error| format!("could not bound approval request write: {error}"))?;
    let message = ApprovalMessage {
        owner_id: owner.owner_id,
        run_id: run_id.to_owned(),
        input_id: input_id.to_owned(),
        decision,
    };
    serde_json::to_writer(&mut stream, &message).map_err(|error| error.to_string())?;
    stream.write_all(b"\n").map_err(|error| error.to_string())?;
    stream.flush().map_err(|error| error.to_string())?;
    let mut reply_line = String::new();
    io::BufReader::new(stream)
        .read_line(&mut reply_line)
        .map_err(|error| error.to_string())?;
    let reply: ApprovalReply =
        serde_json::from_str(&reply_line).map_err(|error| error.to_string())?;
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
