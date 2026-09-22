use std::fs;
use std::process::Command;

#[allow(dead_code)]
mod support;

use support::{TestDirectory, install_fake_command, path_with};

#[test]
fn run_applies_a_named_profile_to_omitted_model_and_access_values() {
    let directory = TestDirectory::new("selection-cli-profile");
    let config_dir = directory.path().join("aifuel");
    fs::create_dir_all(&config_dir).expect("config directory should exist");
    fs::write(
        config_dir.join("execution.json"),
        r#"{
          "schema_version": 1,
          "profiles": {
            "review": {
              "model": "profile-model",
              "access": "workspace-write"
            }
          },
          "policy": {"allowed_roots": [], "retain_content": false}
        }"#,
    )
    .expect("selection config should be writable");
    install_fake_command(directory.path(), "codex");

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "codex",
            "--profile",
            "review",
            "--prompt",
            "hello",
        ])
        .env("PATH", path_with(directory.path()))
        .env("HOME", directory.path())
        .env("XDG_CONFIG_HOME", directory.path())
        .output()
        .expect("aifuel should start");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--model profile-model"));
    assert!(stdout.contains("--sandbox workspace-write"));
}
