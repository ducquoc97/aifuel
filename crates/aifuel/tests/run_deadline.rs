#![cfg(unix)]

use std::process::Command;

#[allow(dead_code)]
mod support;

#[test]
fn a_silent_agent_can_complete_after_thirty_seconds_without_an_explicit_deadline() {
    let directory = support::TestDirectory::new("no-default-deadline");
    let log_path = support::install_fake_codex_app_server(directory.path());
    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "codex",
            "--model",
            "fixture-model",
            "--prompt",
            "hello",
        ])
        .env("PATH", support::path_with(directory.path()))
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"))
        .env("AIFUEL_CODEX_FIXTURE_LOG", &log_path)
        .env("AIFUEL_CODEX_FIXTURE_DELAY_SECONDS", "31")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("fake codex app-server response"));
}
