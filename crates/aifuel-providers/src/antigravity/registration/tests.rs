use super::*;
use aifuel_core::{AIFUEL_GATEWAY_REGISTRATION_NAME, AgentMcpRegistrationAdapter};

#[test]
fn antigravity_registration_targets_global_config_and_gateway_command() {
    let user_home = Path::new("/tmp/antigravity-home");
    let executable = Path::new("/opt/AI Fuel/aifuel");

    assert_eq!(ADAPTER.host_id(), "antigravity");
    assert_eq!(
        ADAPTER.configuration_home(user_home).unwrap(),
        user_home.join(".gemini").join("config")
    );
    assert_eq!(
        ADAPTER.config_file(&user_home.join(".gemini").join("config")),
        user_home
            .join(".gemini")
            .join("config")
            .join("mcp_config.json")
    );
    assert_eq!(
        ADAPTER.expected_entry(executable).unwrap(),
        serde_json::json!({
            "command": "/opt/AI Fuel/aifuel",
            "args": ["mcp", "gateway", "--agent", "antigravity"],
            "disabled": false
        })
    );
}

#[test]
fn write_and_remove_preserve_other_antigravity_mcp_servers() {
    let original = br#"{
  "settings": {"theme": "light"},
  "mcpServers": {
    "docs": {"command": "docs-server", "args": []}
  }
}"#;
    let executable = Path::new("/opt/aifuel");

    let updated = ADAPTER.write_entry(Some(original), executable).unwrap();
    let value: Value = serde_json::from_slice(&updated).unwrap();
    assert_eq!(value["settings"]["theme"], "light");
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
}

#[test]
fn semantic_equality_ignores_json_format_and_key_order_but_keeps_argument_order() {
    let expected = ADAPTER.expected_entry(Path::new("/opt/aifuel")).unwrap();
    let matching = br#"{
      "mcpServers": {
        "aifuel-gateway": {
          "disabled": false,
          "args": ["mcp", "gateway", "--agent", "antigravity"],
          "command": "/opt/aifuel"
        }
      }
    }"#;
    let changed_args = br#"{
      "mcpServers": {
        "aifuel-gateway": {
          "command": "/opt/aifuel",
          "args": ["mcp", "gateway", "antigravity", "--agent"],
          "disabled": false
        }
      }
    }"#;

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
fn malformed_or_incompatible_antigravity_json_fails_closed() {
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
}
