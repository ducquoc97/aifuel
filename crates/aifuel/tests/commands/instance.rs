//! `aifuel instance` against the real binary over an isolated HOME: the
//! management commands edit `providers.json`, list and show never print
//! secret material, and a run through an instance selector injects the
//! resolved overlay into the provider process environment.

use std::process::Command;

use crate::support::{
    TestDirectory, install_fake_codex_app_server, path_with, seed_codex_authentication,
};

fn aifuel(directory: &TestDirectory, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args(args)
        .env("PATH", path_with(directory.path()))
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"))
        .output()
        .expect("aifuel should start")
}

#[test]
fn instance_list_add_show_remove_round_trip() {
    let directory = TestDirectory::new("instance-cli");

    // Nothing configured: the empty state is honest, not an error.
    let output = aifuel(&directory, &["instance", "list"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No Provider Integration instances"),
        "{stdout}"
    );

    // An unknown base integration is refused before the file is touched.
    let output = aifuel(
        &directory,
        &["instance", "add", "ghost.work", "--integration", "ghost"],
    );
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ghost"), "{stderr}");

    // A valid add writes providers.json under the isolated config dir.
    let output = aifuel(
        &directory,
        &[
            "instance",
            "add",
            "codex.work",
            "--integration",
            "codex",
            "--env",
            "FAKE_INSTANCE_MARKER=marker-literal",
            "--env-credential",
            "FAKE_INSTANCE_KEY=work-key",
            "--credential",
            "work-key",
        ],
    );
    assert!(
        output.status.success(),
        "instance add: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // `list` names the instance, its base, the credential state, and env
    // variable names - never values.
    let output = aifuel(&directory, &["instance", "list"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("codex.work"), "{stdout}");
    assert!(stdout.contains("codex"), "{stdout}");
    assert!(stdout.contains("work-key (absent)"), "{stdout}");
    assert!(stdout.contains("FAKE_INSTANCE_MARKER"), "{stdout}");
    assert!(!stdout.contains("marker-literal"), "{stdout}");

    // `show` prints literal values and credential references; resolved
    // material can never appear because nothing resolves it here.
    let output = aifuel(&directory, &["instance", "show", "codex.work"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("FAKE_INSTANCE_MARKER=marker-literal"),
        "{stdout}"
    );
    assert!(stdout.contains("{credential: work-key}"), "{stdout}");

    // `auth set-key` on the instance writes the bound Credential Reference.
    let output = aifuel(
        &directory,
        &["auth", "set-key", "codex.work", "--key", "sk-e2e-secret"],
    );
    assert!(
        output.status.success(),
        "auth set-key: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("work-key bound to codex.work"), "{stdout}");
    assert!(!stdout.contains("sk-e2e-secret"), "{stdout}");

    // The credential now reports present in list output.
    let output = aifuel(&directory, &["instance", "list"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("work-key (present)"), "{stdout}");

    // Remove is refused for unknown ids and honors a known one.
    let output = aifuel(&directory, &["instance", "remove", "nobody.home"]);
    assert_eq!(output.status.code(), Some(2));
    let output = aifuel(&directory, &["instance", "remove", "codex.work"]);
    assert!(output.status.success());
    let output = aifuel(&directory, &["instance", "list"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No Provider Integration instances"),
        "{stdout}"
    );
}

#[test]
fn a_run_through_an_instance_injects_the_resolved_environment() {
    let directory = TestDirectory::new("instance-codex-run");
    let log_path = install_fake_codex_app_server(directory.path());
    seed_codex_authentication(directory.path());
    let env_log = directory.path().join("codex-env.log");

    // `instance add` + `auth set-key` through the real CLI, then a run whose
    // provider process proves the overlay landed.
    let output = aifuel(
        &directory,
        &[
            "instance",
            "add",
            "codex.work",
            "--integration",
            "codex",
            "--env",
            "FAKE_INSTANCE_MARKER=marker-literal",
            "--env-credential",
            "FAKE_INSTANCE_KEY=work-key",
        ],
    );
    assert!(
        output.status.success(),
        "instance add: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = aifuel(
        &directory,
        &["auth", "set-key", "work-key", "--key", "sk-e2e-secret"],
    );
    assert!(
        output.status.success(),
        "auth set-key: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--integration",
            "codex.work",
            "--model",
            "test-model",
            "--prompt",
            "hello",
            "--access",
            "read-only",
            "--output",
            "json",
        ])
        .env("PATH", path_with(directory.path()))
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"))
        .env("AIFUEL_CODEX_FIXTURE_LOG", &log_path)
        .env("AIFUEL_CODEX_FIXTURE_ENV_LOG", &env_log)
        .output()
        .expect("aifuel should start");
    assert!(
        output.status.success(),
        "run: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    // The result names the instance id the caller selected, while the
    // provider id stays the base integration's provider.
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("run should emit JSON");
    assert_eq!(value["integration"], "codex.work", "{stdout}");
    assert_eq!(value["provider"], "codex", "{stdout}");
    assert_eq!(value["status"], "succeeded", "{stdout}");
    assert!(
        value["output"]
            .as_str()
            .expect("provider output should be text")
            .contains("fake codex app-server response"),
        "{stdout}"
    );

    // The provider process saw the literal and the credential-resolved
    // values; the run's own output never carries the secret.
    let recorded = std::fs::read_to_string(&env_log).expect("env log exists");
    assert!(
        recorded.contains("FAKE_INSTANCE_MARKER=marker-literal"),
        "{recorded}"
    );
    assert!(
        recorded.contains("FAKE_INSTANCE_KEY=sk-e2e-secret"),
        "{recorded}"
    );
    assert!(!stdout.contains("sk-e2e-secret"), "{stdout}");

    // A missing credential fails before spawn: remove the key and rerun.
    let output = aifuel(&directory, &["auth", "remove", "work-key"]);
    assert!(output.status.success());
    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--integration",
            "codex.work",
            "--model",
            "test-model",
            "--prompt",
            "hello",
            "--access",
            "read-only",
        ])
        .env("PATH", path_with(directory.path()))
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"))
        .output()
        .expect("aifuel should start");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("codex.work"), "{stderr}");
    assert!(stderr.contains("work-key"), "{stderr}");
    assert!(!stderr.contains("sk-e2e-secret"), "{stderr}");
}
