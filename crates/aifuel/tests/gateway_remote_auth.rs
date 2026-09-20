#[path = "support/gateway.rs"]
#[allow(dead_code)]
mod gateway_support;
#[path = "support/remote_gateway.rs"]
#[allow(dead_code)]
mod remote_gateway_support;
#[path = "support/streamable_http.rs"]
#[allow(dead_code)]
mod streamable_http;
#[allow(dead_code)]
mod support;

use gateway_support::*;
use remote_gateway_support::*;
use serde_json::json;
use std::io::Read;
use std::process::Child;
use std::time::Duration;
use streamable_http::{PendingRequest, StreamableHttpFixture};
use support::TestDirectory;

const TOKEN_ENV: &str = "AIFUEL_TEST_REMOTE_TOKEN";
const API_KEY_ENV: &str = "AIFUEL_TEST_REMOTE_API_KEY";
const TENANT_ENV: &str = "AIFUEL_TEST_REMOTE_TENANT";

#[test]
fn authenticated_remote_gateway_sends_a_startup_token_only_to_the_configured_endpoint() {
    let temporary = TestDirectory::new("mcp-gateway-remote-auth");
    let source = StreamableHttpFixture::start();
    let token = format!("fixture-token-{}", std::process::id());
    let config_root = temporary.path().join("config");
    write_remote_catalog_with_auth(&config_root, &source.url(), json!({}), Some(TOKEN_ENV));
    let (mut gateway, responses) =
        start_gateway_with_environment(&config_root, &[(TOKEN_ENV, token.as_str())]);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );

    let initialize = next_remote_post(&source, "initialize", Duration::from_secs(5));
    assert_eq!(
        initialize.header("authorization"),
        Some(format!("Bearer {token}").as_str())
    );
    let initialize_id = initialize.json()["id"].clone();
    initialize.respond_json(
        200,
        vec![(
            "MCP-Session-Id".to_owned(),
            "authenticated-session".to_owned(),
        )],
        initialize_result(initialize_id),
    );

    let initialized =
        next_remote_post(&source, "notifications/initialized", Duration::from_secs(5));
    assert_eq!(
        initialized.header("authorization"),
        Some(format!("Bearer {token}").as_str())
    );
    initialized.respond(202, None, Vec::new(), Vec::new());

    let tools = next_remote_post(&source, "tools/list", Duration::from_secs(5));
    assert_eq!(
        tools.header("authorization"),
        Some(format!("Bearer {token}").as_str())
    );
    respond_tools_list_request(tools, "authenticated-session");
    let listed = response_with_id(&responses, 2);
    assert_eq!(listed["result"]["tools"][0]["name"], "docs__echo");

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{"name":"docs__echo","arguments":{"message":"authenticated"}}
        }),
    );
    let call = next_remote_post(&source, "tools/call", Duration::from_secs(5));
    assert_eq!(
        call.header("authorization"),
        Some(format!("Bearer {token}").as_str())
    );
    let call_id = call.json()["id"].clone();
    call.respond_json(
        200,
        Vec::new(),
        json!({
            "jsonrpc":"2.0",
            "id":call_id,
            "result":{"content":[{"type":"text","text":"authenticated-result"}],"isError":false}
        }),
    );
    let called = response_with_id(&responses, 3);
    assert_eq!(
        called["result"]["content"][0]["text"],
        "authenticated-result"
    );

    let stderr = finish_authenticated_gateway(
        &mut gateway,
        stdin,
        &source,
        "authenticated-session",
        &token,
    );
    assert!(
        !stderr.contains(&token),
        "gateway diagnostics leaked the token"
    );
}

#[test]
fn missing_and_empty_bearer_credentials_fail_without_contacting_the_server() {
    assert_bearer_configuration_failure(None, "remote MCP bearer token is missing");
    assert_bearer_configuration_failure(Some(""), "remote MCP bearer token is empty");
}

#[test]
fn named_secret_headers_coexist_with_bearer_and_stay_on_the_configured_endpoint() {
    let temporary = TestDirectory::new("mcp-gateway-remote-named-headers");
    let source = StreamableHttpFixture::start();
    let token = format!("fixture-bearer-{}", std::process::id());
    let api_key = format!("fixture-api-key-{}", std::process::id());
    let tenant = format!("fixture-tenant-{}", std::process::id());
    let config_root = temporary.path().join("config");
    write_remote_catalog_with_credentials(
        &config_root,
        &source.url(),
        json!({}),
        Some(TOKEN_ENV),
        json!({
            "X-API-Key":{"env":API_KEY_ENV},
            "X-Tenant":{"env":TENANT_ENV}
        }),
    );
    let (mut gateway, responses) = start_gateway_with_environment(
        &config_root,
        &[
            (TOKEN_ENV, token.as_str()),
            (API_KEY_ENV, api_key.as_str()),
            (TENANT_ENV, tenant.as_str()),
        ],
    );
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );

    let initialize = next_remote_post(&source, "initialize", Duration::from_secs(5));
    assert_remote_credentials(&initialize, &token, &api_key, &tenant);
    let initialize_id = initialize.json()["id"].clone();
    initialize.respond_json(
        200,
        vec![(
            "MCP-Session-Id".to_owned(),
            "named-header-session".to_owned(),
        )],
        initialize_result(initialize_id),
    );

    let initialized =
        next_remote_post(&source, "notifications/initialized", Duration::from_secs(5));
    assert_remote_credentials(&initialized, &token, &api_key, &tenant);
    initialized.respond(202, None, Vec::new(), Vec::new());

    let tools = next_remote_post(&source, "tools/list", Duration::from_secs(5));
    assert_remote_credentials(&tools, &token, &api_key, &tenant);
    respond_tools_list_request(tools, "named-header-session");
    let listed = response_with_id(&responses, 2);
    assert_eq!(listed["result"]["tools"][0]["name"], "docs__echo");

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{"name":"docs__echo","arguments":{"message":"named headers"}}
        }),
    );
    let call = next_remote_post(&source, "tools/call", Duration::from_secs(5));
    assert_remote_credentials(&call, &token, &api_key, &tenant);
    let call_id = call.json()["id"].clone();
    call.respond_json(
        200,
        Vec::new(),
        json!({
            "jsonrpc":"2.0",
            "id":call_id,
            "result":{"content":[{"type":"text","text":"named-header-result"}],"isError":false}
        }),
    );
    let result = response_with_id(&responses, 3);
    assert_eq!(
        result["result"]["content"][0]["text"],
        "named-header-result"
    );

    let stderr = finish_named_header_gateway(
        &mut gateway,
        stdin,
        &source,
        "named-header-session",
        &token,
        &api_key,
        &tenant,
    );
    assert!(!stderr.contains(&token));
    assert!(!stderr.contains(&api_key));
    assert!(!stderr.contains(&tenant));
}

#[test]
fn missing_and_empty_named_secret_headers_fail_without_contacting_the_server() {
    assert_named_header_configuration_failure(
        None,
        "remote MCP named secret header credential is missing",
    );
    assert_named_header_configuration_failure(
        Some(""),
        "remote MCP named secret header credential is empty",
    );
}

#[test]
fn invalid_named_secret_header_values_fail_without_contacting_the_server() {
    let temporary = TestDirectory::new("mcp-gateway-remote-named-header-invalid");
    let source = StreamableHttpFixture::start();
    let config_root = temporary.path().join("config");
    write_remote_catalog_with_credentials(
        &config_root,
        &source.url(),
        json!({}),
        None,
        json!({"X-API-Key":{"env":API_KEY_ENV}}),
    );
    let (mut gateway, responses) =
        start_gateway_with_environment(&config_root, &[(API_KEY_ENV, "invalid\r\nheader")]);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    let response = response_with_id(&responses, 2);
    let message = response["error"]["message"]
        .as_str()
        .expect("invalid credentials should return an error");
    assert!(message.contains("invalid bytes"));
    assert!(!message.contains("invalid\r\nheader"));
    assert!(!message.contains('\r'));
    assert!(!message.contains('\n'));
    assert_no_upstream_post(&source, Duration::from_millis(200));
    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(5)).success());
}

#[test]
fn rejected_named_secret_header_responses_are_sanitized_and_not_refreshed() {
    let temporary = TestDirectory::new("mcp-gateway-remote-named-header-rejected");
    let source = StreamableHttpFixture::start();
    let api_key = format!("rejected-api-key-{}", std::process::id());
    let config_root = temporary.path().join("config");
    write_remote_catalog_with_credentials(
        &config_root,
        &source.url(),
        json!({}),
        None,
        json!({"X-API-Key":{"env":API_KEY_ENV}}),
    );
    let (mut gateway, responses) =
        start_gateway_with_environment(&config_root, &[(API_KEY_ENV, api_key.as_str())]);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    initialize_remote(&source, "named-rejected-session");
    respond_tools_list(&source, "named-rejected-session");
    let _ = response_with_id(&responses, 2);

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{"name":"docs__echo","arguments":{"message":"rejected"}}
        }),
    );
    let call = next_remote_post(&source, "tools/call", Duration::from_secs(5));
    assert_eq!(call.header("x-api-key"), Some(api_key.as_str()));
    let call_id = call.json()["id"].clone();
    call.respond_json(
        401,
        Vec::new(),
        json!({
            "jsonrpc":"2.0",
            "id":call_id,
            "error":{"code":-32001,"message":format!("private body contains {api_key}")}
        }),
    );
    let response = response_with_id(&responses, 3);
    let message = response["error"]["message"]
        .as_str()
        .expect("rejected credentials should return a JSON-RPC error");
    assert!(message.contains("authentication failed"));
    assert!(!message.contains(&api_key));
    assert!(!message.contains("private body"));
    assert_no_upstream_post(&source, Duration::from_millis(200));

    let stderr = finish_named_header_gateway(
        &mut gateway,
        stdin,
        &source,
        "named-rejected-session",
        "",
        &api_key,
        "",
    );
    assert!(!stderr.contains(&api_key));
}

#[test]
fn named_secret_header_redirects_are_rejected_without_forwarding_the_header() {
    let temporary = TestDirectory::new("mcp-gateway-remote-named-header-redirect");
    let source = StreamableHttpFixture::start();
    let target = StreamableHttpFixture::start();
    let api_key = format!("redirect-api-key-{}", std::process::id());
    let config_root = temporary.path().join("config");
    write_remote_catalog_with_credentials(
        &config_root,
        &source.url(),
        json!({}),
        None,
        json!({"X-API-Key":{"env":API_KEY_ENV}}),
    );
    let (mut gateway, responses) =
        start_gateway_with_environment(&config_root, &[(API_KEY_ENV, api_key.as_str())]);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    let initialize = next_remote_post(&source, "initialize", Duration::from_secs(5));
    assert_eq!(initialize.header("x-api-key"), Some(api_key.as_str()));
    initialize.respond(
        302,
        None,
        vec![("Location".to_owned(), target.url())],
        Vec::new(),
    );
    let response = response_with_id(&responses, 2);
    let message = response["error"]["message"]
        .as_str()
        .expect("redirect should return an error");
    assert!(!message.contains(&api_key));
    assert!(target.next_request(Duration::from_millis(200)).is_err());
    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(5)).success());
    let mut stderr = String::new();
    if let Some(mut pipe) = gateway.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    assert!(!stderr.contains(&api_key));
}

#[test]
fn rejected_bearer_responses_are_sanitized_and_are_not_refreshed() {
    for status in [401, 403] {
        let temporary = TestDirectory::new(&format!("mcp-gateway-remote-auth-{status}"));
        let source = StreamableHttpFixture::start();
        let token = format!("rejected-token-{}-{status}", std::process::id());
        let config_root = temporary.path().join("config");
        write_remote_catalog_with_auth(&config_root, &source.url(), json!({}), Some(TOKEN_ENV));
        let (mut gateway, responses) =
            start_gateway_with_environment(&config_root, &[(TOKEN_ENV, token.as_str())]);
        let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
        initialize_host(&mut stdin, &responses);
        send_message(
            &mut stdin,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        );
        initialize_remote(&source, "rejected-session");
        respond_tools_list(&source, "rejected-session");
        let _ = response_with_id(&responses, 2);

        send_message(
            &mut stdin,
            json!({
                "jsonrpc":"2.0",
                "id":3,
                "method":"tools/call",
                "params":{"name":"docs__echo","arguments":{"message":"rejected"}}
            }),
        );
        let call = next_remote_post(&source, "tools/call", Duration::from_secs(5));
        assert_eq!(
            call.header("authorization"),
            Some(format!("Bearer {token}").as_str())
        );
        let call_id = call.json()["id"].clone();
        call.respond_json(
            status,
            Vec::new(),
            json!({
                "jsonrpc":"2.0",
                "id":call_id,
                "error":{"code":-32001,"message":format!("private body contains {token}")}
            }),
        );
        let response = response_with_id(&responses, 3);
        let message = response["error"]["message"]
            .as_str()
            .expect("rejected credentials should return a JSON-RPC error");
        assert!(message.contains(if status == 401 {
            "authentication failed"
        } else {
            "denied access"
        }));
        assert!(!message.contains(&token));
        assert!(!message.contains("private body"));
        assert_no_upstream_post(&source, Duration::from_millis(200));

        let stderr =
            finish_authenticated_gateway(&mut gateway, stdin, &source, "rejected-session", &token);
        assert!(
            !stderr.contains(&token),
            "gateway diagnostics leaked the token"
        );
    }
}

#[test]
fn redirect_rejection_does_not_forward_the_bearer_token_to_another_endpoint() {
    let temporary = TestDirectory::new("mcp-gateway-remote-auth-redirect");
    let source = StreamableHttpFixture::start();
    let target = StreamableHttpFixture::start();
    let token = format!("redirect-token-{}", std::process::id());
    let config_root = temporary.path().join("config");
    write_remote_catalog_with_auth(&config_root, &source.url(), json!({}), Some(TOKEN_ENV));
    let (mut gateway, responses) =
        start_gateway_with_environment(&config_root, &[(TOKEN_ENV, token.as_str())]);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    let initialize = next_remote_post(&source, "initialize", Duration::from_secs(5));
    assert_eq!(
        initialize.header("authorization"),
        Some(format!("Bearer {token}").as_str())
    );
    initialize.respond(
        302,
        None,
        vec![("Location".to_owned(), target.url())],
        Vec::new(),
    );
    let error = response_with_id(&responses, 2);
    let message = error["error"]["message"]
        .as_str()
        .expect("redirected initialization should fail the selected server");
    assert!(!message.contains(&token));
    assert!(target.next_request(Duration::from_millis(200)).is_err());

    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(5)).success());
    let mut stderr = String::new();
    if let Some(mut pipe) = gateway.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    assert!(
        !stderr.contains(&token),
        "gateway diagnostics leaked the token"
    );
}

fn assert_bearer_configuration_failure(token: Option<&str>, expected: &str) {
    let suffix = if token.is_some() { "empty" } else { "missing" };
    let temporary = TestDirectory::new(&format!("mcp-gateway-remote-auth-{suffix}"));
    let source = StreamableHttpFixture::start();
    let config_root = temporary.path().join("config");
    write_remote_catalog_with_auth(&config_root, &source.url(), json!({}), Some(TOKEN_ENV));
    let environment = token.map_or_else(Vec::new, |token| vec![(TOKEN_ENV, token)]);
    let (mut gateway, responses) = start_gateway_with_environment(&config_root, &environment);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    let response = response_with_id(&responses, 2);
    let message = response["error"]["message"]
        .as_str()
        .expect("missing credentials should return a JSON-RPC error");
    assert!(message.contains(expected));
    assert_no_upstream_post(&source, Duration::from_millis(200));
    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(5)).success());
}

fn assert_named_header_configuration_failure(value: Option<&str>, expected: &str) {
    let suffix = if value.is_some() { "empty" } else { "missing" };
    let temporary = TestDirectory::new(&format!("mcp-gateway-remote-named-header-{suffix}"));
    let source = StreamableHttpFixture::start();
    let config_root = temporary.path().join("config");
    write_remote_catalog_with_credentials(
        &config_root,
        &source.url(),
        json!({}),
        None,
        json!({"X-API-Key":{"env":API_KEY_ENV}}),
    );
    let environment = value.map_or_else(Vec::new, |value| vec![(API_KEY_ENV, value)]);
    let (mut gateway, responses) = start_gateway_with_environment(&config_root, &environment);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    let response = response_with_id(&responses, 2);
    let message = response["error"]["message"]
        .as_str()
        .expect("missing credentials should return a JSON-RPC error");
    assert!(message.contains(expected));
    assert_no_upstream_post(&source, Duration::from_millis(200));
    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(5)).success());
}

fn assert_remote_credentials(request: &PendingRequest, token: &str, api_key: &str, tenant: &str) {
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {token}").as_str())
    );
    assert_eq!(request.header("x-api-key"), Some(api_key));
    assert_eq!(request.header("x-tenant"), Some(tenant));
}

fn finish_authenticated_gateway(
    gateway: &mut Child,
    stdin: impl std::io::Write,
    fixture: &StreamableHttpFixture,
    session_id: &str,
    token: &str,
) -> String {
    drop(stdin);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let request = fixture
            .next_request(deadline.saturating_duration_since(std::time::Instant::now()))
            .expect("gateway shutdown should terminate its upstream HTTP session");
        if request.method == "GET" {
            assert_eq!(
                request.header("authorization"),
                Some(format!("Bearer {token}").as_str())
            );
            request.respond(405, None, Vec::new(), Vec::new());
            continue;
        }
        assert_eq!(request.method, "DELETE");
        assert_eq!(request.header("mcp-session-id"), Some(session_id));
        assert_eq!(request.header("mcp-protocol-version"), Some("2025-11-25"));
        assert_eq!(
            request.header("authorization"),
            Some(format!("Bearer {token}").as_str())
        );
        request.respond(200, None, Vec::new(), Vec::new());
        break;
    }
    assert!(wait_for_exit(gateway, Duration::from_secs(5)).success());
    let mut stderr = String::new();
    if let Some(mut pipe) = gateway.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    stderr
}

fn finish_named_header_gateway(
    gateway: &mut Child,
    stdin: impl std::io::Write,
    fixture: &StreamableHttpFixture,
    session_id: &str,
    token: &str,
    api_key: &str,
    tenant: &str,
) -> String {
    drop(stdin);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let request = fixture
            .next_request(deadline.saturating_duration_since(std::time::Instant::now()))
            .expect("gateway shutdown should terminate its upstream HTTP session");
        if request.method == "GET" {
            if !token.is_empty() {
                assert_eq!(
                    request.header("authorization"),
                    Some(format!("Bearer {token}").as_str())
                );
            } else {
                assert_eq!(request.header("authorization"), None);
            }
            assert_eq!(request.header("x-api-key"), Some(api_key));
            if !tenant.is_empty() {
                assert_eq!(request.header("x-tenant"), Some(tenant));
            } else {
                assert_eq!(request.header("x-tenant"), None);
            }
            request.respond(405, None, Vec::new(), Vec::new());
            continue;
        }
        assert_eq!(request.method, "DELETE");
        assert_eq!(request.header("mcp-session-id"), Some(session_id));
        assert_eq!(request.header("mcp-protocol-version"), Some("2025-11-25"));
        if !token.is_empty() {
            assert_eq!(
                request.header("authorization"),
                Some(format!("Bearer {token}").as_str())
            );
        } else {
            assert_eq!(request.header("authorization"), None);
        }
        assert_eq!(request.header("x-api-key"), Some(api_key));
        if !tenant.is_empty() {
            assert_eq!(request.header("x-tenant"), Some(tenant));
        } else {
            assert_eq!(request.header("x-tenant"), None);
        }
        request.respond(200, None, Vec::new(), Vec::new());
        break;
    }
    assert!(wait_for_exit(gateway, Duration::from_secs(5)).success());
    let mut stderr = String::new();
    if let Some(mut pipe) = gateway.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    stderr
}
