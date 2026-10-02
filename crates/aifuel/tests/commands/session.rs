use std::io::Write;
use std::process::{Command, Output, Stdio};

use crate::support::{TestDirectory, ai_fuel_config_dir, start_claude_web_fixture};

/// Run `aifuel …` against an isolated home so the real credential store and
/// environment stay untouched. `CLAUDE_WEB_SESSION` is stripped so a host
/// value cannot flip the reported source.
fn aifuel(directory: &TestDirectory, args: &[&str]) -> Output {
    command(directory, args)
        .output()
        .expect("aifuel should start")
}

fn command(directory: &TestDirectory, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aifuel"));
    command
        .args(args)
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"));
    for var in [
        "CLAUDE_WEB_SESSION",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "OPENROUTER_API_KEY",
    ] {
        command.env_remove(var);
    }
    command
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// `aifuel auth set-session` stores one `"session"` record for the web
/// integration, `auth list` reports it as a session in text and JSON, and
/// the material never appears in output.
#[test]
fn set_session_stores_a_session_record_and_list_reports_it() {
    let directory = TestDirectory::new("auth-session");
    let secret = "sk-ant-session-secret";

    let output = aifuel(
        &directory,
        &["auth", "set-session", "claude-web:web", "--key", secret],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains(
            "Stored session credential claude-web:web bound to integration claude-web:web"
        ),
        "{}",
        stdout(&output)
    );
    assert!(!stdout(&output).contains(secret) && !stderr(&output).contains(secret));

    // One session record on disk; no pool members exist for it.
    let store_file = ai_fuel_config_dir(directory.path()).join("credentials.json");
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(store_file).expect("credentials.json exists"))
            .expect("credentials.json parses");
    assert_eq!(stored["credentials"]["claude-web:web"]["type"], "session");
    assert!(stored["credentials"]["claude-web:web/2"].is_null());
    assert_eq!(
        stored["credentials"]["claude-web:web"]["destination"],
        "claude-web:web"
    );

    // Text and JSON views report the session binding without material.
    let list = aifuel(&directory, &["auth", "list"]);
    assert!(list.status.success(), "{}", stderr(&list));
    let listed = stdout(&list);
    assert!(
        listed.contains("managed session claude-web:web (present)"),
        "{listed}"
    );
    assert!(listed.contains("session"), "{listed}");
    assert!(
        !listed.contains(secret),
        "session material leaked in {listed}"
    );

    let json = aifuel(&directory, &["auth", "list", "--json"]);
    assert!(json.status.success(), "{}", stderr(&json));
    let value: serde_json::Value =
        serde_json::from_slice(&json.stdout).expect("auth list --json emits JSON");
    let entry = value["credentials"]
        .as_array()
        .expect("credentials is an array")
        .iter()
        .find(|entry| entry["credential"] == "claude-web:web")
        .expect("the stored session is listed");
    assert_eq!(entry["kind"], "session", "{value}");
    assert_eq!(entry["health"], "none", "{value}");
    assert!(entry["pool"].is_null(), "{value}");
    assert!(
        !stdout(&json).contains(secret),
        "session material leaked in JSON output"
    );
}

/// `--stdin` is the paste path that keeps the session out of shell history;
/// a copied `Cookie:` header line is stored verbatim and normalized only at
/// send time.
#[test]
fn set_session_reads_material_from_stdin() {
    let directory = TestDirectory::new("auth-session-stdin");
    let material = "Cookie: sessionKey=abc123; other=value\n";

    let mut child = command(
        &directory,
        &["auth", "set-session", "claude-web:web", "--stdin"],
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("aifuel should start");
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(material.as_bytes())
        .expect("stdin is writable");
    let output = child.wait_with_output().expect("aifuel should exit");
    assert!(output.status.success(), "{}", stderr(&output));

    let store_file = ai_fuel_config_dir(directory.path()).join("credentials.json");
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(store_file).expect("credentials.json exists"))
            .expect("credentials.json parses");
    // Stored verbatim (minus the piped trailing newline); send-time
    // normalization lives in `cookie_header_value`.
    assert_eq!(
        stored["credentials"]["claude-web:web"]["key"],
        "Cookie: sessionKey=abc123; other=value"
    );
}

/// Kind-mismatch refusals: `set-session` on a key-delivered binding and
/// `set-key` on a cookie-delivered one each point at the right command
/// rather than storing the wrong record kind.
#[test]
fn set_session_and_set_key_refuse_each_others_targets() {
    let directory = TestDirectory::new("auth-session-mismatch");

    let wrong = aifuel(
        &directory,
        &["auth", "set-session", "openai:api-key", "--key", "s"],
    );
    assert!(!wrong.status.success());
    assert!(
        stderr(&wrong).contains("takes an API key, not a session credential"),
        "{}",
        stderr(&wrong)
    );

    let wrong = aifuel(
        &directory,
        &["auth", "set-key", "claude-web:web", "--key", "k"],
    );
    assert!(!wrong.status.success());
    assert!(
        stderr(&wrong).contains("takes a session credential, not an API key"),
        "{}",
        stderr(&wrong)
    );

    // Nothing was written by the refusals.
    let store_file = ai_fuel_config_dir(directory.path()).join("credentials.json");
    assert!(
        !store_file.exists(),
        "a refused store wrote credentials.json"
    );
}

/// Removing a stored session warns that the binding strands until a
/// replacement is stored or the declared env var is exported - no pool
/// retention note, because a session is a single record.
#[test]
fn remove_session_warns_the_binding_strands() {
    let directory = TestDirectory::new("auth-session-remove");
    let output = aifuel(
        &directory,
        &["auth", "set-session", "claude-web:web", "--key", "s"],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let removed = aifuel(&directory, &["auth", "remove", "claude-web:web"]);
    assert!(removed.status.success(), "{}", stderr(&removed));
    assert!(
        stderr(&removed).contains(
            "binds session credential claude-web:web and will fail authentication until a \
             replacement is stored or CLAUDE_WEB_SESSION is exported"
        ),
        "{}",
        stderr(&removed)
    );
    assert!(!stderr(&removed).contains("pool"), "{}", stderr(&removed));

    let list = aifuel(&directory, &["auth", "list"]);
    assert!(
        stdout(&list).contains("managed session claude-web:web (absent)"),
        "{}",
        stdout(&list)
    );
}

/// The stored session reaches the collector as a `Cookie` header - never
/// `Authorization` - and the usage windows land as typed observations. The
/// local fixture stands in for `GET /api/organizations` and
/// `GET /api/organizations/{uuid}/usage`.
#[test]
fn status_collects_claude_web_usage_over_the_stored_session() {
    let directory = TestDirectory::new("claude-web-status");
    let secret = "sk-ant-session-e2e";
    let output = aifuel(
        &directory,
        &["auth", "set-session", "claude-web:web", "--key", secret],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let (url, requests, server) = start_claude_web_fixture(
        r#"[{"uuid":"org-1","name":"Fixture Org"}]"#,
        r#"{"five_hour":{"utilization":17.0,"resets_at":"2030-01-01T18:59:59Z"},"seven_day":{"utilization":11.0,"resets_at":"2030-01-07T16:59:59Z"}}"#,
    );
    let output = command(&directory, &["--json"])
        .env("AIFUEL_CLAUDE_WEB_USAGE_URL", &url)
        .output()
        .expect("aifuel should start");
    server
        .join()
        .expect("the fixture should answer both requests");

    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("status emits JSON");
    assert!(
        output.status.success(),
        "collection errors: {}",
        stderr(&output)
    );
    let observations: Vec<&serde_json::Value> = report["observations"]
        .as_array()
        .expect("observations is an array")
        .iter()
        .filter(|observation| observation["integration_id"] == "claude-web:web")
        .collect();
    let five_hour = observations
        .iter()
        .find(|observation| observation["quota_pool_id"] == "claude-web:web:usage:5h")
        .expect("the 5-hour window is observed");
    assert_eq!(five_hour["state"], "observed", "{report}");
    assert_eq!(five_hour["used_percent"], 17.0, "{report}");
    assert_eq!(five_hour["remaining_percent"], 83.0, "{report}");
    let seven_day = observations
        .iter()
        .find(|observation| observation["quota_pool_id"] == "claude-web:web:usage:7d")
        .expect("the 7-day window is observed");
    assert_eq!(seven_day["remaining_percent"], 89.0, "{report}");

    // Both requests carried the session as a Cookie header, normalized from
    // the bare stored value, and never an Authorization header.
    let requests = requests.lock().expect("captured requests");
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(requests[1].starts_with("GET /org-1/usage "), "{requests:?}");
    for request in requests.iter() {
        // HTTP/1.1 header names are case-insensitive and reqwest emits them
        // lowercase on the wire, so both sides compare lowercased.
        assert!(
            request
                .to_lowercase()
                .contains(&format!("cookie: sessionKey={secret}").to_lowercase()),
            "the session must travel as a Cookie header: {request}"
        );
        assert!(
            !request.to_lowercase().contains("authorization:"),
            "session material must never become a Bearer token: {request}"
        );
    }
}

/// The declared `CLAUDE_WEB_SESSION` env var is itself a credential source:
/// with no stored record the collector resolves it through EnvOrStore.
#[test]
fn status_collects_claude_web_usage_from_the_env_var() {
    let directory = TestDirectory::new("claude-web-env");
    let (url, _requests, server) = start_claude_web_fixture(
        r#"[{"uuid":"org-1"}]"#,
        r#"{"five_hour":{"utilization":50.0,"resets_at":"2030-01-01T18:59:59Z"}}"#,
    );
    let output = command(&directory, &["--json"])
        .env("CLAUDE_WEB_SESSION", "env-session")
        .env("AIFUEL_CLAUDE_WEB_USAGE_URL", &url)
        .output()
        .expect("aifuel should start");
    server
        .join()
        .expect("the fixture should answer both requests");

    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("status emits JSON");
    let observations: Vec<&serde_json::Value> = report["observations"]
        .as_array()
        .expect("observations is an array")
        .iter()
        .filter(|observation| observation["integration_id"] == "claude-web:web")
        .collect();
    assert!(
        observations
            .iter()
            .all(|observation| observation["state"] == "observed"),
        "{report}"
    );
    assert!(!observations.is_empty(), "{report}");
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("env-session"),
        "session material leaked into the report"
    );
}

/// With no session anywhere the integration reports `unauthenticated`
/// honestly rather than fabricating values - and it is a state, not a
/// collection error.
#[test]
fn status_reports_claude_web_unauthenticated_without_a_session() {
    let directory = TestDirectory::new("claude-web-absent");
    let output = command(&directory, &["--json"])
        .output()
        .expect("aifuel should start");
    assert!(output.status.success(), "{}", stderr(&output));
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("status emits JSON");
    let observations: Vec<&serde_json::Value> = report["observations"]
        .as_array()
        .expect("observations is an array")
        .iter()
        .filter(|observation| observation["integration_id"] == "claude-web:web")
        .collect();
    assert_eq!(observations.len(), 1, "{report}");
    assert_eq!(observations[0]["state"], "unauthenticated", "{report}");
    assert!(
        report["collection"]["errors"]
            .as_array()
            .expect("errors is an array")
            .iter()
            .all(|error| error["provider_id"] != "claude-web"),
        "an absent session is not a collection error: {report}"
    );
}
