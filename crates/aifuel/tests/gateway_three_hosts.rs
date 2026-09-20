#[allow(dead_code)]
#[path = "support/gateway.rs"]
mod gateway_support;
#[allow(dead_code)]
#[path = "support/remote_gateway.rs"]
mod remote_gateway_support;
#[allow(dead_code)]
#[path = "support/streamable_http.rs"]
mod streamable_http;
#[allow(dead_code)]
mod support;

use gateway_support::*;
use remote_gateway_support::*;
use serde_json::json;
use std::process::{Child, ChildStdin};
use std::sync::mpsc::Receiver;
use std::time::Duration;
use streamable_http::StreamableHttpFixture;
use support::TestDirectory;

struct HostGateway {
    id: &'static str,
    session: &'static str,
    process: Child,
    stdin: ChildStdin,
    responses: Receiver<String>,
}

#[test]
fn three_hosts_reuse_one_central_remote_server_with_isolated_sessions() {
    let temporary = TestDirectory::new("mcp-gateway-three-hosts");
    let fixture = StreamableHttpFixture::start();
    let config_root = temporary.path().join("config");
    write_catalog(
        &config_root,
        json!({
            "servers": {
                "shared-docs": {
                    "transport": "streamable-http",
                    "url": fixture.url()
                }
            },
            "defaults": ["shared-docs"]
        }),
    );

    let mut codex = start_host(&config_root, "codex", "codex-session", &fixture);
    let mut claude = start_host(&config_root, "claude", "claude-session", &fixture);
    let mut copilot = start_host(&config_root, "copilot", "copilot-session", &fixture);

    call_shared_tool(&mut codex, &fixture, "codex-result");
    call_shared_tool(&mut claude, &fixture, "claude-result");
    call_shared_tool(&mut copilot, &fixture, "copilot-result");

    finish_gateway(&mut codex.process, codex.stdin, &fixture, codex.session);

    // Closing one host must not close the other hosts' upstream sessions.
    call_shared_tool(&mut claude, &fixture, "claude-after-codex-stop");
    call_shared_tool(&mut copilot, &fixture, "copilot-after-codex-stop");

    finish_gateway(&mut claude.process, claude.stdin, &fixture, claude.session);
    finish_gateway(
        &mut copilot.process,
        copilot.stdin,
        &fixture,
        copilot.session,
    );
}

fn start_host(
    config_root: &std::path::Path,
    id: &'static str,
    session: &'static str,
    fixture: &StreamableHttpFixture,
) -> HostGateway {
    let (mut process, responses) = start_gateway_for_agent(config_root, id);
    let stdin = process.stdin.take().expect("gateway stdin should be piped");
    let mut host = HostGateway {
        id,
        session,
        process,
        stdin,
        responses,
    };
    initialize_host(&mut host.stdin, &host.responses);
    send_message(
        &mut host.stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    initialize_remote(fixture, session);
    respond_tools_list(fixture, session);
    let listed = response_with_id(&host.responses, 2);
    assert_eq!(listed["result"]["tools"][0]["name"], "shared-docs__echo");
    host
}

fn call_shared_tool(host: &mut HostGateway, fixture: &StreamableHttpFixture, expected: &str) {
    send_message(
        &mut host.stdin,
        json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{
                "name":"shared-docs__echo",
                "arguments":{"message":host.id}
            }
        }),
    );
    let call = next_remote_post(fixture, "tools/call", Duration::from_secs(5));
    assert_eq!(call.header("mcp-session-id"), Some(host.session));
    assert_eq!(call.json()["params"]["arguments"]["message"], host.id);
    let call_id = call.json()["id"].clone();
    call.respond_json(
        200,
        Vec::new(),
        json!({
            "jsonrpc":"2.0",
            "id":call_id,
            "result":{"content":[{"type":"text","text":expected}],"isError":false}
        }),
    );
    let response = response_with_id(&host.responses, 3);
    assert_eq!(response["result"]["isError"], false);
    assert_eq!(response["result"]["content"][0]["text"], expected);
}
