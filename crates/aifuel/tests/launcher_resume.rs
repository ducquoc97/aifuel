#[allow(dead_code)]
mod support;

#[cfg(unix)]
mod codex_resume {
    use std::fs;
    use std::process::Command;

    use super::support::{
        TestDirectory, ai_fuel_config_dir, install_fake_codex_app_server, path_with,
    };

    #[test]
    fn codex_resume_uses_the_managed_native_session_with_read_only_sandbox() {
        let directory = TestDirectory::new("codex-resume");
        let log_path = install_fake_codex_app_server(directory.path());
        let config = ai_fuel_config_dir(directory.path());
        fs::create_dir_all(&config).expect("AI Fuel config directory should exist");
        fs::write(
            config.join("execution.json"),
            format!(
                "{{\"schema_version\":1,\"policy\":{{\"allowed_roots\":[{:?}],\"retain_content\":false}}}}",
                directory.path().to_string_lossy()
            ),
        )
        .expect("execution policy should be writable");
        fs::write(
            config.join("agent-sessions.json"),
            format!(
                "{{\"schema_version\":1,\"sessions\":{{\"session-123\":{{\"provider\":\"codex\",\"model\":null,\"effort\":null,\"working_directory\":{:?}}}}}}}",
                directory.path().to_string_lossy()
            ),
        )
        .expect("managed session metadata should be writable");

        let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
            .args([
                "run",
                "--provider",
                "codex",
                "--prompt",
                "continue safely",
                "--resume",
                "session-123",
            ])
            .env("PATH", path_with(directory.path()))
            .env("HOME", directory.path())
            .env("USERPROFILE", directory.path())
            .env("APPDATA", directory.path())
            .env("XDG_CONFIG_HOME", directory.path().join(".config"))
            .env("AIFUEL_CODEX_FIXTURE_LOG", &log_path)
            .output()
            .expect("aifuel should start");

        assert!(
            output.status.success(),
            "Codex App Server fixture rejected the resumed invocation: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "fake codex app-server response"
        );
        let requests: Vec<serde_json::Value> = fs::read_to_string(log_path)
            .expect("App Server requests should be logged")
            .lines()
            .map(|line| serde_json::from_str(line).expect("each request should be JSON"))
            .collect();
        assert_eq!(requests[2]["method"], "thread/resume");
        assert_eq!(requests[2]["params"]["threadId"], "session-123");
        assert_eq!(requests[2]["params"]["sandbox"], "read-only");
        assert_eq!(requests[3]["method"], "turn/start");
        assert_eq!(requests[3]["params"]["input"][0]["text"], "continue safely");
    }
}
