mod common;

use common::{TestDirectory, ai_fuel_config_dir, backup_files, backup_path, codex_home, run_setup};
use std::fs;

#[test]
fn public_setup_previews_applies_repeats_and_removes_without_losing_user_config() {
    let directory = TestDirectory::new("mcp-setup-public");
    let root = directory.path();
    let config = codex_home(root).join("config.toml");
    let config_dir = ai_fuel_config_dir(root);
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let original = br#"# Keep this comment and the user's Codex settings.
model = "gpt-5"

[mcp_servers.docs]
command = "docs-server"
args = ["--mode", "local"]
"#;
    fs::write(&config, original).unwrap();
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("mcp.json"),
        br#"{"servers":{"private":{"transport":"stdio","command":"fixture","env":{"TOKEN":{"value":"fixture-secret"}}}},"defaults":["private"]}"#,
    )
    .unwrap();

    let preview = run_setup(root, &["mcp", "setup", "--agent", "codex", "--dry-run"]);

    assert!(preview.status.success());
    assert!(String::from_utf8_lossy(&preview.stdout).contains("dry run: would apply"));
    assert_eq!(fs::read(&config).unwrap(), original);
    assert!(!config_dir.join("mcp-registrations").exists());

    let latest = br#"# Keep this comment and the user's Codex settings.
model = "gpt-5"
approval_policy = "on-request"

[mcp_servers.docs]
command = "docs-server"
args = ["--mode", "local"]
"#;
    fs::write(&config, latest).unwrap();
    let apply = run_setup(root, &["mcp", "setup", "--agent", "codex"]);

    assert!(
        apply.status.success(),
        "setup should apply: {}",
        String::from_utf8_lossy(&apply.stderr)
    );
    assert!(String::from_utf8_lossy(&apply.stdout).contains("registration applied"));
    let apply_backup = backup_path(&apply);
    assert_eq!(fs::read(&apply_backup).unwrap(), latest);
    let updated = fs::read_to_string(&config).unwrap();
    assert!(updated.contains("# Keep this comment"));
    assert!(updated.contains("approval_policy = \"on-request\""));
    assert!(updated.contains("[mcp_servers.docs]"));
    assert!(updated.contains("command = \"docs-server\""));
    assert!(updated.contains("[mcp_servers.aifuel-gateway]"));
    let parsed: toml_edit::DocumentMut = updated
        .parse()
        .expect("setup should write valid Codex TOML");
    assert_eq!(
        parsed["mcp_servers"]["aifuel-gateway"]["command"].as_str(),
        Some(env!("CARGO_BIN_EXE_aifuel"))
    );
    assert!(updated.contains("args = [\"mcp\", \"gateway\", \"--agent\", \"codex\"]"));
    assert!(updated.contains("env_vars = [\"XDG_CONFIG_HOME\"]"));
    assert!(!updated.contains("fixture-secret"));
    assert!(!updated.contains("private"));

    let repeated = run_setup(root, &["mcp", "setup", "--agent", "codex"]);
    assert!(repeated.status.success());
    assert!(
        String::from_utf8_lossy(&repeated.stdout).contains("no configuration change was needed")
    );
    assert_eq!(updated.matches("[mcp_servers.aifuel-gateway]").count(), 1);
    assert_eq!(backup_files(&config_dir).len(), 1);

    let remove = run_setup(root, &["mcp", "setup", "--agent", "codex", "--remove"]);
    assert!(
        remove.status.success(),
        "removal should succeed: {}",
        String::from_utf8_lossy(&remove.stderr)
    );
    assert!(String::from_utf8_lossy(&remove.stdout).contains("registration removed"));
    assert!(
        fs::read_to_string(&config)
            .unwrap()
            .contains("[mcp_servers.docs]")
    );
    assert!(
        !fs::read_to_string(&config)
            .unwrap()
            .contains("[mcp_servers.aifuel-gateway]")
    );
    let remove_backup = backup_path(&remove);
    assert_eq!(remove_backup, apply_backup);
    assert!(
        fs::read_to_string(remove_backup)
            .unwrap()
            .contains("[mcp_servers.aifuel-gateway]")
    );
    assert_eq!(backup_files(&config_dir).len(), 1);
}

#[test]
fn public_antigravity_setup_targets_its_global_mcp_config() {
    let directory = TestDirectory::new("mcp-setup-antigravity-public");
    let root = directory.path();
    let config = root.join(".gemini").join("config").join("mcp_config.json");
    let config_dir = ai_fuel_config_dir(root);
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let original = br#"{
  "settings": {"theme": "light"},
  "mcpServers": {
    "docs": {"command": "docs-server", "args": []}
  }
}"#;
    fs::write(&config, original).unwrap();

    let apply = run_setup(root, &["mcp", "setup", "--agent", "antigravity"]);

    assert!(
        apply.status.success(),
        "Antigravity setup should apply: {}",
        String::from_utf8_lossy(&apply.stderr)
    );
    assert!(String::from_utf8_lossy(&apply.stdout).contains("registration applied"));
    let apply_backup = backup_path(&apply);
    assert_eq!(fs::read(&apply_backup).unwrap(), original);
    let updated: serde_json::Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert_eq!(updated["settings"]["theme"], "light");
    assert_eq!(updated["mcpServers"]["docs"]["command"], "docs-server");
    assert_eq!(
        updated["mcpServers"]["aifuel-gateway"],
        serde_json::json!({
            "command": env!("CARGO_BIN_EXE_aifuel"),
            "args": ["mcp", "gateway", "--agent", "antigravity"],
            "disabled": false
        })
    );

    let repeated = run_setup(root, &["mcp", "setup", "--agent", "antigravity"]);
    assert!(repeated.status.success());
    assert!(
        String::from_utf8_lossy(&repeated.stdout).contains("no configuration change was needed")
    );
    assert_eq!(backup_files(&config_dir).len(), 1);

    let remove = run_setup(
        root,
        &["mcp", "setup", "--agent", "antigravity", "--remove"],
    );
    assert!(
        remove.status.success(),
        "Antigravity removal should succeed: {}",
        String::from_utf8_lossy(&remove.stderr)
    );
    let removed: serde_json::Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert!(removed["mcpServers"].get("aifuel-gateway").is_none());
    assert_eq!(removed["mcpServers"]["docs"]["command"], "docs-server");
    assert_eq!(backup_path(&remove), apply_backup);
    assert_eq!(backup_files(&config_dir).len(), 1);
}

#[test]
fn public_setup_rejects_unowned_conflicts_without_changing_them() {
    let directory = TestDirectory::new("mcp-setup-conflict");
    let root = directory.path();
    let config = codex_home(root).join("config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let original = br#"[mcp_servers.aifuel-gateway]
command = "some-other-command"
args = ["mcp", "gateway", "--agent", "codex"]
"#;
    fs::write(&config, original).unwrap();

    let setup = run_setup(root, &["mcp", "setup", "--agent", "codex"]);
    assert_eq!(setup.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&setup.stderr).contains("different or unowned"));
    assert_eq!(fs::read(&config).unwrap(), original);

    let remove = run_setup(root, &["mcp", "setup", "--agent", "codex", "--remove"]);
    assert_eq!(remove.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&remove.stderr).contains("unowned"));
    assert_eq!(fs::read(&config).unwrap(), original);
}

#[test]
fn public_setup_does_not_adopt_an_identical_unowned_registration() {
    let directory = TestDirectory::new("mcp-setup-unowned-identical");
    let root = directory.path();
    let config = codex_home(root).join("config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let original = format!(
        "[mcp_servers.aifuel-gateway]\ncommand = {:?}\nargs = [\"mcp\", \"gateway\", \"--agent\", \"codex\"]\nenv_vars = [\"XDG_CONFIG_HOME\"]\n",
        env!("CARGO_BIN_EXE_aifuel")
    );
    fs::write(&config, &original).unwrap();

    let setup = run_setup(root, &["mcp", "setup", "--agent", "codex"]);

    assert!(setup.status.success());
    assert!(String::from_utf8_lossy(&setup.stdout).contains("no configuration change was needed"));
    assert_eq!(fs::read_to_string(&config).unwrap(), original);
    assert!(
        !ai_fuel_config_dir(root)
            .join("mcp-registrations")
            .join("receipts")
            .exists()
    );

    let remove = run_setup(root, &["mcp", "setup", "--agent", "codex", "--remove"]);
    assert_eq!(remove.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&remove.stderr).contains("unowned"));
    assert_eq!(fs::read_to_string(&config).unwrap(), original);
}

#[test]
fn public_setup_rejects_malformed_toml_without_replacing_it() {
    let directory = TestDirectory::new("mcp-setup-malformed");
    let root = directory.path();
    let config = codex_home(root).join("config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, b"[mcp_servers\n").unwrap();

    let setup = run_setup(root, &["mcp", "setup", "--agent", "codex"]);

    assert_eq!(setup.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&setup.stderr).contains("Codex config.toml is malformed"));
    assert_eq!(fs::read(&config).unwrap(), b"[mcp_servers\n");
}

#[test]
fn public_setup_refuses_to_remove_a_registration_edited_after_apply() {
    let directory = TestDirectory::new("mcp-setup-edited");
    let root = directory.path();
    let config = codex_home(root).join("config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "model = \"gpt-5\"\n").unwrap();
    let setup = run_setup(root, &["mcp", "setup", "--agent", "codex"]);
    assert!(setup.status.success());
    let mut edited = fs::read_to_string(&config).unwrap();
    edited = edited.replace("--agent\", \"codex", "--agent\", \"changed");
    fs::write(&config, &edited).unwrap();

    let remove = run_setup(root, &["mcp", "setup", "--agent", "codex", "--remove"]);

    assert_eq!(remove.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&remove.stderr).contains("edited after AI Fuel created it"));
    assert_eq!(fs::read(&config).unwrap(), edited.as_bytes());
}

#[test]
fn public_claude_setup_uses_user_scope_json_and_preserves_other_settings() {
    let directory = TestDirectory::new("mcp-setup-claude-public");
    let root = directory.path();
    let config = root.join(".claude.json");
    let config_dir = ai_fuel_config_dir(root);
    fs::write(
        &config,
        br#"{
  "userID": "temporary-user",
  "projects": {
    "/work/example": {
      "hasTrustDialogAccepted": true,
      "mcpServers": {"project-tools": {"type": "stdio", "command": "project-tool"}}
    }
  },
  "mcpServers": {
    "notes": {"type": "stdio", "command": "notes", "args": [], "env": {}}
  }
}"#,
    )
    .unwrap();
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("mcp.json"),
        br#"{"servers":{"private":{"transport":"stdio","command":"fixture","env":{"TOKEN":{"value":"fixture-secret"}}}},"defaults":["private"]}"#,
    )
    .unwrap();
    let original = fs::read(&config).unwrap();

    let preview = run_setup(root, &["mcp", "setup", "--agent", "claude", "--dry-run"]);

    assert!(preview.status.success());
    assert!(String::from_utf8_lossy(&preview.stdout).contains("dry run: would apply"));
    assert_eq!(fs::read(&config).unwrap(), original);
    assert!(!config_dir.join("mcp-registrations").exists());

    let apply = run_setup(root, &["mcp", "setup", "--agent", "claude"]);

    assert!(
        apply.status.success(),
        "Claude setup should apply: {}",
        String::from_utf8_lossy(&apply.stderr)
    );
    assert!(String::from_utf8_lossy(&apply.stdout).contains("registration applied"));
    let apply_backup = backup_path(&apply);
    assert_eq!(fs::read(&apply_backup).unwrap(), original);
    let updated: serde_json::Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert_eq!(updated["userID"], "temporary-user");
    assert_eq!(
        updated["projects"]["/work/example"]["hasTrustDialogAccepted"],
        true
    );
    assert_eq!(
        updated["projects"]["/work/example"]["mcpServers"]["project-tools"]["command"],
        "project-tool"
    );
    assert_eq!(updated["mcpServers"]["notes"]["command"], "notes");
    assert_eq!(updated["mcpServers"]["aifuel-gateway"]["type"], "stdio");
    assert_eq!(
        updated["mcpServers"]["aifuel-gateway"]["command"],
        env!("CARGO_BIN_EXE_aifuel")
    );
    assert_eq!(
        updated["mcpServers"]["aifuel-gateway"]["args"],
        serde_json::json!(["mcp", "gateway", "--agent", "claude"])
    );
    assert_eq!(
        updated["mcpServers"]["aifuel-gateway"]["env"],
        serde_json::json!({})
    );
    let updated_text = String::from_utf8(fs::read(&config).unwrap()).unwrap();
    assert!(!updated_text.contains("fixture-secret"));
    assert!(!updated_text.contains("private"));

    let repeated = run_setup(root, &["mcp", "setup", "--agent", "claude"]);
    assert!(repeated.status.success());
    assert!(
        String::from_utf8_lossy(&repeated.stdout).contains("no configuration change was needed")
    );
    assert_eq!(backup_files(&config_dir).len(), 1);

    let remove = run_setup(root, &["mcp", "setup", "--agent", "claude", "--remove"]);
    assert!(
        remove.status.success(),
        "Claude removal should succeed: {}",
        String::from_utf8_lossy(&remove.stderr)
    );
    let removed: serde_json::Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert!(removed["mcpServers"].get("aifuel-gateway").is_none());
    assert_eq!(removed["mcpServers"]["notes"]["command"], "notes");
    assert_eq!(
        removed["projects"]["/work/example"]["mcpServers"]["project-tools"]["command"],
        "project-tool"
    );
    assert_eq!(backup_path(&remove), apply_backup);
    assert_eq!(backup_files(&config_dir).len(), 1);
}

#[test]
fn public_claude_setup_rejects_an_unowned_gateway_conflict() {
    let directory = TestDirectory::new("mcp-setup-claude-conflict");
    let root = directory.path();
    let config = root.join(".claude.json");
    let original = br#"{"mcpServers":{"aifuel-gateway":{"type":"stdio","command":"user-owned","args":["mcp","gateway","--agent","claude"],"env":{}}}}"#;
    fs::write(&config, original).unwrap();

    let setup = run_setup(root, &["mcp", "setup", "--agent", "claude"]);

    assert_eq!(setup.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&setup.stderr).contains("different or unowned"));
    assert_eq!(fs::read(&config).unwrap(), original);
}
