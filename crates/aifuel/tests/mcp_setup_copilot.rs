#[allow(dead_code)]
mod common;

use common::{
    TestDirectory, ai_fuel_config_dir, backup_files, backup_path, run_setup_with_copilot_home,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

fn copilot_receipt(config_dir: &Path) -> PathBuf {
    let receipts = config_dir.join("mcp-registrations").join("receipts");
    fs::read_dir(receipts)
        .expect("ownership receipt directory should exist")
        .next()
        .expect("AI Fuel should have written a receipt")
        .expect("receipt entry should be readable")
        .path()
}

fn write_interrupted_copilot_setup(
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
            "host_id": "copilot",
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
fn public_copilot_setup_uses_the_override_and_preserves_user_configuration() {
    let directory = TestDirectory::new("mcp-setup-copilot-public");
    let root = directory.path();
    let copilot_home = root.join("copilot-home");
    fs::create_dir_all(&copilot_home).unwrap();
    let config = copilot_home.join("mcp-config.json");
    let config_dir = ai_fuel_config_dir(root);
    let original = br#"{
  "settings": {"theme": "dark"},
  "mcpServers": {
    "docs": {"type": "local", "command": "docs-server", "args": ["--mode", "local"]}
  }
}"#;
    fs::write(&config, original).unwrap();
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("mcp.json"),
        br#"{"servers":{"private":{"transport":"stdio","command":"fixture","env":{"TOKEN":{"value":"fixture-secret"}}}},"defaults":["private"]}"#,
    )
    .unwrap();

    let preview = run_setup_with_copilot_home(
        root,
        &["mcp", "setup", "--agent", "copilot", "--dry-run"],
        Some(&copilot_home),
    );

    assert!(preview.status.success());
    assert!(String::from_utf8_lossy(&preview.stdout).contains("dry run: would apply"));
    assert_eq!(fs::read(&config).unwrap(), original);
    assert!(!config_dir.join("mcp-registrations").exists());

    let apply = run_setup_with_copilot_home(
        root,
        &["mcp", "setup", "--agent", "copilot"],
        Some(&copilot_home),
    );

    assert!(
        apply.status.success(),
        "Copilot setup should apply: {}",
        String::from_utf8_lossy(&apply.stderr)
    );
    assert!(String::from_utf8_lossy(&apply.stdout).contains("registration applied"));
    let apply_backup = backup_path(&apply);
    assert_eq!(fs::read(&apply_backup).unwrap(), original);
    let updated: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert_eq!(updated["settings"]["theme"], "dark");
    assert_eq!(updated["mcpServers"]["docs"]["command"], "docs-server");
    assert_eq!(updated["mcpServers"]["aifuel-gateway"]["type"], "local");
    assert_eq!(
        updated["mcpServers"]["aifuel-gateway"]["command"],
        env!("CARGO_BIN_EXE_aifuel")
    );
    assert_eq!(
        updated["mcpServers"]["aifuel-gateway"]["args"],
        serde_json::json!(["mcp", "gateway", "--agent", "copilot"])
    );
    assert_eq!(
        updated["mcpServers"]["aifuel-gateway"]["env"]["XDG_CONFIG_HOME"],
        "${XDG_CONFIG_HOME}"
    );
    assert_eq!(
        updated["mcpServers"]["aifuel-gateway"]["tools"],
        serde_json::json!(["*"])
    );
    assert!(!String::from_utf8_lossy(&fs::read(&config).unwrap()).contains("fixture-secret"));

    let repeated = run_setup_with_copilot_home(
        root,
        &["mcp", "setup", "--agent", "copilot"],
        Some(&copilot_home),
    );
    assert!(repeated.status.success());
    assert!(
        String::from_utf8_lossy(&repeated.stdout).contains("no configuration change was needed")
    );
    assert_eq!(backup_files(&config_dir).len(), 1);

    let remove = run_setup_with_copilot_home(
        root,
        &["mcp", "setup", "--agent", "copilot", "--remove"],
        Some(&copilot_home),
    );
    assert!(
        remove.status.success(),
        "Copilot removal should succeed: {}",
        String::from_utf8_lossy(&remove.stderr)
    );
    let removed: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert!(removed["mcpServers"].get("aifuel-gateway").is_none());
    assert_eq!(removed["mcpServers"]["docs"]["command"], "docs-server");
    assert_eq!(removed["settings"]["theme"], "dark");
    assert_eq!(backup_path(&remove), apply_backup);
    assert_eq!(backup_files(&config_dir).len(), 1);
}

#[test]
fn public_copilot_setup_defaults_to_user_dot_copilot_and_rejects_unowned_conflicts() {
    let directory = TestDirectory::new("mcp-setup-copilot-default");
    let root = directory.path();
    let config = root.join(".copilot").join("mcp-config.json");

    let apply = run_setup_with_copilot_home(root, &["mcp", "setup", "--agent", "copilot"], None);

    assert!(
        apply.status.success(),
        "default Copilot setup should apply: {}",
        String::from_utf8_lossy(&apply.stderr)
    );
    let installed: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert_eq!(installed["mcpServers"]["aifuel-gateway"]["type"], "local");

    let conflicting_home = root.join("conflicting-copilot-home");
    fs::create_dir_all(&conflicting_home).unwrap();
    let conflicting_config = conflicting_home.join("mcp-config.json");
    let original = br#"{"mcpServers":{"aifuel-gateway":{"type":"local","command":"user-owned","args":["mcp","gateway","--agent","copilot"]}}}"#;
    fs::write(&conflicting_config, original).unwrap();

    let conflict = run_setup_with_copilot_home(
        root,
        &["mcp", "setup", "--agent", "copilot"],
        Some(&conflicting_home),
    );

    assert_eq!(conflict.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("different or unowned"));
    assert_eq!(fs::read(&conflicting_config).unwrap(), original);
}

#[test]
fn public_copilot_remove_preserves_a_receipt_owned_registration_after_user_edits() {
    let directory = TestDirectory::new("mcp-setup-copilot-edited");
    let root = directory.path();
    let copilot_home = root.join("copilot-home");
    fs::create_dir_all(&copilot_home).unwrap();
    let config = copilot_home.join("mcp-config.json");
    let original = br#"{"settings":{"theme":"dark"},"mcpServers":{"docs":{"type":"local","command":"docs-server"}}}"#;
    fs::write(&config, original).unwrap();

    let apply = run_setup_with_copilot_home(
        root,
        &["mcp", "setup", "--agent", "copilot"],
        Some(&copilot_home),
    );
    assert!(apply.status.success());

    let mut edited: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    edited["settings"]["newer"] = json!("preserve this change");
    edited["mcpServers"]["aifuel-gateway"]["args"] =
        json!(["mcp", "gateway", "--agent", "user-edited"]);
    let edited = serde_json::to_vec_pretty(&edited).unwrap();
    fs::write(&config, &edited).unwrap();

    let remove = run_setup_with_copilot_home(
        root,
        &["mcp", "setup", "--agent", "copilot", "--remove"],
        Some(&copilot_home),
    );

    assert_eq!(remove.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&remove.stderr).contains("edited after AI Fuel created it"));
    assert_eq!(fs::read(&config).unwrap(), edited);
}

#[test]
fn public_copilot_recovery_preserves_newer_config_after_interrupted_receipt_write() {
    let directory = TestDirectory::new("mcp-setup-copilot-recovery-conflict");
    let root = directory.path();
    let copilot_home = root.join("copilot-home");
    fs::create_dir_all(&copilot_home).unwrap();
    let config = copilot_home.join("mcp-config.json");
    let config_dir = ai_fuel_config_dir(root);
    let base = br#"{"settings":{"theme":"dark"},"mcpServers":{"docs":{"type":"local","command":"docs-server"}}}"#;
    fs::write(&config, base).unwrap();

    let apply = run_setup_with_copilot_home(
        root,
        &["mcp", "setup", "--agent", "copilot"],
        Some(&copilot_home),
    );
    assert!(apply.status.success());
    let target = fs::read(&config).unwrap();
    let receipt = copilot_receipt(&config_dir);
    let backup = backup_path(&apply);
    let journal =
        write_interrupted_copilot_setup(&config_dir, &config, &receipt, base, &target, &backup);
    fs::remove_file(&receipt).unwrap();

    let mut newer: Value = serde_json::from_slice(&target).unwrap();
    newer["settings"]["newer"] = json!("do not overwrite");
    let newer = serde_json::to_vec_pretty(&newer).unwrap();
    fs::write(&config, &newer).unwrap();

    let remove = run_setup_with_copilot_home(
        root,
        &["mcp", "setup", "--agent", "copilot", "--remove"],
        Some(&copilot_home),
    );

    assert_eq!(remove.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&remove.stderr).contains("did not assume ownership"));
    assert_eq!(fs::read(&config).unwrap(), newer);
    assert!(journal.is_file());
    assert!(!receipt.exists());
    assert_eq!(fs::read(&backup).unwrap(), base);
}
