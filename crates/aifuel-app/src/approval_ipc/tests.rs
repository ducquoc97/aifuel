use super::protocol::{OwnerRecord, PendingApprovalRecord};
use super::records::{
    canonical_socket_root, next_owner_id, owner_record_path, owner_socket_directory,
    owner_socket_path, pending_record_path, socket_path_fits, write_owner_record,
    write_pending_record,
};
use super::{LocalApprovalDecision, LocalApprovalServer, submit_local_approval};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn test_directory(label: &str) -> PathBuf {
    let nonce = next_owner_id().expect("create unpredictable test token");
    let directory =
        std::env::temp_dir().join(format!("aifuel-ap-{label}-{}-{nonce}", std::process::id(),));
    fs::create_dir(&directory).expect("create test directory");
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
        .expect("protect test directory");
    directory
}

#[test]
fn pending_mapping_routes_directly_to_its_owner() {
    let directory = test_directory("routing");
    let non_owner_calls = Arc::new(AtomicUsize::new(0));
    let owner_calls = Arc::new(AtomicUsize::new(0));
    let non_owner_calls_for_server = Arc::clone(&non_owner_calls);
    let non_owner = LocalApprovalServer::start(&directory, move |_, _, _| {
        non_owner_calls_for_server.fetch_add(1, Ordering::Relaxed);
        Ok(false)
    })
    .expect("start non-owner");
    let owner_calls_for_server = Arc::clone(&owner_calls);
    let owner = LocalApprovalServer::start(&directory, move |run_id, input_id, _| {
        if run_id == "run-owned-by-second" && input_id == "pending-approval" {
            owner_calls_for_server.fetch_add(1, Ordering::Relaxed);
            Ok(true)
        } else {
            Ok(false)
        }
    })
    .expect("start owner");
    owner
        .register_pending("run-owned-by-second", "pending-approval")
        .expect("register the pending owner mapping");

    submit_local_approval(
        &directory,
        "run-owned-by-second",
        "pending-approval",
        LocalApprovalDecision::Accept,
    )
    .expect("route to the matching owner");

    assert_eq!(non_owner_calls.load(Ordering::Relaxed), 0);
    assert_eq!(owner_calls.load(Ordering::Relaxed), 1);
    drop(owner);
    drop(non_owner);
    fs::remove_dir_all(directory).expect("remove test directory");
}

#[test]
fn removes_stale_pending_mapping_under_the_owner_lock() {
    use std::os::unix::net::UnixListener;

    let directory = test_directory("stale");
    let socket_root = canonical_socket_root().expect("resolve socket root");
    let stale_id = next_owner_id().expect("create unpredictable owner token");
    let stale_socket_directory = owner_socket_directory(&socket_root, &stale_id);
    fs::create_dir(&stale_socket_directory).expect("create stale socket directory");
    fs::set_permissions(&stale_socket_directory, fs::Permissions::from_mode(0o700))
        .expect("protect stale socket directory");
    let stale_socket = owner_socket_path(&socket_root, &stale_id);
    let stale_record = owner_record_path(&directory, &stale_id);
    let stale_listener = UnixListener::bind(&stale_socket).expect("create stale socket");
    fs::set_permissions(&stale_socket, fs::Permissions::from_mode(0o600))
        .expect("protect stale socket");
    let stale_owner_file = write_owner_record(
        &stale_record,
        &OwnerRecord {
            owner_id: stale_id.clone(),
            socket: stale_socket.clone(),
        },
    )
    .expect("write stale record");
    let stale_mapping = pending_record_path(&directory, "stale-run", "stale-input");
    write_pending_record(
        &stale_mapping,
        &PendingApprovalRecord {
            owner_id: stale_id.clone(),
            socket: stale_socket.clone(),
            run_id: "stale-run".to_owned(),
            input_id: "stale-input".to_owned(),
        },
        &stale_id,
    )
    .expect("write pending mapping");
    drop(stale_owner_file);
    drop(stale_listener);

    let result = submit_local_approval(
        &directory,
        "stale-run",
        "stale-input",
        LocalApprovalDecision::Decline,
    );

    assert!(result.is_err());
    assert!(!stale_record.exists());
    assert!(!stale_mapping.exists());
    assert!(!stale_socket.exists());
    assert!(!stale_socket_directory.exists());
    fs::remove_dir_all(directory).expect("remove test directory");
}

#[test]
fn uses_short_private_socket_path_for_long_config_directory() {
    let root = test_directory("long-config");
    let long_component = "long-approval-directory-".repeat(6);
    let directory = root.join(long_component);
    fs::create_dir_all(&directory).expect("create long config path");
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
        .expect("protect long config path");

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_server = Arc::clone(&calls);
    let server = LocalApprovalServer::start(&directory, move |_, _, _| {
        calls_for_server.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    })
    .expect("start owner despite long config path");
    assert_eq!(
        canonical_socket_root().expect("resolve canonical socket root"),
        fs::canonicalize("/tmp").expect("canonicalize /tmp")
    );
    assert!(socket_path_fits(&server.socket_path));
    server
        .register_pending("long-path-run", "long-path-input")
        .expect("register the pending request");

    submit_local_approval(
        &directory,
        "long-path-run",
        "long-path-input",
        LocalApprovalDecision::Accept,
    )
    .expect("deliver through short socket path");
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    drop(server);
    fs::remove_dir_all(root).expect("remove test directory");
}
