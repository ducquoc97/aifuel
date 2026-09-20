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
use std::time::Duration;
use streamable_http::StreamableHttpFixture;
use support::TestDirectory;

#[test]
fn remote_gateway_preserves_prompt_content_and_reverse_routes_completion() {
    let temporary = TestDirectory::new("mcp-gateway-remote-prompts");
    let fixture = StreamableHttpFixture::start();
    let config_root = temporary.path().join("config");
    write_remote_catalog(&config_root, &fixture.url(), json!({}));
    let (mut gateway, responses) = start_gateway(&config_root);
    let mut stdin = gateway.stdin.take().expect("gateway stdin should be piped");
    initialize_host(&mut stdin, &responses);
    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"prompts/list","params":{}}),
    );
    initialize_remote_prompts(&fixture, "remote-prompt-session");
    let list = next_remote_post(&fixture, "prompts/list", Duration::from_secs(5));
    let list_id = list.json()["id"].clone();
    list.respond_json(
        200,
        Vec::new(),
        json!({
            "jsonrpc":"2.0",
            "id":list_id,
            "result":{"prompts":[{"name":"summarize","description":"remote"}]}
        }),
    );
    let listed = response_with_id(&responses, 2);
    assert_eq!(listed["result"]["prompts"][0]["name"], "docs__summarize");

    send_message(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":3,"method":"prompts/get","params":{"name":"docs__summarize"}}),
    );
    let get = next_remote_post(&fixture, "prompts/get", Duration::from_secs(5));
    let get_body = get.json();
    assert_eq!(get_body["params"]["name"], "summarize");
    let get_id = get_body["id"].clone();
    get.respond_json(
        200,
        Vec::new(),
        json!({
            "jsonrpc":"2.0",
            "id":get_id,
            "result":{
                "description":"remote prompt",
                "messages":[
                    {"role":"user","content":{"type":"text","text":"remote"}},
                    {"role":"assistant","content":{"type":"resource","resource":{"uri":"file:///remote.md","text":"remote guide"}}},
                    {"role":"assistant","content":{"type":"resource_link","uri":"https://example.test/direct","name":"direct"}}
                ]
            }
        }),
    );
    let prompt = response_with_id(&responses, 3);
    assert_eq!(prompt["result"]["messages"][0]["role"], "user");
    assert!(
        prompt["result"]["messages"][1]["content"]["resource"]["uri"]
            .as_str()
            .is_some_and(|uri| uri.starts_with("aifuel-resource+646f6373+"))
    );
    assert_eq!(
        prompt["result"]["messages"][2]["content"]["uri"],
        "https://example.test/direct"
    );

    send_message(
        &mut stdin,
        json!({
            "jsonrpc":"2.0",
            "id":4,
            "method":"completion/complete",
            "params":{
                "ref":{"type":"ref/prompt","name":"docs__summarize"},
                "argument":{"name":"topic","value":"re"},
                "context":{"arguments":{"other":"value"}}
            }
        }),
    );
    let completion = next_remote_post(&fixture, "completion/complete", Duration::from_secs(5));
    let completion_body = completion.json();
    assert_eq!(completion_body["params"]["ref"]["name"], "summarize");
    assert_eq!(
        completion_body["params"]["context"]["arguments"]["other"],
        "value"
    );
    let completion_id = completion_body["id"].clone();
    completion.respond_json(
        200,
        Vec::new(),
        json!({
            "jsonrpc":"2.0",
            "id":completion_id,
            "result":{"completion":{"values":["remote-alpha"],"total":1,"hasMore":false}}
        }),
    );
    assert_eq!(
        response_with_id(&responses, 4)["result"]["completion"]["values"],
        json!(["remote-alpha"])
    );

    finish_gateway(&mut gateway, stdin, &fixture, "remote-prompt-session");
}
