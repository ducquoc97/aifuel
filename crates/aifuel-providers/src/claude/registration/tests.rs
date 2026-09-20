use super::*;
use aifuel_core::{AIFUEL_GATEWAY_REGISTRATION_NAME, AgentMcpRegistrationAdapter};

#[test]
fn claude_registration_targets_user_scope_and_the_gateway_command() {
    let user_home = Path::new("/tmp/claude-home");
    let executable = Path::new("/opt/AI Fuel/aifuel");

    assert_eq!(ADAPTER.host_id(), "claude");
    assert_eq!(
        ADAPTER.config_file(user_home),
        user_home.join(".claude.json")
    );
    assert_eq!(
        ADAPTER.expected_entry(executable).unwrap(),
        json!({
            "type": "stdio",
            "command": "/opt/AI Fuel/aifuel",
            "args": ["mcp", "gateway", "--agent", "claude"],
            "env": {}
        })
    );
}

#[test]
fn write_and_remove_preserve_other_user_and_project_mcp_settings() {
    let original = br#"{
  "userID": "local-user",
  "projects": {
    "/work/example": {
      "hasTrustDialogAccepted": true,
      "mcpServers": {"project-tools": {"type": "stdio", "command": "project-tool"}}
    }
  },
  "mcpServers": {
    "notes": {"type": "stdio", "command": "notes", "args": [], "env": {}}
  }
}"#;
    let executable = Path::new("/opt/aifuel");

    let updated = ADAPTER.write_entry(Some(original), executable).unwrap();
    let value: Value = serde_json::from_slice(&updated).unwrap();
    assert_eq!(value["userID"], "local-user");
    assert_eq!(
        value["projects"]["/work/example"]["hasTrustDialogAccepted"],
        true
    );
    assert_eq!(
        value["projects"]["/work/example"]["mcpServers"]["project-tools"]["command"],
        "project-tool"
    );
    assert_eq!(value["mcpServers"]["notes"]["command"], "notes");
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
    assert_eq!(value["mcpServers"]["notes"]["command"], "notes");
    assert_eq!(
        value["projects"]["/work/example"]["mcpServers"]["project-tools"]["command"],
        "project-tool"
    );
}

#[test]
fn semantic_equality_ignores_json_format_and_key_order_but_keeps_argument_order() {
    let expected = ADAPTER.expected_entry(Path::new("/opt/aifuel")).unwrap();
    let matching = br#"{"mcpServers":{"aifuel-gateway":{"env":{},"args":["mcp","gateway","--agent","claude"],"command":"/opt/aifuel","type":"stdio"}}}"#;
    let changed_args = br#"{"mcpServers":{"aifuel-gateway":{"type":"stdio","command":"/opt/aifuel","args":["mcp","gateway","claude","--agent"],"env":{}}}}"#;

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
fn malformed_or_incompatible_claude_json_fails_closed() {
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
