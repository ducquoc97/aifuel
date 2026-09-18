#[allow(dead_code)]
mod common;

use common::{
    TestDirectory, ai_fuel_config_dir, backup_files, backup_path, run_setup_with_copilot_home,
};
use serde_json::Value;
use std::fs;

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
