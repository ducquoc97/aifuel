#[allow(dead_code)]
#[path = "support/gateway.rs"]
mod gateway_support;
#[path = "support/remote_gateway.rs"]
#[allow(dead_code)]
mod remote_gateway_support;
#[path = "support/streamable_http.rs"]
mod streamable_http;
#[allow(dead_code)]
mod support;

use gateway_support::*;
use remote_gateway_support::*;
use serde_json::json;
use std::time::Duration;
use streamable_http::StreamableHttpFixture;
use support::TestDirectory;

#[test]
fn remote_plain_http_non_loopback_endpoint_is_rejected() {
    let temporary = TestDirectory::new("mcp-gateway-remote-http-policy");
    let config_root = temporary.path().join("config");
    write_remote_catalog(&config_root, "http://192.0.2.1/mcp", json!({}));
    let (mut gateway, responses) = start_gateway(&config_root);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );

    let failure = response_with_id_timeout(&responses, 2, Duration::from_secs(5));
    assert!(
        failure["error"]["message"]
            .as_str()
            .unwrap()
            .contains("loopback address"),
        "{failure}"
    );
    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(5)).success());
}

#[test]
fn remote_redirect_is_rejected_without_requesting_the_redirect_target() {
    let temporary = TestDirectory::new("mcp-gateway-remote-redirect");
    let endpoint = StreamableHttpFixture::start();
    let target = StreamableHttpFixture::start();
    let config_root = temporary.path().join("config");
    write_remote_catalog(&config_root, &endpoint.url(), json!({}));
    let (mut gateway, responses) = start_gateway(&config_root);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );

    let initialize = next_remote_post(&endpoint, "initialize", Duration::from_secs(5));
    initialize.respond(
        302,
        None,
        vec![("Location".to_owned(), target.url())],
        Vec::new(),
    );
    let failure = response_with_id_timeout(&responses, 2, Duration::from_secs(5));
    assert!(failure["error"].is_object());
    assert!(failure["result"]["tools"].is_null());
    assert!(matches!(
        target.next_request(Duration::from_millis(200)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));

    drop(stdin);
    assert!(wait_for_exit(&mut gateway, Duration::from_secs(5)).success());
}

#[test]
fn remote_malformed_and_oversized_json_are_reported_as_failures() {
    for oversized in [false, true] {
        let temporary = TestDirectory::new("mcp-gateway-remote-body-limit");
        let fixture = StreamableHttpFixture::start();
        let config_root = temporary.path().join("config");
        let server_limits = if oversized {
            json!({"maxMessageBytes":512})
        } else {
            json!({})
        };
        write_remote_catalog(&config_root, &fixture.url(), server_limits);
        let (mut gateway, responses) = start_gateway(&config_root);
        let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
        initialize_host(&mut stdin, &responses);
        send_message(
            &mut stdin,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        );
        initialize_remote(&fixture, "body-limit-session");

        let list = next_remote_post(&fixture, "tools/list", Duration::from_secs(5));
        if oversized {
            list.respond(200, Some("application/json"), Vec::new(), vec![b'x'; 1024]);
        } else {
            list.respond(
                200,
                Some("application/json"),
                Vec::new(),
                b"not json".to_vec(),
            );
        }
        let failure = response_with_id_timeout(&responses, 2, Duration::from_secs(5));
        assert!(failure["error"].is_object());
        assert!(failure["result"]["tools"].is_null());
        if oversized {
            assert!(
                failure["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("byte limit")
            );
        } else {
            assert!(
                failure["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("malformed JSON-RPC")
            );
        }

        finish_gateway(&mut gateway, stdin, &fixture, "body-limit-session");
    }
}
