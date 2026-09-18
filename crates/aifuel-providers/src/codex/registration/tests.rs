use super::*;
use aifuel_core::{AIFUEL_GATEWAY_REGISTRATION_NAME, AgentMcpRegistrationAdapter};

#[test]
fn codex_registration_uses_the_documented_user_config_and_gateway_command() {
    let host_home = Path::new("/tmp/codex-home");
    let executable = Path::new("/opt/AI Fuel/aifuel");

    assert_eq!(CODEX_REGISTRATION.host_id(), "codex");
    assert_eq!(
        CODEX_REGISTRATION.config_file(host_home),
        host_home.join("config.toml")
    );
    let expected = CODEX_REGISTRATION.expected_entry(executable).unwrap();
    let table = expected["value"].as_object().unwrap();

    assert_eq!(expected["type"], "table");
    assert_eq!(table["command"]["value"], executable.to_str().unwrap());
    assert_eq!(
        table["args"]["value"],
        serde_json::json!([
            {"type":"string", "value":"mcp"},
            {"type":"string", "value":"gateway"},
            {"type":"string", "value":"--agent"},
            {"type":"string", "value":"codex"}
        ])
    );
}

#[test]
fn writes_only_the_gateway_entry_and_preserves_comments_and_other_servers() {
    let original = br#"# Keep the user's model setting.
model = "gpt-5"

[mcp_servers.docs]
command = "docs-server"
args = ["--mode", "local"]
"#;
    let executable = Path::new("/opt/aifuel");

    let updated = CODEX_REGISTRATION
        .write_entry(Some(original), executable)
        .unwrap();
    let text = std::str::from_utf8(&updated).unwrap();
    let document = DocumentMut::from_str(text).unwrap();
    let servers = document["mcp_servers"].as_table().unwrap();
    let gateway = servers
        .get(AIFUEL_GATEWAY_REGISTRATION_NAME)
        .unwrap()
        .as_table()
        .unwrap();

    assert!(text.contains("# Keep the user's model setting."));
    assert_eq!(document["model"].as_str(), Some("gpt-5"));
    assert_eq!(servers["docs"]["command"].as_str(), Some("docs-server"));
    assert_eq!(gateway["command"].as_str(), Some("/opt/aifuel"));
    let args: Vec<_> = gateway["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(toml_edit::Value::as_str)
        .collect();
    assert_eq!(
        args,
        vec![Some("mcp"), Some("gateway"), Some("--agent"), Some("codex")]
    );
    assert_eq!(
        CODEX_REGISTRATION.current_entry(Some(original)).unwrap(),
        None
    );
    assert_eq!(
        CODEX_REGISTRATION.current_entry(Some(&updated)).unwrap(),
        Some(CODEX_REGISTRATION.expected_entry(executable).unwrap())
    );
}

#[test]
fn semantic_entry_equality_ignores_formatting_and_key_order_but_keeps_args_order() {
    let executable = Path::new("/opt/aifuel");
    let expected = CODEX_REGISTRATION.expected_entry(executable).unwrap();
    let same_entry = br#"[mcp_servers.aifuel-gateway]
args = [ "mcp", "gateway", "--agent", "codex" ]
command = "/opt/aifuel" # harmless formatting
"#;
    let reordered_args = br#"[mcp_servers.aifuel-gateway]
command = "/opt/aifuel"
args = ["mcp", "gateway", "codex", "--agent"]
"#;

    assert_eq!(
        CODEX_REGISTRATION.current_entry(Some(same_entry)).unwrap(),
        Some(expected.clone())
    );
    assert_ne!(
        CODEX_REGISTRATION
            .current_entry(Some(reordered_args))
            .unwrap(),
        Some(expected)
    );
}

#[test]
fn removal_leaves_other_servers_and_unrelated_settings_unchanged() {
    let executable = Path::new("/opt/aifuel");
    let original = CODEX_REGISTRATION
        .write_entry(Some(b"theme = \"light\"\n"), executable)
        .unwrap();
    let removed = CODEX_REGISTRATION.remove_entry(&original).unwrap();
    let document = DocumentMut::from_str(std::str::from_utf8(&removed).unwrap()).unwrap();

    assert_eq!(document["theme"].as_str(), Some("light"));
    assert!(document["mcp_servers"].as_table().unwrap().is_empty());
    assert_eq!(
        CODEX_REGISTRATION.current_entry(Some(&removed)).unwrap(),
        None
    );
}

#[test]
fn malformed_or_incompatible_codex_config_fails_without_silent_replacement() {
    assert!(
        CODEX_REGISTRATION
            .current_entry(Some(b"[mcp_servers\n"))
            .is_err()
    );
    assert!(
        CODEX_REGISTRATION
            .write_entry(
                Some(b"mcp_servers = \"not a table\"\n"),
                Path::new("/opt/aifuel")
            )
            .is_err()
    );
}
