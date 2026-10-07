mod common;

use common::{TestDirectory, ai_fuel_config_dir, run_setup};
use std::fs;
use std::process::Command;

#[test]
fn removed_gemini_host_rejects_setup_preview_and_removal_without_changing_user_config() {
    let directory = TestDirectory::new("mcp-setup-gemini-removed");
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

    for flags in [
        vec!["--dry-run"],
        vec![],
        vec!["--remove"],
        vec!["--remove", "--dry-run"],
    ] {
        let mut args = vec!["mcp", "setup", "--agent", "gemini"];
        args.extend(flags);
        let output = run_setup(root, &args);

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {stderr}");
        assert!(
            stderr.contains("MCP Host \"gemini\" has no Agent MCP Registration adapter"),
            "{args:?}: {stderr}"
        );
        assert!(
            output.stdout.is_empty(),
            "no Gemini registration was changed"
        );
        assert_eq!(fs::read(&config).unwrap(), original);
        assert!(!config_dir.join("mcp-registrations").exists());
    }
}

#[test]
fn removing_all_known_mcp_hosts_leaves_legacy_gemini_config_untouched() {
    let directory = TestDirectory::new("mcp-remove-known-hosts");
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

    let remove = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args(["mcp", "setup", "--remove"])
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("APPDATA", root)
        .env("XDG_CONFIG_HOME", root)
        .env("CODEX_HOME", root.join("codex-home"))
        .env("COPILOT_HOME", root.join("copilot-home"))
        .output()
        .expect("aifuel setup command should start");

    let stderr = String::from_utf8_lossy(&remove.stderr);
    assert!(remove.status.success(), "{stderr}");
    assert!(!String::from_utf8_lossy(&remove.stdout).contains("MCP Host gemini:"));
    assert_eq!(fs::read(&config).unwrap(), original);
}
