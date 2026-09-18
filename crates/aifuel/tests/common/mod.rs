use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct TestDirectory(PathBuf);

impl TestDirectory {
    pub fn new(prefix: &str) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock should be after the unix epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("aifuel-{prefix}-{}-{suffix}", std::process::id()));
        fs::create_dir_all(&path).expect("test directory should be creatable");
        Self(path)
    }

    pub fn path(&self) -> &Path {
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

pub fn codex_home(root: &Path) -> PathBuf {
    root.join("codex-home")
}

pub fn ai_fuel_config_dir(root: &Path) -> PathBuf {
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

pub fn run_setup(root: &Path, args: &[&str]) -> Output {
    let codex_home = args
        .windows(2)
        .any(|pair| pair[0] == "--agent" && pair[1] == "codex")
        .then(|| codex_home(root));
    if let Some(home) = &codex_home {
        fs::create_dir_all(home).expect("temporary CODEX_HOME should exist before setup");
    }
    run_setup_with_codex_home(root, args, codex_home)
}

fn run_setup_with_codex_home(root: &Path, args: &[&str], codex_home: Option<PathBuf>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aifuel"));
    command.args(args);
    if let Some(codex_home) = codex_home {
        command.env("CODEX_HOME", codex_home);
    }
    configure_user_config_root(&mut command, root);
    command.output().expect("aifuel setup command should start")
}

pub fn backup_files(config_dir: &Path) -> Vec<PathBuf> {
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

pub fn backup_path(output: &Output) -> PathBuf {
    let text = String::from_utf8_lossy(&output.stdout);
    let path = text
        .lines()
        .find_map(|line| line.strip_prefix("Configuration backup: "))
        .expect("setup should report its recoverable backup");
    PathBuf::from(path)
}
