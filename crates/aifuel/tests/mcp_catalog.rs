#[allow(dead_code)]
mod common;

use common::{TestDirectory, run_setup};
use serde_json::{Value, json};
use std::fs;

#[test]
fn central_servers_can_be_added_listed_and_validated_without_launching_them() {
    let root = TestDirectory::new("catalog");
    let definition = root.path().join("server.json");
    fs::write(
        &definition,
        json!({"transport":"stdio","command":"not-installed","envFrom":{"TOKEN":"EXTERNAL_TOKEN"}})
            .to_string(),
    )
    .unwrap();
    let added = run_setup(
        root.path(),
        &[
            "mcp",
            "servers",
            "add",
            "docs",
            "--definition",
            definition.to_str().unwrap(),
        ],
    );
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let listed = run_setup(root.path(), &["mcp", "servers", "list"]);
    assert!(listed.status.success());
    let catalog: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(catalog["servers"]["docs"]["command"], "not-installed");
    assert_eq!(
        catalog["servers"]["docs"]["envFrom"]["TOKEN"],
        "EXTERNAL_TOKEN"
    );
    assert!(
        run_setup(root.path(), &["mcp", "servers", "validate"])
            .status
            .success()
    );
}

#[test]
fn selections_are_exact_and_referenced_servers_cannot_be_removed() {
    let root = TestDirectory::new("catalog-selection");
    let definition = root.path().join("server.json");
    fs::write(
        &definition,
        json!({"transport":"stdio","command":"not-installed"}).to_string(),
    )
    .unwrap();
    let add = run_setup(
        root.path(),
        &[
            "mcp",
            "servers",
            "add",
            "docs",
            "--definition",
            definition.to_str().unwrap(),
        ],
    );
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );

    let defaults = run_setup(
        root.path(),
        &["mcp", "servers", "select", "--defaults", "docs"],
    );
    assert!(
        defaults.status.success(),
        "{}",
        String::from_utf8_lossy(&defaults.stderr)
    );
    let explicit = run_setup(
        root.path(),
        &["mcp", "servers", "select", "--agent", "codex", "docs"],
    );
    assert!(
        explicit.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit.stderr)
    );
    let refused = run_setup(root.path(), &["mcp", "servers", "remove", "docs"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("referenced"));

    let inherit = run_setup(
        root.path(),
        &["mcp", "servers", "select", "--agent", "codex", "--inherit"],
    );
    assert!(
        inherit.status.success(),
        "{}",
        String::from_utf8_lossy(&inherit.stderr)
    );
    let remove = run_setup(root.path(), &["mcp", "servers", "remove", "docs"]);
    assert!(
        !remove.status.success(),
        "defaults still reference docs and must block removal"
    );

    let clear_defaults = run_setup(root.path(), &["mcp", "servers", "select", "--defaults"]);
    assert!(
        clear_defaults.status.success(),
        "{}",
        String::from_utf8_lossy(&clear_defaults.stderr)
    );
    let remove = run_setup(root.path(), &["mcp", "servers", "remove", "docs"]);
    assert!(
        remove.status.success(),
        "{}",
        String::from_utf8_lossy(&remove.stderr)
    );
}
