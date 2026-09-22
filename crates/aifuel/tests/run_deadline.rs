#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[allow(dead_code)]
mod support;

#[test]
fn a_silent_agent_can_complete_after_thirty_seconds_without_an_explicit_deadline() {
    let directory = support::TestDirectory::new("no-default-deadline");
    let executable = directory.path().join("codex");
    fs::write(&executable, "#!/bin/sh\nif [ \"$2\" = \"--help\" ]; then printf 'exec --sandbox --ignore-user-config --ignore-rules --json\\n'; exit 0; fi\nsleep 31\nprintf 'finished after silence\\n'\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
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
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("finished after silence"));
}
