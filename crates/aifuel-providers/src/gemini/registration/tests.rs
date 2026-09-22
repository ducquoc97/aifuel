use super::*;
use aifuel_core::{AIFUEL_GATEWAY_REGISTRATION_NAME, AgentMcpRegistrationAdapter};

#[test]
fn gemini_registration_targets_settings_json_and_the_gateway_command() {
    let user_home = Path::new("/tmp/gemini-home");
    let executable = Path::new("/opt/AI Fuel/aifuel");

    assert_eq!(ADAPTER.host_id(), "gemini");
    assert_eq!(
        ADAPTER.config_file(user_home),
        user_home.join(".gemini").join("settings.json")
    );
    assert_eq!(
        ADAPTER.expected_entry(executable).unwrap(),
        serde_json::json!({
            "command": "/opt/AI Fuel/aifuel",
            "args": ["mcp", "gateway", "--agent", "gemini"]
        })
    );
}

#[test]
fn write_and_remove_preserve_other_gemini_settings_and_mcp_servers() {
    let original = br#"{
  "theme": "light",
  "mcp": {"allowed": ["docs"]},
  "mcpServers": {
    "docs": {"command": "docs-server", "args": ["--mode", "local"]}
  }
}"#;
    let executable = Path::new("/opt/aifuel");

    let updated = ADAPTER.write_entry(Some(original), executable).unwrap();
    let value: Value = serde_json::from_slice(&updated).unwrap();
    assert_eq!(value["theme"], "light");
    assert_eq!(value["mcp"]["allowed"], serde_json::json!(["docs"]));
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
    assert_eq!(value["mcp"]["allowed"], serde_json::json!(["docs"]));
}

#[test]
fn semantic_equality_ignores_json_format_and_key_order_but_keeps_argument_order() {
    let expected = ADAPTER.expected_entry(Path::new("/opt/aifuel")).unwrap();
    let matching = br#"{
      "mcpServers": {
        "aifuel-gateway": {
          "args": ["mcp", "gateway", "--agent", "gemini"],
          "command": "/opt/aifuel"
        }
      }
    }"#;
    let changed_args = br#"{
      "mcpServers": {
        "aifuel-gateway": {
          "command": "/opt/aifuel",
          "args": ["mcp", "gateway", "gemini", "--agent"]
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
fn malformed_or_incompatible_gemini_json_fails_closed() {
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
