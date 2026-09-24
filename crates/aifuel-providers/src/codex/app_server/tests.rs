use super::mcp::mcp_tools_are_ready;
use super::protocol::{self, MAX_FRAME_BYTES};
use super::*;
use aifuel_core::{
    AIFUEL_GATEWAY_REGISTRATION_NAME, AgentInputQuestion, AgentInteractionHandler,
    AgentInteractionKind, AgentInteractionRequest, AgentInteractionResponse, OutputFormat,
    ProviderKey,
};
use std::collections::BTreeMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

#[derive(Debug)]
struct AnswerHandler;

#[derive(Debug)]
struct OutputCollector(Arc<std::sync::Mutex<String>>);

impl AgentRunOutputHandler for OutputCollector {
    fn on_output(&self, delta: &str) {
        self.0
            .lock()
            .expect("output observer mutex")
            .push_str(delta);
    }
}

impl AgentInteractionHandler for AnswerHandler {
    fn interact(
        &self,
        request: AgentInteractionRequest,
        _cancellation: &RunCancellationToken,
    ) -> Result<AgentInteractionResponse, AgentRunError> {
        assert_eq!(request.kind, AgentInteractionKind::OrdinaryInput);
        assert_eq!(
            request.questions,
            vec![AgentInputQuestion {
                id: "target".to_owned(),
                text: "Which target?".to_owned(),
            }]
        );
        assert_eq!(request.parameters["questions"][0]["id"], "target");
        assert_eq!(
            request.parameters["questions"][0]["question"],
            "Which target?"
        );
        Ok(AgentInteractionResponse::Answers(BTreeMap::from([(
            "target".to_owned(),
            vec!["workspace".to_owned()],
        )])))
    }
}

fn request() -> RunRequest {
    RunRequest {
        provider: ProviderKey::Codex,
        model: Some("gpt-5-codex".to_owned()),
        effort: Some("high".to_owned()),
        external_tools: None,
        account: None,
        prompt: "Continue the task".to_owned(),
        output: OutputFormat::Text,
        working_directory: Some(std::env::temp_dir()),
        access: AccessMode::ReadOnly,
        resume: Some("native-thread-42".to_owned()),
        timeout: None,
        interaction_handler: Some(Arc::new(AnswerHandler)),
    }
}

async fn read_server_message<R>(reader: &mut R) -> Value
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    reader
        .read_until(b'\n', &mut line)
        .await
        .expect("client message should be readable");
    serde_json::from_slice(&line).expect("client message should be valid JSON")
}

async fn write_server_message<W>(writer: &mut W, message: Value)
where
    W: AsyncWrite + Unpin,
{
    let mut bytes = serde_json::to_vec(&message).expect("server message should serialize");
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .await
        .expect("server message should be writable");
}

#[tokio::test]
async fn resumes_native_thread_and_preserves_fragmented_stream_frames() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (client_read, mut client_write) = tokio::io::split(client);
    let (server_read, mut server_write) = tokio::io::split(server);
    let mut server_read = BufReader::new(server_read);
    let mut client_read = BufReader::new(client_read);
    let request = request();
    let server = tokio::spawn(async move {
        let initialize = read_server_message(&mut server_read).await;
        assert_eq!(initialize["method"], "initialize");
        assert_eq!(
            initialize["params"]["capabilities"]["experimentalApi"],
            true
        );
        write_server_message(&mut server_write, json!({"id":0,"result":{}})).await;

        let initialized = read_server_message(&mut server_read).await;
        assert_eq!(initialized["method"], "initialized");

        let resume = read_server_message(&mut server_read).await;
        assert_eq!(resume["method"], "thread/resume");
        assert_eq!(resume["params"]["threadId"], "native-thread-42");
        assert_eq!(resume["params"]["sandbox"], "read-only");
        write_server_message(
            &mut server_write,
            json!({"id":1,"result":{"thread":{"id":"native-thread-42"}}}),
        )
        .await;

        let turn = read_server_message(&mut server_read).await;
        assert_eq!(turn["method"], "turn/start");
        assert_eq!(turn["params"]["threadId"], "native-thread-42");
        assert_eq!(turn["params"]["effort"], "high");
        assert_eq!(turn["params"]["sandboxPolicy"]["type"], "readOnly");
        write_server_message(
            &mut server_write,
            json!({"id":2,"result":{"turn":{"id":"turn-1"}}}),
        )
        .await;

        server_write
            .write_all(br#"{"method":"item/agentMessage/delta","params":{"delta":"Hel"#)
            .await
            .expect("partial frame should be writable");
        tokio::time::sleep(Duration::from_millis(150)).await;
        server_write
            .write_all(b"lo world\"}}\n")
            .await
            .expect("frame suffix should be writable");
        write_server_message(
            &mut server_write,
            json!({
                "id":"user-input-7",
                "method":"item/tool/requestUserInput",
                "params":{"questions":[{"id":"target","question":"Which target?"}]}
            }),
        )
        .await;
        let response = read_server_message(&mut server_read).await;
        assert_eq!(response["id"], "user-input-7");
        assert_eq!(
            response["result"]["answers"]["target"]["answers"][0],
            "workspace"
        );
        write_server_message(
            &mut server_write,
            json!({
                "method":"turn/completed",
                "params":{"threadId":"native-thread-42","turn":{"id":"turn-1","items":[],"status":"completed"}}
            }),
        )
        .await;
    });

    let cancellation = RunCancellationToken::new();
    let cwd = std::env::temp_dir();
    let result = run_protocol(
        &mut client_write,
        &mut client_read,
        &request,
        &cancellation,
        None,
        ProtocolDeadlines {
            setup: Instant::now() + Duration::from_secs(2),
            task: None,
        },
        &cwd,
    )
    .await
    .expect("simulated App Server run should succeed");
    server.await.expect("simulated App Server should finish");

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(result.output, "Hello world");
    assert_eq!(result.session_id.as_deref(), Some("native-thread-42"));
    assert_eq!(result.resumed_from.as_deref(), Some("native-thread-42"));
}

#[tokio::test]
async fn selected_gateway_tools_are_verified_before_the_turn_starts() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (client_read, mut client_write) = tokio::io::split(client);
    let (server_read, mut server_write) = tokio::io::split(server);
    let mut server_read = BufReader::new(server_read);
    let mut client_read = BufReader::new(client_read);
    let mut request = request();
    request.external_tools = Some(vec!["acceptance__record-call".to_owned()]);

    let server = tokio::spawn(async move {
        let _ = read_server_message(&mut server_read).await;
        write_server_message(&mut server_write, json!({"id":0,"result":{}})).await;
        let _ = read_server_message(&mut server_read).await;
        let start = read_server_message(&mut server_read).await;
        assert_eq!(start["method"], "thread/resume");
        assert_eq!(
            start["params"]["config"]["mcp_servers"][AIFUEL_GATEWAY_REGISTRATION_NAME]["args"],
            json!([
                "mcp",
                "gateway",
                "--agent",
                "codex",
                "--tool",
                "acceptance__record-call"
            ])
        );
        write_server_message(
            &mut server_write,
            json!({"id":1,"result":{"thread":{"id":"native-thread-42"}}}),
        )
        .await;

        let status = read_server_message(&mut server_read).await;
        assert_eq!(status["id"], 3);
        assert_eq!(status["method"], "mcpServerStatus/list");
        write_server_message(
            &mut server_write,
            json!({
                "id":3,
                "result":{"data":[{
                    "name":AIFUEL_GATEWAY_REGISTRATION_NAME,
                    "runtimeStatus":"connected",
                    "tools":{"acceptance__record-call":{"name":"acceptance__record-call"}}
                }]}
            }),
        )
        .await;

        let turn = read_server_message(&mut server_read).await;
        assert_eq!(turn["id"], 2);
        assert_eq!(turn["method"], "turn/start");
        write_server_message(
            &mut server_write,
            json!({"id":2,"result":{"turn":{"id":"turn-1"}}}),
        )
        .await;
        write_server_message(
            &mut server_write,
            json!({
                "method":"turn/completed",
                "params":{"threadId":"native-thread-42","turn":{"id":"turn-1","items":[],"status":"completed"}}
            }),
        )
        .await;
    });

    let cancellation = RunCancellationToken::new();
    let result = run_protocol(
        &mut client_write,
        &mut client_read,
        &request,
        &cancellation,
        None,
        ProtocolDeadlines {
            setup: Instant::now() + Duration::from_secs(2),
            task: None,
        },
        &std::env::temp_dir(),
    )
    .await
    .expect("exact selected Gateway tools should pass setup validation");
    server.await.expect("App Server fixture should finish");
    assert_eq!(result.status, RunStatus::Succeeded);
}

#[test]
fn mcp_status_rejects_extra_tools_and_unrelated_connected_servers() {
    let response = json!({
        "result":{"data":[
            {
                "name":AIFUEL_GATEWAY_REGISTRATION_NAME,
                "runtimeStatus":"connected",
                "tools":{"selected__tool":{},"extra__tool":{}}
            },
            {"name":"unrelated", "runtimeStatus":"connected", "tools":{}}
        ]}
    });
    let error = mcp_tools_are_ready(&response, &["selected__tool".to_owned()])
        .expect_err("additional connected MCP servers must fail closed");
    assert!(error.to_string().contains("unrelated Codex MCP server"));

    let response = json!({
        "result":{"data":[{
            "name":AIFUEL_GATEWAY_REGISTRATION_NAME,
            "runtimeStatus":"connected",
            "tools":{"selected__tool":{},"extra__tool":{}}
        }]}
    });
    let error = mcp_tools_are_ready(&response, &["selected__tool".to_owned()])
        .expect_err("extra Gateway tools must fail closed");
    assert!(
        error
            .to_string()
            .contains("exactly the selected Gateway tools")
    );
}

#[test]
fn managed_app_server_disables_user_plugins_and_native_mcp_config() {
    assert!(
        APP_SERVER_ARGS
            .windows(2)
            .any(|pair| pair == ["--disable", "plugins"])
    );
    assert!(
        APP_SERVER_ARGS
            .windows(2)
            .any(|pair| pair == ["--disable", "remote_plugin"])
    );
    assert!(
        APP_SERVER_ARGS
            .windows(2)
            .any(|pair| pair == ["--config", "mcp_servers={}"])
    );
}

#[tokio::test]
async fn malformed_and_oversized_frames_fail_explicitly() {
    let (client, mut server) = tokio::io::duplex(MAX_FRAME_BYTES + 16);
    let (reader, _writer) = tokio::io::split(client);
    let mut reader = BufReader::new(reader);
    let cancellation = RunCancellationToken::new();

    server
        .write_all(b"not-json\n")
        .await
        .expect("malformed frame should be writable");
    let malformed = protocol::read_message(
        &mut reader,
        Some(Instant::now() + Duration::from_secs(1)),
        &cancellation,
    )
    .await;
    assert!(matches!(malformed, Err(AgentRunError::InvalidRequest(_))));

    let (client, mut server) = tokio::io::duplex(MAX_FRAME_BYTES + 16);
    let (reader, _writer) = tokio::io::split(client);
    let mut reader = BufReader::new(reader);
    server
        .write_all(&vec![b'a'; MAX_FRAME_BYTES + 1])
        .await
        .expect("oversized frame should be writable");
    let oversized = protocol::read_message(
        &mut reader,
        Some(Instant::now() + Duration::from_secs(1)),
        &cancellation,
    )
    .await;
    assert!(matches!(oversized, Err(AgentRunError::InvalidRequest(_))));
}

#[test]
fn selected_tools_add_only_the_filtered_ai_fuel_gateway() {
    let mut request = request();
    request.external_tools = Some(vec![
        "docs__search".to_owned(),
        "repo__read_file".to_owned(),
    ]);

    let config = app_server_config(&request).expect("Gateway config should be valid");
    let gateway = &config["mcp_servers"][AIFUEL_GATEWAY_REGISTRATION_NAME];

    assert!(gateway["command"].as_str().is_some());
    assert_eq!(gateway["args"][0], "mcp");
    assert_eq!(gateway["args"][1], "gateway");
    assert_eq!(gateway["args"][2], "--agent");
    assert_eq!(gateway["args"][3], "codex");
    assert_eq!(gateway["args"][4], "--tool");
    assert_eq!(gateway["args"][5], "docs__search");
    assert_eq!(gateway["args"][6], "--tool");
    assert_eq!(gateway["args"][7], "repo__read_file");
    assert_eq!(gateway["env_vars"][0], "XDG_CONFIG_HOME");
    assert_eq!(config["mcp_servers"].as_object().unwrap().len(), 1);
}

#[test]
fn no_tool_selection_does_not_start_a_gateway_server() {
    let mut request = request();
    assert!(
        app_server_config(&request).unwrap()["mcp_servers"]
            .as_object()
            .unwrap()
            .is_empty()
    );

    request.external_tools = Some(Vec::new());
    assert!(
        app_server_config(&request).unwrap()["mcp_servers"]
            .as_object()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn cancellation_keeps_answer_text_received_before_the_cancel() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (client_read, mut client_write) = tokio::io::split(client);
    let (server_read, mut server_write) = tokio::io::split(server);
    let mut client_read = BufReader::new(client_read);
    let mut server_read = BufReader::new(server_read);
    let cancellation = RunCancellationToken::new();
    let observed = Arc::new(std::sync::Mutex::new(String::new()));
    let output_handler = OutputCollector(Arc::clone(&observed));
    let server_cancellation = cancellation.clone();
    let server = tokio::spawn(async move {
        let _ = read_server_message(&mut server_read).await;
        write_server_message(&mut server_write, json!({"id":0,"result":{}})).await;
        let _ = read_server_message(&mut server_read).await;
        let _ = read_server_message(&mut server_read).await;
        write_server_message(
            &mut server_write,
            json!({"id":1,"result":{"thread":{"id":"native-thread-42"}}}),
        )
        .await;
        let _ = read_server_message(&mut server_read).await;
        write_server_message(
            &mut server_write,
            json!({"id":2,"result":{"turn":{"id":"turn-1"}}}),
        )
        .await;
        write_server_message(
            &mut server_write,
            json!({
                "method":"item/agentMessage/delta",
                "params":{"delta":"partial answer"}
            }),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        server_cancellation.cancel();
    });

    let request = request();
    let cwd = std::env::temp_dir();
    let result = run_protocol(
        &mut client_write,
        &mut client_read,
        &request,
        &cancellation,
        Some(&output_handler),
        ProtocolDeadlines {
            setup: Instant::now() + Duration::from_secs(2),
            task: None,
        },
        &cwd,
    )
    .await
    .expect("cancellation after turn start should produce a partial result");
    server.await.expect("simulated server should finish");

    assert_eq!(result.status, RunStatus::Cancelled);
    assert!(result.output.is_empty());
    assert_eq!(
        observed.lock().expect("output observer mutex").as_str(),
        "partial answer"
    );
}
