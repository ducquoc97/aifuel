use aifuel_app::{McpGatewayFacade, McpServerDefinition};
use std::path::PathBuf;

#[test]
fn gateway_facade_uses_an_exact_host_selection_and_default_inheritance() {
    let catalog = br#"{
        "servers": {
            "docs": {"transport":"stdio","command":"docs-mcp"},
            "local": {"transport":"stdio","command":"local-mcp"}
        },
        "defaults": ["docs"],
        "agents": {
            "codex": {"servers":["local"]},
            "isolated": {"servers":[]}
        }
    }"#;
    let home = PathBuf::from("/home/test");

    let codex = McpGatewayFacade::from_json(catalog, "codex", &home)
        .expect("Codex host selection should resolve");
    let claude = McpGatewayFacade::from_json(catalog, "claude", &home)
        .expect("missing host entry should inherit defaults");
    let isolated = McpGatewayFacade::from_json(catalog, "isolated", &home)
        .expect("empty host entry should be valid");

    assert_eq!(codex.selected_servers()[0].id, "local");
    assert_eq!(claude.selected_servers()[0].id, "docs");
    assert!(isolated.selected_servers().is_empty());
}

#[test]
fn explicit_host_selection_does_not_change_when_defaults_change() {
    let first_catalog = br#"{
        "servers": {
            "docs": {"transport":"stdio","command":"docs-mcp"},
            "memory": {"transport":"stdio","command":"memory-mcp"}
        },
        "defaults": ["docs"],
        "agents": {"codex": {"servers":["memory"]}}
    }"#;
    let next_catalog = br#"{
        "servers": {
            "docs": {"transport":"stdio","command":"docs-mcp"},
            "memory": {"transport":"stdio","command":"memory-mcp"}
        },
        "defaults": ["memory"],
        "agents": {"codex": {"servers":["memory"]}}
    }"#;

    let before = McpGatewayFacade::from_json(first_catalog, "codex", "/home/test")
        .expect("first catalog should be valid");
    let after = McpGatewayFacade::from_json(next_catalog, "codex", "/home/test")
        .expect("updated defaults should keep the catalog valid");

    let before_ids = before
        .selected_servers()
        .iter()
        .map(|server| server.id.as_str())
        .collect::<Vec<_>>();
    let after_ids = after
        .selected_servers()
        .iter()
        .map(|server| server.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(before_ids, ["memory"]);
    assert_eq!(after_ids, ["memory"]);
}

#[test]
fn gateway_facade_rejects_dangling_server_references() {
    let catalog = br#"{
        "servers": {},
        "defaults": ["missing"]
    }"#;

    let error = McpGatewayFacade::from_json(catalog, "codex", PathBuf::from("/home/test"))
        .expect_err("dangling defaults must invalidate the catalog");

    assert!(error.to_string().contains("defaults"));
    assert!(error.to_string().contains("missing"));
}

#[test]
fn gateway_facade_rejects_host_message_limits_above_eight_mib() {
    let catalog = br#"{"gateway":{"maxMessageBytes":8388609}}"#;

    let error = McpGatewayFacade::from_json(catalog, "codex", "/home/test")
        .expect_err("host messages must stay within the approved 8 MiB ceiling");

    assert!(error.to_string().contains("gateway limits"));
}

#[test]
fn gateway_facade_keeps_only_the_bearer_environment_reference() {
    let catalog = br#"{
        "servers": {
            "private": {
                "transport":"streamable-http",
                "url":"https://example.test/mcp",
                "auth":{"bearerTokenEnv":"PRIVATE_MCP_TOKEN"}
            }
        },
        "defaults":["private"]
    }"#;

    let facade = McpGatewayFacade::from_json(catalog, "codex", "/home/test")
        .expect("the bearer environment reference should be valid");
    let McpServerDefinition::StreamableHttp(server) = &facade.selected_servers()[0].definition
    else {
        panic!("the catalog should select a remote server");
    };
    assert_eq!(
        server
            .auth
            .as_ref()
            .expect("bearer authentication should be present")
            .bearer_token_env,
        "PRIVATE_MCP_TOKEN"
    );
}

#[test]
fn gateway_facade_rejects_literal_or_unknown_remote_authentication() {
    for auth in [
        r#"{"bearerToken":"literal-secret"}"#,
        r#"{"unknown":"PRIVATE_MCP_TOKEN"}"#,
    ] {
        let catalog = format!(
            r#"{{"servers":{{"private":{{"transport":"streamable-http","url":"https://example.test/mcp","auth":{auth}}}}},"defaults":["private"]}}"#
        );
        let error = McpGatewayFacade::from_json(catalog.as_bytes(), "codex", "/home/test")
            .expect_err("unsupported authentication fields must be rejected");
        assert!(error.to_string().contains("invalid MCP gateway JSON"));
    }
}

#[test]
fn gateway_facade_rejects_an_empty_bearer_environment_reference() {
    let catalog = br#"{
        "servers": {
            "private": {
                "transport":"streamable-http",
                "url":"https://example.test/mcp",
                "auth":{"bearerTokenEnv":"  "}
            }
        },
        "defaults":["private"]
    }"#;

    let error = McpGatewayFacade::from_json(catalog, "codex", "/home/test")
        .expect_err("empty bearer environment references must be rejected");
    assert!(error.to_string().contains("bearerTokenEnv"));
}
