use aifuel_app::McpGatewayFacade;
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
