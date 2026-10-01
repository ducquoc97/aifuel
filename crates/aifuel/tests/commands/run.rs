use std::fs;
use std::process::Command;

use crate::support::{
    TestDirectory, install_fake_codex_app_server, install_fake_command, path_with,
    seed_claude_authentication, seed_codex_authentication, seed_codex_token, start_json_fixture,
};

/// The environment variables the API-key integrations declare
/// (`crates/aifuel-providers/src/integrations/builtin/api_keys.rs`). An
/// inherited variable would make its integration an `auto` candidate, so
/// tests scrub them all and set the ones they exercise.
const API_KEY_ENV_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "CEREBRAS_API_KEY",
    "COHERE_API_KEY",
    "DEEPINFRA_API_KEY",
    "DEEPSEEK_API_KEY",
    "FIREWORKS_API_KEY",
    "GROQ_API_KEY",
    "MISTRAL_API_KEY",
    "MOONSHOT_API_KEY",
    "NVIDIA_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
    "PERPLEXITY_API_KEY",
    "SILICONFLOW_API_KEY",
    "TOGETHER_API_KEY",
    "XAI_API_KEY",
    "ZAI_API_KEY",
];

fn aifuel(directory: &TestDirectory) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aifuel"));
    command
        .env("PATH", path_with(directory.path()))
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"));
    for var in API_KEY_ENV_VARS {
        command.env_remove(var);
    }
    command
}

#[test]
fn run_rejects_prompt_only_read_only_without_verified_provider_enforcement() {
    for (provider, executable) in [
        ("claude", "claude"),
        ("copilot", "copilot"),
        ("gemini", "gemini"),
        ("antigravity", "agy"),
    ] {
        let directory = TestDirectory::new(&format!("{provider}-prompt-read-only"));
        install_fake_command(directory.path(), executable);

        let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
            .args([
                "run",
                "--provider",
                provider,
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

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{provider}: {stderr}");
        assert!(
            stderr.contains(&format!("{provider} cannot enforce read-only access")),
            "expected a read-only enforcement error for {provider}, got {stderr:?}"
        );
        assert!(stdout.is_empty(), "{provider} must not launch: {stdout:?}");
    }
}

#[test]
fn supported_codex_run_emits_a_structured_result_when_json_is_requested() {
    let directory = TestDirectory::new("json-run");
    let log_path = install_fake_codex_app_server(directory.path());
    seed_codex_authentication(directory.path());

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "codex",
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
        .output()
        .expect("aifuel should start");

    assert!(output.status.success());
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("run should emit JSON");
    assert_eq!(value["provider"], "codex");
    assert_eq!(value["requested_model"], "test-model");
    assert!(value["effective_model"].is_null());
    assert_eq!(value["state"], "succeeded");
    assert_eq!(value["status"], "succeeded");
    assert!(value["error"].is_null());
    assert!(
        value["output"]
            .as_str()
            .expect("provider output should be text")
            .contains("fake codex app-server response")
    );
}

#[test]
fn run_uses_the_selected_codex_integration() {
    let directory = TestDirectory::new("codex-run");
    let log_path = install_fake_codex_app_server(directory.path());
    seed_codex_authentication(directory.path());

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "codex",
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
        .env("AIFUEL_CODEX_FIXTURE_LOG", &log_path)
        .output()
        .expect("aifuel should start");

    assert!(
        output.status.success(),
        "{}",
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
    assert_eq!(requests[0]["method"], "initialize");
    assert_eq!(requests[1]["method"], "initialized");
    assert_eq!(requests[2]["method"], "thread/start");
    assert_eq!(requests[2]["params"]["model"], "test-model");
    assert_eq!(requests[2]["params"]["sandbox"], "read-only");
    assert_eq!(requests[3]["method"], "turn/start");
    assert_eq!(requests[3]["params"]["input"][0]["text"], "hello");
}

#[test]
fn model_catalog_refresh_reports_unsupported_provider_interfaces() {
    let directory = TestDirectory::new("model-refresh-unsupported");
    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args(["model", "refresh", "--provider", "gemini"])
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"))
        .output()
        .expect("model refresh should start");

    assert_eq!(output.status.code(), Some(3));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("gemini: unsupported"));
    assert!(stdout.contains("no model list was inferred"));
}

#[test]
fn model_catalog_list_reports_a_missing_cache_without_fabricating_models() {
    let directory = TestDirectory::new("model-list-empty");
    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args(["model", "list", "--provider", "codex"])
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"))
        .output()
        .expect("model list should start");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "No cached model catalog evidence for codex."
    );
}

/// Claude's usage fixture reports 90% remaining while Codex's reports 60%
/// (5% of its primary window used): Claude ranks first, its read-only
/// validation error is a pre-execution launch failure, and the chain moves
/// to Codex, which runs the prompt.
#[test]
fn auto_run_falls_back_to_the_next_ranked_provider_on_a_launch_error() {
    let directory = TestDirectory::new("auto-launch-fallback");
    let log_path = install_fake_codex_app_server(directory.path());
    seed_codex_token(directory.path());
    seed_claude_authentication(directory.path());
    let (claude_url, claude_server) = start_json_fixture(
        r#"{"five_hour":{"remaining_percentage":90,"resets_at":"2030-01-01T00:00:00Z"}}"#,
        1,
    );
    let (codex_url, codex_server) = start_json_fixture(
        r#"{"rate_limit":{"primary_window":{"limit_window_seconds":18000,"used_percent":40,"reset_at":4102444800}}}"#,
        1,
    );

    let output = aifuel(&directory)
        .args([
            "run",
            "--provider",
            "auto",
            "--prompt",
            "hello",
            "--output",
            "json",
        ])
        .env("AIFUEL_CLAUDE_USAGE_URL", &claude_url)
        .env("AIFUEL_CODEX_USAGE_URL", &codex_url)
        .env("AIFUEL_CODEX_FIXTURE_LOG", &log_path)
        .output()
        .expect("aifuel should start");
    claude_server.join().expect("claude fixture should answer");
    codex_server.join().expect("codex fixture should answer");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("auto run should emit JSON");
    assert_eq!(value["provider"], "codex");
    assert_eq!(value["status"], "succeeded");
    assert_eq!(value["routing"]["requested"], "auto");
    assert_eq!(value["routing"]["selected"], "codex");
    assert_eq!(
        value["routing"]["candidates"],
        serde_json::json!([
            {
                "provider": "claude",
                "integration": "claude",
                "basis": "quota",
                "remaining_percent": 90.0,
            },
            {
                "provider": "codex",
                "integration": "codex",
                "basis": "quota",
                "remaining_percent": 60.0,
            },
        ])
    );
    let attempts = value["routing"]["attempts"]
        .as_array()
        .expect("routing attempts should be an array");
    assert_eq!(attempts.len(), 2, "{stdout}");
    assert_eq!(attempts[0]["provider"], "claude");
    assert_eq!(attempts[0]["outcome"], "launch_error");
    assert!(
        attempts[0]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("read-only")),
        "the launch error should carry Claude's validation reason: {stdout}"
    );
    assert_eq!(attempts[1]["provider"], "codex");
    assert_eq!(attempts[1]["outcome"], "succeeded");
    assert!(
        stderr.contains("auto trying codex"),
        "stderr should narrate the fallback: {stderr}"
    );
}

/// A provider-reported quota failure is retriable after a run was
/// accepted: Codex's turn ends on a rate-limit message, the chain records
/// `quota_exhausted`, and with no further candidates the run reports the
/// last attempt's failure.
#[test]
fn auto_run_retries_provider_reported_quota_exhaustion() {
    let directory = TestDirectory::new("auto-quota-fallback");
    install_fake_codex_app_server(directory.path());
    seed_codex_token(directory.path());
    seed_claude_authentication(directory.path());
    let (claude_url, claude_server) = start_json_fixture(
        r#"{"five_hour":{"remaining_percentage":90,"resets_at":"2030-01-01T00:00:00Z"}}"#,
        1,
    );
    let (codex_url, codex_server) = start_json_fixture(
        r#"{"rate_limit":{"primary_window":{"limit_window_seconds":18000,"used_percent":40,"reset_at":4102444800}}}"#,
        1,
    );

    let output = aifuel(&directory)
        .args([
            "run",
            "--provider",
            "auto",
            "--prompt",
            "hello",
            "--output",
            "json",
        ])
        .env("AIFUEL_CLAUDE_USAGE_URL", &claude_url)
        .env("AIFUEL_CODEX_USAGE_URL", &codex_url)
        .env(
            "AIFUEL_CODEX_FIXTURE_TURN_ERROR",
            "Error: rate limit exceeded",
        )
        .output()
        .expect("aifuel should start");
    claude_server.join().expect("claude fixture should answer");
    codex_server.join().expect("codex fixture should answer");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(4), "{stderr}");
    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("auto run should emit JSON");
    assert_eq!(value["provider"], "codex");
    assert_eq!(value["status"], "failed");
    let attempts = value["routing"]["attempts"]
        .as_array()
        .expect("routing attempts should be an array");
    assert_eq!(attempts.len(), 2, "{stdout}");
    assert_eq!(attempts[0]["outcome"], "launch_error");
    assert_eq!(attempts[1]["provider"], "codex");
    assert_eq!(attempts[1]["outcome"], "quota_exhausted");
}

/// A run that reached its provider is the chain's end: Codex fails
/// mid-turn with an ordinary error, Claude still ranks below it, and no
/// second attempt runs.
#[test]
fn auto_run_does_not_fall_back_after_a_mid_run_failure() {
    let directory = TestDirectory::new("auto-no-fallback");
    install_fake_codex_app_server(directory.path());
    seed_codex_token(directory.path());
    seed_claude_authentication(directory.path());
    let (claude_url, claude_server) = start_json_fixture(
        r#"{"five_hour":{"remaining_percentage":90,"resets_at":"2030-01-01T00:00:00Z"}}"#,
        1,
    );
    let (codex_url, codex_server) = start_json_fixture(
        r#"{"rate_limit":{"primary_window":{"limit_window_seconds":18000,"used_percent":5,"reset_at":4102444800}}}"#,
        1,
    );

    let output = aifuel(&directory)
        .args([
            "run",
            "--provider",
            "auto",
            "--prompt",
            "hello",
            "--output",
            "json",
        ])
        .env("AIFUEL_CLAUDE_USAGE_URL", &claude_url)
        .env("AIFUEL_CODEX_USAGE_URL", &codex_url)
        .env("AIFUEL_CODEX_FIXTURE_TURN_ERROR", "the provider exploded")
        .output()
        .expect("aifuel should start");
    claude_server.join().expect("claude fixture should answer");
    codex_server.join().expect("codex fixture should answer");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(4), "{stderr}");
    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("auto run should emit JSON");
    assert_eq!(value["provider"], "codex");
    assert_eq!(value["status"], "failed");
    let attempts = value["routing"]["attempts"]
        .as_array()
        .expect("routing attempts should be an array");
    assert_eq!(attempts.len(), 1, "{stdout}");
    assert_eq!(attempts[0]["provider"], "codex");
    assert_eq!(attempts[0]["outcome"], "failed");
    assert!(
        !stderr.contains("auto trying"),
        "a mid-run failure must not start another attempt: {stderr}"
    );
}

/// A deadline reached during execution stops the chain the way a
/// single-provider run stops: exit 5 and no next provider.
#[test]
fn auto_run_does_not_fall_back_after_a_timeout() {
    let directory = TestDirectory::new("auto-timeout");
    install_fake_codex_app_server(directory.path());
    seed_codex_token(directory.path());
    seed_claude_authentication(directory.path());
    let (claude_url, claude_server) = start_json_fixture(
        r#"{"five_hour":{"remaining_percentage":90,"resets_at":"2030-01-01T00:00:00Z"}}"#,
        1,
    );
    let (codex_url, codex_server) = start_json_fixture(
        r#"{"rate_limit":{"primary_window":{"limit_window_seconds":18000,"used_percent":5,"reset_at":4102444800}}}"#,
        1,
    );

    let output = aifuel(&directory)
        .args([
            "run",
            "--provider",
            "auto",
            "--prompt",
            "hello",
            "--timeout",
            "2s",
            "--output",
            "json",
        ])
        .env("AIFUEL_CLAUDE_USAGE_URL", &claude_url)
        .env("AIFUEL_CODEX_USAGE_URL", &codex_url)
        .env("AIFUEL_CODEX_FIXTURE_DELAY_SECONDS", "30")
        .output()
        .expect("aifuel should start");
    claude_server.join().expect("claude fixture should answer");
    codex_server.join().expect("codex fixture should answer");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(5), "{stderr} {stdout}");
    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("auto run should emit JSON");
    let attempts = value["routing"]["attempts"]
        .as_array()
        .expect("routing attempts should be an array");
    assert_eq!(attempts.len(), 1, "{stdout}");
    assert_eq!(attempts[0]["provider"], "codex");
    assert_eq!(attempts[0]["outcome"], "timed_out");
}

/// `--provider claude` is a literal selection: the same pre-execution
/// failure that moved `auto` to Codex ends this run immediately.
#[test]
fn explicit_provider_never_falls_back() {
    let directory = TestDirectory::new("explicit-no-fallback");
    let log_path = install_fake_codex_app_server(directory.path());
    seed_codex_token(directory.path());
    seed_claude_authentication(directory.path());
    let (claude_url, _claude_server) = start_json_fixture(
        r#"{"five_hour":{"remaining_percentage":90,"resets_at":"2030-01-01T00:00:00Z"}}"#,
        1,
    );

    let output = aifuel(&directory)
        .args([
            "run",
            "--provider",
            "claude",
            "--model",
            "test-model",
            "--prompt",
            "hello",
            "--output",
            "json",
        ])
        .env("AIFUEL_CLAUDE_USAGE_URL", &claude_url)
        .env("AIFUEL_CODEX_FIXTURE_LOG", &log_path)
        .output()
        .expect("aifuel should start");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("read-only"),
        "the failure should be Claude's own validation: {stderr}"
    );
    assert!(
        !log_path.exists(),
        "Codex must not run for an explicit Claude selection"
    );
}

/// `auto --resume` resolves the stored session's owning provider and runs
/// there; Claude ranks higher on quota but the session is Provider-scoped
/// and the chain never moves.
#[test]
fn auto_resume_runs_on_the_session_owning_provider() {
    let directory = TestDirectory::new("auto-resume");
    let log_path = install_fake_codex_app_server(directory.path());
    seed_codex_token(directory.path());

    // A prompt-only session records a scratch working directory that is
    // deleted when the run ends; a real directory keeps the resume valid,
    // and the execution policy must admit it.
    let workspace = directory.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace should be creatable");
    let config_dir = crate::support::ai_fuel_config_dir(directory.path());
    fs::create_dir_all(&config_dir).expect("config dir should be creatable");
    fs::write(
        config_dir.join("execution.json"),
        serde_json::json!({"policy": {"allowed_roots": [workspace]}}).to_string(),
    )
    .expect("execution policy should be writable");

    let first = aifuel(&directory)
        .args([
            "run",
            "--provider",
            "codex",
            "--model",
            "test-model",
            "--prompt",
            "hello",
            "--working-directory",
            workspace.to_str().expect("workspace is UTF-8"),
        ])
        .env("AIFUEL_CODEX_FIXTURE_LOG", &log_path)
        .output()
        .expect("first run should start");
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );

    // Claude is discovered too - and would rank higher - but a resume pins
    // to the session owner before any quota collection runs.
    seed_claude_authentication(directory.path());

    let output = aifuel(&directory)
        .args([
            "run",
            "--provider",
            "auto",
            "--resume",
            "fixture-thread",
            "--prompt",
            "continue",
            "--output",
            "json",
        ])
        .env("AIFUEL_CODEX_FIXTURE_LOG", &log_path)
        .output()
        .expect("resume should start");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("auto resume should emit JSON");
    assert_eq!(value["provider"], "codex");
    assert_eq!(value["routing"]["requested"], "auto");
    assert_eq!(value["routing"]["selected"], "codex");
    assert_eq!(value["routing"]["resumed"], true);
    assert_eq!(
        value["routing"]["candidates"],
        serde_json::json!([
            {"provider": "codex", "integration": "codex", "basis": "session"}
        ]),
        "a resumed run is pinned, not ranked: {stdout}"
    );
    let attempts = value["routing"]["attempts"]
        .as_array()
        .expect("routing attempts should be an array");
    assert_eq!(attempts.len(), 1, "{stdout}");
    assert_eq!(attempts[0]["provider"], "codex");
    assert_eq!(attempts[0]["outcome"], "succeeded");

    let requests = fs::read_to_string(&log_path).expect("requests should be logged");
    assert!(
        requests.contains("thread/resume"),
        "the session owner should resume the thread: {requests}"
    );
}

/// `--model` narrows `auto` to providers whose cached catalog advertises
/// the model: Claude ranks higher on quota but advertises nothing, so the
/// only candidate is Codex.
#[test]
fn auto_run_model_filter_keeps_only_advertising_providers() {
    let directory = TestDirectory::new("auto-model-filter");
    let log_path = install_fake_codex_app_server(directory.path());
    seed_codex_token(directory.path());
    seed_claude_authentication(directory.path());

    let catalog = aifuel(&directory)
        .args(["model", "refresh", "--provider", "codex"])
        .env("PATH", path_with(directory.path()))
        .env(
            "AIFUEL_CODEX_FIXTURE_CATALOG",
            r#"{"models":[{"slug":"fixture-model"}]}"#,
        )
        .output()
        .expect("model refresh should start");
    assert!(
        catalog.status.success(),
        "{}",
        String::from_utf8_lossy(&catalog.stderr)
    );

    let (claude_url, claude_server) = start_json_fixture(
        r#"{"five_hour":{"remaining_percentage":90,"resets_at":"2030-01-01T00:00:00Z"}}"#,
        1,
    );
    let output = aifuel(&directory)
        .args([
            "run",
            "--provider",
            "auto",
            "--model",
            "fixture-model",
            "--prompt",
            "hello",
            "--output",
            "json",
        ])
        .env("AIFUEL_CLAUDE_USAGE_URL", &claude_url)
        .env("AIFUEL_CODEX_FIXTURE_LOG", &log_path)
        .output()
        .expect("aifuel should start");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("auto run should emit JSON");
    assert_eq!(value["provider"], "codex");
    assert_eq!(
        value["routing"]["candidates"],
        serde_json::json!([
            {"provider": "codex", "integration": "codex", "basis": "quota"}
        ]),
        "only Codex advertises fixture-model: {stdout}"
    );
    let attempts = value["routing"]["attempts"]
        .as_array()
        .expect("routing attempts should be an array");
    assert_eq!(attempts.len(), 1, "{stdout}");
    assert_eq!(attempts[0]["provider"], "codex");
    drop(claude_server);
}

/// With nothing on disk `auto` honestly reports that no provider was
/// discovered rather than picking a literal `auto` integration.
#[test]
fn auto_run_reports_when_no_provider_is_discovered() {
    let directory = TestDirectory::new("auto-none-discovered");
    let output = aifuel(&directory)
        .args(["run", "--provider", "auto", "--prompt", "hello"])
        .output()
        .expect("aifuel should start");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("found no Discovered Provider"),
        "expected the discovery guidance, got {stderr:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).is_empty(),
        "nothing ran, so stdout stays empty"
    );
}

/// A keyed API-key integration is a valid `auto` candidate on credential
/// evidence alone - a Groq key works without any subscription. The
/// routing report explains each candidate's basis: measured quota
/// headroom first, then keyed API-key integrations with a documented
/// free tier ahead of paid keys. Here Claude's launch error moves the
/// chain to Codex, so the Groq and xAI candidates are ranked but never
/// attempted.
#[test]
fn auto_run_ranks_keyed_api_key_integrations_after_quota_headroom() {
    let directory = TestDirectory::new("auto-api-key-ranking");
    let log_path = install_fake_codex_app_server(directory.path());
    seed_codex_token(directory.path());
    seed_claude_authentication(directory.path());
    let (claude_url, claude_server) = start_json_fixture(
        r#"{"five_hour":{"remaining_percentage":90,"resets_at":"2030-01-01T00:00:00Z"}}"#,
        1,
    );
    let (codex_url, codex_server) = start_json_fixture(
        r#"{"rate_limit":{"primary_window":{"limit_window_seconds":18000,"used_percent":40,"reset_at":4102444800}}}"#,
        1,
    );

    let output = aifuel(&directory)
        .args([
            "run",
            "--provider",
            "auto",
            "--prompt",
            "hello",
            "--output",
            "json",
        ])
        .env("AIFUEL_CLAUDE_USAGE_URL", &claude_url)
        .env("AIFUEL_CODEX_USAGE_URL", &codex_url)
        .env("AIFUEL_CODEX_FIXTURE_LOG", &log_path)
        .env("GROQ_API_KEY", "gsk-fake")
        .env("XAI_API_KEY", "xai-fake")
        .output()
        .expect("aifuel should start");
    claude_server.join().expect("claude fixture should answer");
    codex_server.join().expect("codex fixture should answer");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("auto run should emit JSON");
    assert_eq!(
        value["routing"]["candidates"],
        serde_json::json!([
            {
                "provider": "claude",
                "integration": "claude",
                "basis": "quota",
                "remaining_percent": 90.0,
            },
            {
                "provider": "codex",
                "integration": "codex",
                "basis": "quota",
                "remaining_percent": 60.0,
            },
            {
                "provider": "groq",
                "integration": "groq:api-key",
                "basis": "free_tier",
            },
            {
                "provider": "xai",
                "integration": "xai:api-key",
                "basis": "api_key",
            },
        ]),
        "{stdout}"
    );
    // The chain settled on subscription headroom first: the keyed
    // integrations were candidates but never reached.
    let attempts = value["routing"]["attempts"]
        .as_array()
        .expect("routing attempts should be an array");
    assert_eq!(attempts.len(), 2, "{stdout}");
    assert_eq!(attempts[0]["provider"], "claude");
    assert_eq!(attempts[0]["outcome"], "launch_error");
    assert_eq!(attempts[1]["provider"], "codex");
    assert_eq!(attempts[1]["outcome"], "succeeded");
}

/// A Groq key alone makes `auto` work: with no subscription markers the
/// integration's credential is still candidacy evidence, and the chain
/// tries it. The request carries no model, so the attempt reports the
/// endpoint's model requirement instead of guessing one.
#[test]
fn auto_run_attempts_a_keyed_api_key_integration_without_subscriptions() {
    let directory = TestDirectory::new("auto-api-key-only");
    let output = aifuel(&directory)
        .args([
            "run",
            "--provider",
            "auto",
            "--prompt",
            "hello",
            "--output",
            "json",
        ])
        .env("GROQ_API_KEY", "gsk-fake")
        .output()
        .expect("aifuel should start");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr} {stdout}");
    assert!(
        stderr.contains("auto selected groq"),
        "the keyed integration should be the selected candidate: {stderr}"
    );
    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("an exhausted chain still emits JSON");
    assert!(value["error"].is_string(), "{stdout}");
    assert_eq!(
        value["routing"]["candidates"],
        serde_json::json!([
            {
                "provider": "groq",
                "integration": "groq:api-key",
                "basis": "free_tier",
            }
        ]),
        "{stdout}"
    );
    let attempts = value["routing"]["attempts"]
        .as_array()
        .expect("routing attempts should be an array");
    assert_eq!(attempts.len(), 1, "{stdout}");
    assert_eq!(attempts[0]["provider"], "groq");
    assert_eq!(attempts[0]["integration"], "groq:api-key");
    assert_eq!(attempts[0]["outcome"], "launch_error");
    assert!(
        attempts[0]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("requires a model")),
        "the attempt should explain why it could not launch: {stdout}"
    );
}
