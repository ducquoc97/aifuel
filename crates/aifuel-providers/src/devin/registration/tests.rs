use super::*;
use aifuel_core::{AIFUEL_GATEWAY_REGISTRATION_NAME, AgentMcpRegistrationAdapter};

#[test]
fn devin_registration_targets_user_scope_and_the_gateway_command() {
    let user_home = Path::new("/tmp/devin-home");
    let executable = Path::new("/opt/AI Fuel/aifuel");

    assert_eq!(ADAPTER.host_id(), "devin");
    assert_eq!(
        ADAPTER.config_file(user_home),
        user_home
            .join(".config")
            .join("devin")
            .join("mcp_config.json")
    );
    assert_eq!(
        ADAPTER.expected_entry(executable).unwrap(),
        json!({
            "command": "/opt/AI Fuel/aifuel",
            "args": ["mcp", "gateway", "--agent", "devin"],
            "transport": "stdio"
        })
    );
}

#[test]
fn write_and_remove_preserve_other_devin_settings_and_mcp_servers() {
    let original = br#"{
  "settings": {"theme": "dark"},
  "mcpServers": {
    "docs": {"command": "docs-server", "args": [], "transport": "stdio"}
  }
}"#;
    let executable = Path::new("/opt/aifuel");

    let updated = ADAPTER.write_entry(Some(original), executable).unwrap();
    let value: Value = serde_json::from_slice(&updated).unwrap();
    assert_eq!(value["settings"]["theme"], "dark");
    assert_eq!(value["mcpServers"]["docs"]["command"], "docs-server");
    assert_eq!(
        ADAPTER.current_entry(Some(&updated)).unwrap(),
        Some(ADAPTER.expected_entry(executable).unwrap())
    );

    let removed = ADAPTER.remove_entry(&updated).unwrap();
    let value: Value = serde_json::from_slice(&removed).unwrap();
    assert!(
        value["mcpServers"]
            .get(AIFUEL_GATEWAY_REGISTRATION_NAME)
            .is_none()
    );
    assert_eq!(value["mcpServers"]["docs"]["command"], "docs-server");
    assert_eq!(value["settings"]["theme"], "dark");
}

#[test]
fn write_on_absent_config_creates_an_mcp_servers_document() {
    let executable = Path::new("/opt/aifuel");

    let written = ADAPTER.write_entry(None, executable).unwrap();
    let value: Value = serde_json::from_slice(&written).unwrap();
    assert_eq!(
        value,
        json!({
            "mcpServers": {
                "aifuel-gateway": {
                    "command": "/opt/aifuel",
                    "args": ["mcp", "gateway", "--agent", "devin"],
                    "transport": "stdio"
                }
            }
        })
    );
}

#[test]
fn semantic_equality_ignores_json_format_and_key_order_but_keeps_argument_order() {
    let expected = ADAPTER.expected_entry(Path::new("/opt/aifuel")).unwrap();
    let matching = br#"{"mcpServers":{"aifuel-gateway":{"transport":"stdio","args":["mcp","gateway","--agent","devin"],"command":"/opt/aifuel"}}}"#;
    let changed_args = br#"{"mcpServers":{"aifuel-gateway":{"command":"/opt/aifuel","args":["mcp","gateway","devin","--agent"],"transport":"stdio"}}}"#;

    assert_eq!(
        ADAPTER.current_entry(Some(matching)).unwrap(),
        Some(expected.clone())
    );
    assert_ne!(
        ADAPTER.current_entry(Some(changed_args)).unwrap(),
        Some(expected)
    );
}

#[test]
fn malformed_or_incompatible_devin_json_fails_closed() {
    assert!(ADAPTER.current_entry(Some(b"{\n")).is_err());
    assert!(ADAPTER.current_entry(Some(b"[]")).is_err());
    assert!(
        ADAPTER
            .current_entry(Some(br#"{"mcpServers":[]}"#))
            .is_err()
    );
    assert!(
        ADAPTER
            .write_entry(Some(br#"{"mcpServers":false}"#), Path::new("/opt/aifuel"))
            .is_err()
    );
    assert!(ADAPTER.remove_entry(br#"{"mcpServers":[]}"#).is_err());
}
