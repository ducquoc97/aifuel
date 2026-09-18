use super::*;
use aifuel_core::{AIFUEL_GATEWAY_REGISTRATION_NAME, AgentMcpRegistrationAdapter};

#[test]
fn copilot_registration_uses_its_documented_user_config_and_gateway_command() {
    let host_home = Path::new("/tmp/copilot-home");
    let executable = Path::new("/opt/AI Fuel/aifuel");

    assert_eq!(COPILOT_REGISTRATION.host_id(), "copilot");
    assert_eq!(
        COPILOT_REGISTRATION.config_file(host_home),
        host_home.join("mcp-config.json")
    );
    assert_eq!(
        COPILOT_REGISTRATION.expected_entry(executable).unwrap(),
        serde_json::json!({
            "type": "local",
            "command": executable.to_str().unwrap(),
            "args": ["mcp", "gateway", "--agent", "copilot"],
            "env": {"XDG_CONFIG_HOME": "${XDG_CONFIG_HOME}"},
            "tools": ["*"]
        })
    );
}

#[test]
fn copilot_configuration_home_defaults_to_dot_copilot_and_accepts_an_absolute_override() {
    let user_home = Path::new("/tmp/user-home");
    let override_home = PathBuf::from("/tmp/custom-copilot-home");

    assert_eq!(
        resolve_configuration_home(user_home, None).unwrap(),
        user_home.join(".copilot")
    );
    assert_eq!(
        resolve_configuration_home(user_home, Some(override_home.clone())).unwrap(),
        override_home
    );
    assert!(resolve_configuration_home(user_home, Some(PathBuf::from("relative-home"))).is_err());
}

#[test]
fn writes_only_the_gateway_entry_and_preserves_other_servers_and_settings() {
    let original = br#"{
  "appearance": {"theme": "light"},
  "mcpServers": {
    "docs": {"type": "local", "command": "docs-server", "args": ["--mode", "local"]}
  }
}"#;
    let executable = Path::new("/opt/aifuel");

    let updated = COPILOT_REGISTRATION
        .write_entry(Some(original), executable)
        .unwrap();
    let document: JsonValue = serde_json::from_slice(&updated).unwrap();
    let servers = document["mcpServers"].as_object().unwrap();

    assert_eq!(document["appearance"]["theme"], "light");
    assert_eq!(servers["docs"]["command"], "docs-server");
    assert_eq!(
        servers[AIFUEL_GATEWAY_REGISTRATION_NAME],
        COPILOT_REGISTRATION.expected_entry(executable).unwrap()
    );
    assert_eq!(
        COPILOT_REGISTRATION.current_entry(Some(original)).unwrap(),
        None
    );
    assert_eq!(
        COPILOT_REGISTRATION.current_entry(Some(&updated)).unwrap(),
        Some(COPILOT_REGISTRATION.expected_entry(executable).unwrap())
    );
}

#[test]
fn semantic_entry_equality_ignores_json_format_and_object_key_order_but_keeps_args_order() {
    let executable = Path::new("/opt/aifuel");
    let expected = COPILOT_REGISTRATION.expected_entry(executable).unwrap();
    let same_entry = br#"{
      "mcpServers": {
        "aifuel-gateway": {
          "tools": ["*"],
          "env": {"XDG_CONFIG_HOME": "${XDG_CONFIG_HOME}"},
          "args": ["mcp", "gateway", "--agent", "copilot"],
          "command": "/opt/aifuel",
          "type": "local"
        }
      }
    }"#;
    let reordered_args = br#"{
      "mcpServers": {
        "aifuel-gateway": {
          "type": "local",
          "command": "/opt/aifuel",
          "args": ["mcp", "gateway", "copilot", "--agent"],
          "env": {"XDG_CONFIG_HOME": "${XDG_CONFIG_HOME}"},
          "tools": ["*"]
        }
      }
    }"#;

    assert_eq!(
        COPILOT_REGISTRATION
            .current_entry(Some(same_entry))
            .unwrap(),
        Some(expected.clone())
    );
    assert_ne!(
        COPILOT_REGISTRATION
            .current_entry(Some(reordered_args))
            .unwrap(),
        Some(expected)
    );
}

#[test]
fn removal_preserves_other_servers_and_settings() {
    let executable = Path::new("/opt/aifuel");
    let original = COPILOT_REGISTRATION
        .write_entry(Some(br#"{"setting":true}"#), executable)
        .unwrap();

    let removed = COPILOT_REGISTRATION.remove_entry(&original).unwrap();
    let document: JsonValue = serde_json::from_slice(&removed).unwrap();

    assert_eq!(document["setting"], true);
    assert!(document["mcpServers"].as_object().unwrap().is_empty());
    assert_eq!(
        COPILOT_REGISTRATION.current_entry(Some(&removed)).unwrap(),
        None
    );
}

#[test]
fn malformed_or_incompatible_copilot_config_fails_without_replacement() {
    assert!(
        COPILOT_REGISTRATION
            .current_entry(Some(b"{\"mcpServers\":"))
            .is_err()
    );
    assert!(
        COPILOT_REGISTRATION
            .write_entry(
                Some(br#"{"mcpServers":"not an object"}"#),
                Path::new("/opt/aifuel")
            )
            .is_err()
    );
    assert!(COPILOT_REGISTRATION.current_entry(Some(br#"[]"#)).is_err());
    assert!(
        COPILOT_REGISTRATION
            .write_entry(Some(br#"[]"#), Path::new("/opt/aifuel"))
            .is_err()
    );
}
