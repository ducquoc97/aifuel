use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(prefix: &str) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock should be after the unix epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("aifuel-{prefix}-{}-{suffix}", std::process::id()));
        fs::create_dir_all(&path).expect("test directory should be creatable");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn configure_user_config_root(command: &mut Command, root: &Path) {
    #[cfg(target_os = "windows")]
    command.env("APPDATA", root).env("USERPROFILE", root);

    #[cfg(target_os = "macos")]
    command.env("HOME", root);

    #[cfg(all(unix, not(target_os = "macos")))]
    command.env("XDG_CONFIG_HOME", root).env("HOME", root);
}

fn codex_home(root: &Path) -> PathBuf {
    root.join("codex-home")
}

fn ai_fuel_config_dir(root: &Path) -> PathBuf {
    #[cfg(target_os = "windows")]
    let path = root.join("aifuel");

    #[cfg(target_os = "macos")]
    let path = root
        .join("Library")
        .join("Application Support")
        .join("aifuel");

    #[cfg(all(unix, not(target_os = "macos")))]
    let path = root.join("aifuel");

    path
}

fn run_setup(root: &Path, args: &[&str]) -> Output {
    let home = codex_home(root);
    fs::create_dir_all(&home).expect("temporary CODEX_HOME should exist before setup");
    let mut command = Command::new(env!("CARGO_BIN_EXE_aifuel"));
    command.args(args).env("CODEX_HOME", home);
    configure_user_config_root(&mut command, root);
    command.output().expect("aifuel setup command should start")
}

fn backup_files(config_dir: &Path) -> Vec<PathBuf> {
    let backup_root = config_dir.join("mcp-registrations").join("backups");
    let Ok(host_dirs) = fs::read_dir(backup_root) else {
        return Vec::new();
    };
    let mut files: Vec<_> = host_dirs
        .flat_map(|entry| {
            fs::read_dir(entry.expect("backup host directory should exist").path())
                .expect("backup directory should exist")
        })
        .map(|entry| entry.expect("backup file should exist").path())
        .collect();
    files.sort();
    files
}

fn backup_path(output: &Output) -> PathBuf {
    let text = String::from_utf8_lossy(&output.stdout);
    let path = text
        .lines()
        .find_map(|line| line.strip_prefix("Configuration backup: "))
        .expect("setup should report its recoverable backup");
    PathBuf::from(path)
}

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
    assert!(updated.contains(&format!("command = \"{}\"", env!("CARGO_BIN_EXE_aifuel"))));
    assert!(updated.contains("args = [\"mcp\", \"gateway\", \"--agent\", \"codex\"]"));
    assert!(updated.contains("env_vars = [\"XDG_CONFIG_HOME\"]"));
    assert!(!updated.contains("fixture-secret"));
    assert!(!updated.contains("private"));

    let repeated = run_setup(root, &["mcp", "setup", "--agent", "codex"]);
    assert!(repeated.status.success());
    assert!(String::from_utf8_lossy(&repeated.stdout).contains("ownership was not adopted"));
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
