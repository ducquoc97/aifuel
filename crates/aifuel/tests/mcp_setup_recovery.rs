mod common;

use common::{TestDirectory, ai_fuel_config_dir, backup_files, backup_path, codex_home, run_setup};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

fn ownership_receipt(config_dir: &Path) -> PathBuf {
    let receipts = config_dir.join("mcp-registrations").join("receipts");
    fs::read_dir(receipts)
        .expect("ownership receipt directory should exist")
        .next()
        .expect("AI Fuel should have written a receipt")
        .expect("receipt entry should be readable")
        .path()
}

fn write_interrupted_transaction(
    config_dir: &Path,
    config_file: &Path,
    receipt_file: &Path,
    base: &[u8],
    target: &[u8],
    backup: &Path,
) -> PathBuf {
    let receipt: Value = serde_json::from_slice(&fs::read(receipt_file).unwrap()).unwrap();
    let journal = config_dir
        .join("mcp-registrations")
        .join("pending")
        .join(receipt_file.file_name().unwrap());
    fs::create_dir_all(journal.parent().unwrap()).unwrap();
    fs::write(
        &journal,
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "host_id": "codex",
            "config_file": config_file.to_string_lossy(),
            "entry_name": "aifuel-gateway",
            "base_sha256": sha256_hex(base),
            "target_sha256": sha256_hex(target),
            "backup_file": backup.to_string_lossy(),
            "previous_receipt_entry": null,
            "entry": receipt["entry"]
        }))
        .unwrap(),
    )
    .unwrap();
    journal
}

fn sha256_hex(contents: &[u8]) -> String {
    let digest = Sha256::digest(contents);
    let mut result = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut result, "{byte:02x}").unwrap();
    }
    result
}

#[test]
fn public_setup_recovers_when_config_replacement_was_interrupted() {
    let directory = TestDirectory::new("mcp-setup-interrupted-config");
    let root = directory.path();
    let config_dir = ai_fuel_config_dir(root);
    let config = codex_home(root).join("config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let base = b"model = \"gpt-5\"\n";
    fs::write(&config, base).unwrap();
    let completed = run_setup(root, &["mcp", "setup", "--agent", "codex"]);
    assert!(completed.status.success());
    let target = fs::read(&config).unwrap();
    let receipt = ownership_receipt(&config_dir);
    let backup = backup_path(&completed);
    let journal =
        write_interrupted_transaction(&config_dir, &config, &receipt, base, &target, &backup);

    fs::write(&config, base).unwrap();
    fs::remove_file(&receipt).unwrap();

    let retry = run_setup(root, &["mcp", "setup", "--agent", "codex"]);

    assert!(
        retry.status.success(),
        "recovery should retry setup: {}",
        String::from_utf8_lossy(&retry.stderr)
    );
    assert!(String::from_utf8_lossy(&retry.stdout).contains("registration applied"));
    assert!(
        fs::read_to_string(&config)
            .unwrap()
            .contains("[mcp_servers.aifuel-gateway]")
    );
    assert!(ownership_receipt(&config_dir).is_file());
    assert!(!journal.exists());
    assert_eq!(fs::read(&backup).unwrap(), base);
}

#[test]
fn public_setup_recovers_a_missing_receipt_before_owned_removal() {
    let directory = TestDirectory::new("mcp-setup-interrupted-receipt");
    let root = directory.path();
    let config_dir = ai_fuel_config_dir(root);
    let config = codex_home(root).join("config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let base = b"model = \"gpt-5\"\n";
    fs::write(&config, base).unwrap();
    let completed = run_setup(root, &["mcp", "setup", "--agent", "codex"]);
    assert!(completed.status.success());
    let target = fs::read(&config).unwrap();
    let receipt = ownership_receipt(&config_dir);
    let backup = backup_path(&completed);
    let journal =
        write_interrupted_transaction(&config_dir, &config, &receipt, base, &target, &backup);
    fs::remove_file(&receipt).unwrap();

    let remove = run_setup(root, &["mcp", "setup", "--agent", "codex", "--remove"]);

    assert!(
        remove.status.success(),
        "recovered removal should succeed: {}",
        String::from_utf8_lossy(&remove.stderr)
    );
    assert!(String::from_utf8_lossy(&remove.stdout).contains("registration removed"));
    assert!(
        fs::read_to_string(&config)
            .unwrap()
            .contains("model = \"gpt-5\"")
    );
    assert!(
        !fs::read_to_string(&config)
            .unwrap()
            .contains("[mcp_servers.aifuel-gateway]")
    );
    assert!(!journal.exists());
    assert!(!receipt.exists());
    assert!(
        fs::read_to_string(&backup)
            .unwrap()
            .contains("[mcp_servers.aifuel-gateway]")
    );
}

#[test]
fn public_setup_recovers_an_interrupted_receipt_update() {
    let directory = TestDirectory::new("mcp-setup-interrupted-receipt-update");
    let root = directory.path();
    let config_dir = ai_fuel_config_dir(root);
    let config = codex_home(root).join("config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "model = \"gpt-5\"\n").unwrap();
    let initial = run_setup(root, &["mcp", "setup", "--agent", "codex"]);
    assert!(initial.status.success());

    let receipt = ownership_receipt(&config_dir);
    let mut previous_receipt: Value = serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
    let previous_entry = previous_receipt["entry"].clone();
    let target_config = fs::read_to_string(&config).unwrap();
    let previous_command = "previous-gateway-command";
    let mut base_config: toml_edit::DocumentMut = target_config
        .parse()
        .expect("setup should write valid Codex TOML");
    base_config["mcp_servers"]["aifuel-gateway"]["command"] = toml_edit::value(previous_command);
    let base = base_config.to_string().into_bytes();
    assert_ne!(base, target_config.as_bytes());
    let mut previous_entry = previous_entry;
    previous_entry["value"]["command"]["value"] = json!(previous_command);
    previous_receipt["entry"] = previous_entry.clone();
    fs::write(&config, &base).unwrap();
    fs::write(
        &receipt,
        serde_json::to_vec_pretty(&previous_receipt).unwrap(),
    )
    .unwrap();

    let update = run_setup(root, &["mcp", "setup", "--agent", "codex"]);
    assert!(
        update.status.success(),
        "setup should update the owned entry: {}",
        String::from_utf8_lossy(&update.stderr)
    );
    let target = fs::read(&config).unwrap();
    let backup = backup_path(&update);
    let journal =
        write_interrupted_transaction(&config_dir, &config, &receipt, &base, &target, &backup);
    let mut transaction: Value = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    transaction["previous_receipt_entry"] = previous_entry.clone();
    fs::write(&journal, serde_json::to_vec_pretty(&transaction).unwrap()).unwrap();
    let mut target_receipt: Value = serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
    let target_entry = target_receipt["entry"].clone();
    target_receipt["entry"] = previous_entry;
    fs::write(
        &receipt,
        serde_json::to_vec_pretty(&target_receipt).unwrap(),
    )
    .unwrap();

    let recovery = run_setup(root, &["mcp", "setup", "--agent", "codex"]);

    assert!(
        recovery.status.success(),
        "recovery should finish the receipt update: {}",
        String::from_utf8_lossy(&recovery.stderr)
    );
    assert!(
        String::from_utf8_lossy(&recovery.stdout).contains("no configuration change was needed")
    );
    assert_eq!(fs::read(&config).unwrap(), target);
    let recovered_receipt: Value = serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
    assert_eq!(recovered_receipt["entry"], target_entry);
    assert!(!journal.exists());
    assert_eq!(fs::read(&backup).unwrap(), base);
}

#[test]
fn public_setup_conflicts_when_current_config_was_edited_during_recovery() {
    let directory = TestDirectory::new("mcp-setup-recovery-conflict");
    let root = directory.path();
    let config_dir = ai_fuel_config_dir(root);
    let config = codex_home(root).join("config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let base = b"model = \"gpt-5\"\n";
    fs::write(&config, base).unwrap();
    let completed = run_setup(root, &["mcp", "setup", "--agent", "codex"]);
    assert!(completed.status.success());
    let target = fs::read(&config).unwrap();
    let receipt = ownership_receipt(&config_dir);
    let backup = backup_path(&completed);
    let journal =
        write_interrupted_transaction(&config_dir, &config, &receipt, base, &target, &backup);
    fs::remove_file(&receipt).unwrap();
    let edited = String::from_utf8(target)
        .unwrap()
        .replace("\"--agent\", \"codex\"", "\"--agent\", \"user-edit\"");
    fs::write(&config, edited.as_bytes()).unwrap();

    let remove = run_setup(root, &["mcp", "setup", "--agent", "codex", "--remove"]);

    assert_eq!(remove.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&remove.stderr).contains("did not assume ownership"));
    assert_eq!(fs::read(&config).unwrap(), edited.as_bytes());
    assert!(journal.is_file());
    assert!(!receipt.exists());
    assert_eq!(fs::read(&backup).unwrap(), base);
}

#[test]
fn public_setup_refuses_recovery_when_the_private_backup_changed() {
    let directory = TestDirectory::new("mcp-setup-recovery-backup-conflict");
    let root = directory.path();
    let config_dir = ai_fuel_config_dir(root);
    let config = codex_home(root).join("config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let base = b"model = \"gpt-5\"\n";
    fs::write(&config, base).unwrap();
    let completed = run_setup(root, &["mcp", "setup", "--agent", "codex"]);
    assert!(completed.status.success());
    let target = fs::read(&config).unwrap();
    let receipt = ownership_receipt(&config_dir);
    let backup = backup_path(&completed);
    let journal =
        write_interrupted_transaction(&config_dir, &config, &receipt, base, &target, &backup);
    fs::remove_file(&receipt).unwrap();
    fs::write(&backup, b"damaged backup").unwrap();
    let before_recovery = fs::read(&config).unwrap();

    let remove = run_setup(root, &["mcp", "setup", "--agent", "codex", "--remove"]);

    assert_eq!(remove.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&remove.stderr).contains("did not assume ownership"));
    assert_eq!(fs::read(&config).unwrap(), before_recovery);
    assert!(journal.is_file());
    assert!(!receipt.exists());
}

#[test]
fn public_setup_preserves_config_when_receipt_changed_during_recovery() {
    let directory = TestDirectory::new("mcp-setup-recovery-receipt-conflict");
    let root = directory.path();
    let config_dir = ai_fuel_config_dir(root);
    let config = codex_home(root).join("config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let base = b"model = \"gpt-5\"\n";
    fs::write(&config, base).unwrap();
    let completed = run_setup(root, &["mcp", "setup", "--agent", "codex"]);
    assert!(completed.status.success());
    let target = fs::read(&config).unwrap();
    let receipt = ownership_receipt(&config_dir);
    let backup = backup_path(&completed);
    let journal =
        write_interrupted_transaction(&config_dir, &config, &receipt, base, &target, &backup);
    let mut edited_receipt: Value = serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
    edited_receipt["entry"] = json!({"command": "user-edited-receipt", "args": []});
    fs::write(
        &receipt,
        serde_json::to_vec_pretty(&edited_receipt).unwrap(),
    )
    .unwrap();
    let before_recovery = fs::read(&config).unwrap();
    let before_receipt = fs::read(&receipt).unwrap();

    let remove = run_setup(root, &["mcp", "setup", "--agent", "codex", "--remove"]);

    assert_eq!(remove.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&remove.stderr).contains("did not assume ownership"));
    assert_eq!(fs::read(&config).unwrap(), before_recovery);
    assert_eq!(fs::read(&receipt).unwrap(), before_receipt);
    assert!(journal.is_file());
}

#[cfg(unix)]
#[test]
fn public_setup_recovers_after_config_replacement_but_receipt_write_failed() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TestDirectory::new("mcp-setup-receipt-write-failure");
    let root = directory.path();
    let config_dir = ai_fuel_config_dir(root);
    let config = codex_home(root).join("config.toml");
    let base = b"model = \"gpt-5\"\n";
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, base).unwrap();
    let receipts = config_dir.join("mcp-registrations").join("receipts");
    fs::create_dir_all(&receipts).unwrap();
    fs::set_permissions(&receipts, fs::Permissions::from_mode(0o500)).unwrap();

    let interrupted = run_setup(root, &["mcp", "setup", "--agent", "codex"]);

    assert_eq!(interrupted.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&interrupted.stderr)
            .contains("could not save its ownership receipt")
    );
    let replaced_config = fs::read(&config).unwrap();
    assert!(String::from_utf8_lossy(&replaced_config).contains("[mcp_servers.aifuel-gateway]"));
    assert!(fs::read_dir(&receipts).unwrap().next().is_none());
    let pending_dir = config_dir.join("mcp-registrations").join("pending");
    let pending = fs::read_dir(&pending_dir)
        .unwrap()
        .next()
        .expect("the interrupted setup should leave its recovery journal")
        .unwrap()
        .path();
    assert_eq!(
        fs::metadata(&pending).unwrap().permissions().mode() & 0o077,
        0
    );
    assert_eq!(
        fs::metadata(&pending_dir).unwrap().permissions().mode() & 0o077,
        0
    );
    let backup = backup_files(&config_dir)
        .pop()
        .expect("the interrupted setup should retain its private backup");
    assert_eq!(fs::read(&backup).unwrap(), base);

    fs::set_permissions(&receipts, fs::Permissions::from_mode(0o700)).unwrap();
    let recovered = run_setup(root, &["mcp", "setup", "--agent", "codex"]);

    assert!(
        recovered.status.success(),
        "recovery should finish receipt creation: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(
        String::from_utf8_lossy(&recovered.stdout).contains("no configuration change was needed")
    );
    assert!(ownership_receipt(&config_dir).is_file());
    assert!(!pending.exists());
    assert_eq!(fs::read(&config).unwrap(), replaced_config);

    let remove = run_setup(root, &["mcp", "setup", "--agent", "codex", "--remove"]);
    assert!(
        remove.status.success(),
        "recovered ownership should permit removal: {}",
        String::from_utf8_lossy(&remove.stderr)
    );
    assert!(
        !fs::read_to_string(&config)
            .unwrap()
            .contains("[mcp_servers.aifuel-gateway]")
    );
}
