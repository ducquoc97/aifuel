use std::process::Command;

#[allow(dead_code)]
mod support;

use support::TestDirectory;

#[test]
fn profiles_are_saved_and_listed_only_by_explicit_commands() {
    let directory = TestDirectory::new("profile-cli");
    let mut save_command = Command::new(env!("CARGO_BIN_EXE_aifuel"));
    configure(&mut save_command, directory.path());
    let save = save_command
        .args([
            "profile",
            "save",
            "review",
            "--provider",
            "codex",
            "--model",
            "profile-model",
            "--access",
            "read-only",
        ])
        .output()
        .expect("profile save should start");
    assert!(
        save.status.success(),
        "{}",
        String::from_utf8_lossy(&save.stderr)
    );

    let mut list_command = Command::new(env!("CARGO_BIN_EXE_aifuel"));
    configure(&mut list_command, directory.path());
    let list = list_command
        .args(["profile", "list"])
        .output()
        .expect("profile list should start");
    assert!(list.status.success());
    let output = String::from_utf8_lossy(&list.stdout);
    assert!(output.contains("review"));
    assert!(output.contains("profile-model"));

    let mut remove_command = Command::new(env!("CARGO_BIN_EXE_aifuel"));
    configure(&mut remove_command, directory.path());
    let remove = remove_command
        .args(["profile", "remove", "review"])
        .output()
        .expect("profile remove should start");
    assert!(remove.status.success());
}

fn configure(command: &mut Command, root: &std::path::Path) {
    command.env("HOME", root).env("XDG_CONFIG_HOME", root);
}
