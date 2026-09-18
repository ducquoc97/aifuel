#[allow(dead_code)]
#[path = "support/gateway.rs"]
mod gateway_support;
#[allow(dead_code)]
mod support;

use gateway_support::*;
use serde_json::json;
use std::time::Duration;
use support::TestDirectory;

#[test]
fn gateway_selection_is_snapshotted_until_a_new_process_starts() {
    let temporary = TestDirectory::new("mcp-gateway-selection-snapshot");
    let server = compile_local_mcp_server(temporary.path());
    let config_root = temporary.path().join("config");
    let servers = json!({
        "docs": {"transport":"stdio","command":server.clone()},
        "search": {"transport":"stdio","command":server}
    });
    write_catalog(
        &config_root,
        json!({
            "servers":servers.clone(),
            "defaults":["docs"],
            "agents":{"exact":{"servers":["docs"]}}
        }),
    );

    let (mut running, running_messages) = start_gateway_for_agent(&config_root, "inherited");
    let mut running_stdin = running.stdin.take().expect("running gateway stdin");
    initialize_host(&mut running_stdin, &running_messages);
    send_message(
        &mut running_stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    assert_eq!(
        response_with_id(&running_messages, 2)["result"]["tools"][0]["name"],
        "docs__echo"
    );

    write_catalog(
        &config_root,
        json!({
            "servers":servers.clone(),
            "defaults":["search"],
            "agents":{"exact":{"servers":["docs"]}}
        }),
    );
    send_message(
        &mut running_stdin,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}),
    );
    assert_eq!(
        response_with_id(&running_messages, 3)["result"]["tools"][0]["name"],
        "docs__echo"
    );

    let (mut restarted, restarted_messages) = start_gateway_for_agent(&config_root, "inherited");
    let mut restarted_stdin = restarted.stdin.take().expect("restarted gateway stdin");
    initialize_host(&mut restarted_stdin, &restarted_messages);
    send_message(
        &mut restarted_stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    assert_eq!(
        response_with_id(&restarted_messages, 2)["result"]["tools"][0]["name"],
        "search__echo"
    );

    let (mut exact, exact_messages) = start_gateway_for_agent(&config_root, "exact");
    let mut exact_stdin = exact.stdin.take().expect("exact gateway stdin");
    initialize_host(&mut exact_stdin, &exact_messages);
    send_message(
        &mut exact_stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    assert_eq!(
        response_with_id(&exact_messages, 2)["result"]["tools"][0]["name"],
        "docs__echo"
    );

    drop(running_stdin);
    drop(restarted_stdin);
    drop(exact_stdin);
    assert!(wait_for_exit(&mut running, Duration::from_secs(8)).success());
    assert!(wait_for_exit(&mut restarted, Duration::from_secs(8)).success());
    assert!(wait_for_exit(&mut exact, Duration::from_secs(8)).success());
}
