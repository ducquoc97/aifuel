use super::*;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_FIRST_PIPE_INSTANCE;
use windows_sys::Win32::System::Pipes::PIPE_REJECT_REMOTE_CLIENTS;

fn test_directory(label: &str) -> PathBuf {
    let nonce = security::next_owner_id().expect("create unpredictable test token");
    let directory =
        std::env::temp_dir().join(format!("aifuel-ap-{label}-{}-{nonce}", std::process::id()));
    fs::create_dir_all(&directory).expect("create test directory");
    directory
}

#[test]
fn pending_mapping_routes_to_its_owner_and_drop_cleans_pipe_and_records() {
    let directory = test_directory("routing");
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_server = Arc::clone(&calls);
    let server = LocalApprovalServer::start(&directory, move |run_id, input_id, _| {
        if run_id == "run-owned-by-this-process" && input_id == "pending-approval" {
            calls_for_server.fetch_add(1, Ordering::Relaxed);
            Ok(true)
        } else {
            Ok(false)
        }
    })
    .expect("start private named-pipe owner");
    let owner_record = server.owner_record.clone();
    let pipe_name = server.pipe_name.clone();
    server
        .register_pending("run-owned-by-this-process", "pending-approval")
        .expect("register pending owner mapping");
    let pending =
        records::pending_record_path(&directory, "run-owned-by-this-process", "pending-approval");

    submit_local_approval(
        &directory,
        "run-owned-by-this-process",
        "pending-approval",
        LocalApprovalDecision::Accept,
    )
    .expect("deliver over local named pipe");
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(pipe_name.starts_with(PIPE_PREFIX));
    assert!(security::private_dacl_matches(&directory, true).expect("read directory DACL"));

    drop(server);
    assert!(!owner_record.exists());
    assert!(!pending.exists());
    assert!(security::private_dacl_matches(&directory, true).expect("read directory DACL"));
    fs::remove_dir_all(directory).expect("remove test directory");
}

#[test]
fn private_pipe_descriptor_is_protected_and_does_not_use_a_default_acl() {
    let descriptor =
        security::private_security_descriptor(false).expect("build explicit user DACL");
    assert!(!descriptor.as_ptr().is_null());
    let sddl = String::from_utf16(&descriptor.sddl()[..descriptor.sddl().len() - 1])
        .expect("expected descriptor is UTF-16");
    assert!(sddl.starts_with("D:P(A;;GA;;;S-1-"));
    assert!(sddl.contains("(A;;0x00000000;;;S-1-3-4)"));
    assert!(!sddl.contains(";;;WD)"));
    assert!(!sddl.contains(";;;AN)"));
    assert_ne!(pipe::SERVER_PIPE_MODE & PIPE_REJECT_REMOTE_CLIENTS, 0);
    assert_ne!(
        pipe::SERVER_PIPE_OPEN_MODE & FILE_FLAG_FIRST_PIPE_INSTANCE,
        0
    );
}

#[test]
fn rejects_a_reparse_point_as_the_owner_directory() {
    let directory = test_directory("reparse");
    let link = directory.with_extension("link");
    std::os::windows::fs::symlink_dir(&directory, &link).expect("create directory symbolic link");
    let result = security::prepare_private_directory(&link);
    assert!(result.is_err());
    fs::remove_dir_all(directory).expect("remove test directory");
    let _ = fs::remove_dir(link);
}
