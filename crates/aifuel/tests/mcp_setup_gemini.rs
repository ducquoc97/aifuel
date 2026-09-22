mod common;

use common::{TestDirectory, ai_fuel_config_dir, backup_files, backup_path, run_setup};
use serde_json::Value;
use std::fs;

#[test]
fn public_gemini_setup_previews_applies_repeats_and_removes_without_losing_user_config() {
    let directory = TestDirectory::new("mcp-setup-gemini-public");
    let root = directory.path();
    let config = root.join(".gemini").join("settings.json");
    let config_dir = ai_fuel_config_dir(root);
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let original = br#"{
  "theme": "dark",
  "mcp": {"allowed": ["docs"]},
  "mcpServers": {
    "docs": {"command": "docs-server", "args": ["--mode", "local"]}
  }
}"#;
    fs::write(&config, original).unwrap();

    let preview = run_setup(root, &["mcp", "setup", "--agent", "gemini", "--dry-run"]);

    assert!(preview.status.success());
    assert!(String::from_utf8_lossy(&preview.stdout).contains("dry run: would apply"));
    assert_eq!(fs::read(&config).unwrap(), original);
    assert!(!config_dir.join("mcp-registrations").exists());

    let apply = run_setup(root, &["mcp", "setup", "--agent", "gemini"]);

    assert!(
        apply.status.success(),
        "Gemini setup should apply: {}",
        String::from_utf8_lossy(&apply.stderr)
    );
    assert!(String::from_utf8_lossy(&apply.stdout).contains("registration applied"));
    let apply_backup = backup_path(&apply);
    assert_eq!(fs::read(&apply_backup).unwrap(), original);
    let updated: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert_eq!(updated["theme"], "dark");
    assert_eq!(updated["mcp"]["allowed"], serde_json::json!(["docs"]));
    assert_eq!(updated["mcpServers"]["docs"]["command"], "docs-server");
    assert_eq!(
        updated["mcpServers"]["aifuel-gateway"],
        serde_json::json!({
            "command": env!("CARGO_BIN_EXE_aifuel"),
            "args": ["mcp", "gateway", "--agent", "gemini"]
        })
    );

    let repeated = run_setup(root, &["mcp", "setup", "--agent", "gemini"]);
    assert!(repeated.status.success());
    assert!(
        String::from_utf8_lossy(&repeated.stdout).contains("no configuration change was needed")
    );
    assert_eq!(backup_files(&config_dir).len(), 1);

    let remove = run_setup(root, &["mcp", "setup", "--agent", "gemini", "--remove"]);
    assert!(
        remove.status.success(),
        "Gemini removal should succeed: {}",
        String::from_utf8_lossy(&remove.stderr)
    );
    assert!(String::from_utf8_lossy(&remove.stdout).contains("registration removed"));
    let removed: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert!(removed["mcpServers"].get("aifuel-gateway").is_none());
    assert_eq!(removed["mcpServers"]["docs"]["command"], "docs-server");
    assert_eq!(removed["theme"], "dark");
    assert_eq!(backup_path(&remove), apply_backup);
    assert_eq!(backup_files(&config_dir).len(), 1);
}

#[test]
fn public_gemini_setup_rejects_unowned_conflicts_without_changing_them() {
    let directory = TestDirectory::new("mcp-setup-gemini-conflict");
    let root = directory.path();
    let config = root.join(".gemini").join("settings.json");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let original = br#"{
  "mcpServers": {
    "aifuel-gateway": {
      "command": "some-other-command",
      "args": ["mcp", "gateway", "--agent", "gemini"]
    }
  }
}"#;
    fs::write(&config, original).unwrap();

    let setup = run_setup(root, &["mcp", "setup", "--agent", "gemini"]);

    assert_eq!(setup.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&setup.stderr).contains("different or unowned"));
    assert_eq!(fs::read(&config).unwrap(), original);

    let remove = run_setup(root, &["mcp", "setup", "--agent", "gemini", "--remove"]);

    assert_eq!(remove.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&remove.stderr).contains("unowned"));
    assert_eq!(fs::read(&config).unwrap(), original);
}
