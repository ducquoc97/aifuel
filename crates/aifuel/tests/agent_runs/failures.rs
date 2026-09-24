#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::process::Command;

#[cfg(unix)]
use std::process::Stdio;

#[cfg(unix)]
use crate::support::{TestDirectory, path_with};

#[cfg(unix)]
use crate::support::{ai_fuel_config_dir, install_fake_codex_app_server};

#[cfg(unix)]
#[test]
fn stdin_prompt_and_working_directory_reach_the_verified_codex_adapter() {
    use std::io::Write;
    use std::os::unix::fs::symlink;

    let directory = TestDirectory::new("stdin-working-directory");
    let physical_working_directory = directory.path().join("workspace");
    let working_directory = directory.path().join("workspace-alias");
    let home = directory.path().join("home");
    fs::create_dir_all(&physical_working_directory).expect("workspace should be creatable");
    symlink(&physical_working_directory, &working_directory)
        .expect("working-directory alias should be creatable");
    fs::create_dir_all(&home).expect("temporary home should be creatable");
    let config = ai_fuel_config_dir(&home);
    fs::create_dir_all(&config).expect("AI Fuel config directory should exist");
    fs::write(
        config.join("execution.json"),
        serde_json::json!({
            "schema_version": 1,
            "policy": {
                "allowed_roots": [physical_working_directory],
                "retain_content": false
            }
        })
        .to_string(),
    )
    .expect("execution policy should be writable");
    let app_server_log = install_fake_codex_app_server(directory.path());

    let mut child = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "codex",
            "--model",
            "test-model",
            "--working-directory",
            working_directory.to_str().expect("test path is UTF-8"),
            "--output",
            "json",
        ])
        .env("PATH", path_with(directory.path()))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("APPDATA", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("AIFUEL_CODEX_FIXTURE_LOG", &app_server_log)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("aifuel should start");
    child
        .stdin
        .take()
        .expect("stdin should be piped")
        .write_all(b"prompt from stdin")
        .expect("stdin prompt should be written");
    let output = child
        .wait_with_output()
        .expect("aifuel should finish the selected run");

    assert!(
        output.status.success(),
        "aifuel run should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("managed run should return JSON");
    let expected_working_directory =
        fs::canonicalize(&working_directory).expect("working-directory alias should resolve");
    assert_ne!(working_directory, expected_working_directory);
    assert!(
        result["output"]
            .as_str()
            .expect("Codex should return public text")
            .contains("fake codex app-server response")
    );
    let requests = fs::read_to_string(app_server_log)
        .expect("App Server request frames should be logged")
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .expect("App Server frames should be valid JSON");
    assert_eq!(
        requests[2]["params"]["cwd"],
        expected_working_directory.to_string_lossy().as_ref()
    );
    assert_eq!(
        requests[3]["params"]["input"][0]["text"],
        "prompt from stdin"
    );
    assert!(
        fs::read_dir(&physical_working_directory)
            .expect("workspace should remain inspectable")
            .next()
            .is_none(),
        "read-only Codex run should not write into the repository workspace"
    );
}
