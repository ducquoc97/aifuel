//! Connection-owned Agent Runs. This endpoint never grants permissions.

use aifuel_app::RunManager;
use aifuel_core::{AccessMode, OutputFormat, RunManagementError, RunRequest};
use serde_json::{Value, json};
use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

const MAX_FRAME_BYTES: u64 = 1024 * 1024;

pub fn serve(manager: RunManager) -> Result<(), String> {
    serve_with_catalog(manager, Vec::new())
}

pub fn serve_with_catalog(manager: RunManager, catalog: Vec<Value>) -> Result<(), String> {
    let result = serve_connection(&manager, &catalog);
    manager.shutdown();
    result
}

fn serve_connection(manager: &RunManager, catalog: &[Value]) -> Result<(), String> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut output = io::BufWriter::new(io::stdout().lock());
    let mut initialized = false;
    loop {
        let mut frame = Vec::new();
        let count = input
            .by_ref()
            .take(MAX_FRAME_BYTES + 1)
            .read_until(b'\n', &mut frame)
            .map_err(|error| error.to_string())?;
        if count == 0 {
            return Ok(());
        }
        if count as u64 > MAX_FRAME_BYTES {
            return Err("execution MCP frame exceeds 1 MiB".to_owned());
        }
        if frame.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let request: Value = match serde_json::from_slice(&frame) {
            Ok(request) => request,
            Err(_) => {
                write_response(&mut output, &rpc_error(Value::Null, -32700, "invalid JSON"))?;
                continue;
            }
        };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        if request["jsonrpc"] != "2.0" || !(id.is_string() || id.is_number() || id.is_null()) {
            write_response(
                &mut output,
                &rpc_error(Value::Null, -32600, "invalid request"),
            )?;
            continue;
        }
        let response = match request["method"].as_str() {
            Some("initialize") if !initialized => {
                let version = request["params"]["protocolVersion"].as_str().unwrap_or("");
                if !matches!(
                    version,
                    "2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25"
                ) {
                    rpc_error(id, -32602, "unsupported protocol version")
                } else {
                    initialized = true;
                    json!({"jsonrpc":"2.0","id":id,"result":{
                        "protocolVersion":version,"capabilities":{"tools":{}},
                        "serverInfo":{"name":"aifuel-execution","version":env!("CARGO_PKG_VERSION")}
                    }})
                }
            }
            Some("ping") => json!({"jsonrpc":"2.0","id":id,"result":{}}),
            _ if !initialized => rpc_error(id, -32002, "initialize first"),
            Some("tools/list") => {
                json!({"jsonrpc":"2.0","id":id,"result":{"tools":tool_definitions()}})
            }
            Some("tools/call") => {
                let result = call(manager, &request["params"], catalog);
                let (value, is_error) = match result {
                    Ok(value) => (value, false),
                    Err(error) => (
                        serde_json::to_value(error).expect("execution error serializes"),
                        true,
                    ),
                };
                json!({"jsonrpc":"2.0","id":id,"result":{
                    "content":[{"type":"text","text":value.to_string()}],
                    "structuredContent":value,"isError":is_error
                }})
            }
            _ => rpc_error(id, -32601, "method not found"),
        };
        write_response(&mut output, &response)?;
    }
}

fn write_response(output: &mut impl Write, response: &Value) -> Result<(), String> {
    serde_json::to_writer(&mut *output, response).map_err(|error| error.to_string())?;
    output
        .write_all(b"\n")
        .and_then(|()| output.flush())
        .map_err(|error| error.to_string())
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

fn encoded(value: impl serde::Serialize) -> Result<Value, RunManagementError> {
    serde_json::to_value(value)
        .map_err(|_| RunManagementError::invalid_request("response encoding failed"))
}

fn call(
    manager: &RunManager,
    params: &Value,
    catalog: &[Value],
) -> Result<Value, RunManagementError> {
    let name = params["name"]
        .as_str()
        .ok_or_else(|| RunManagementError::invalid_request("tool name is required"))?;
    let empty = json!({});
    let args = params.get("arguments").unwrap_or(&empty);
    match name {
        "list_agents" => {
            only_fields(args, &[])?;
            Ok(json!({
                "schema_version": 1,
                "agents": manager
                    .registered_providers()
                    .into_iter()
                    .map(|provider| json!({
                        "provider": provider,
                        "integration": "compiled",
                        "presence": "unknown",
                        "authentication": "unknown",
                        "capabilities": {
                            "model_catalog": "unknown",
                            "streaming": "unknown",
                            "workspace_write": "unknown"
                        }
                    }))
                    .collect::<Vec<_>>()
            }))
        }
        "list_models" => {
            only_fields(args, &["provider"])?;
            let provider = optional_string(args, "provider")?;
            let snapshots = catalog.iter().filter(|snapshot| {
                provider.is_none_or(|provider| snapshot["scope"]["provider"] == provider)
            });
            let models = snapshots
                .flat_map(|snapshot| snapshot["models"].as_array().into_iter().flatten())
                .cloned()
                .collect::<Vec<_>>();
            Ok(json!({
                "schema_version": 1,
                "provider": provider,
                "models": models,
                "evidence": if catalog.is_empty() { "unknown" } else { "cached" },
                "freshness": if catalog.is_empty() { "unknown" } else { "stale_or_fresh" },
                "diagnostics": if catalog.is_empty() { Some("model discovery has not produced a cached catalog") } else { None }
            }))
        }
        "start_run" => encoded(manager.start_run(parse_request(args)?)?),
        "resolve_run" => encoded(manager.resolve_run(&parse_request(args)?)?),
        "resume_session" => {
            only_fields(
                args,
                &[
                    "session_id",
                    "provider",
                    "model",
                    "effort",
                    "prompt",
                    "working_directory",
                    "access",
                    "timeout_seconds",
                ],
            )?;
            let session_id = required_string(args, "session_id")?;
            let mut request_args = args.clone();
            request_args
                .as_object_mut()
                .expect("arguments object was validated")
                .remove("session_id");
            let request = parse_request(&request_args)?;
            encoded(manager.resume_session(session_id, request)?)
        }
        "answer_input" => Err(RunManagementError::new(
            aifuel_core::RunManagementErrorCode::UnsupportedCapability,
            "ordinary input is not supported by the selected integration; permission approvals remain local-only",
        )),
        "get_run" | "get_result" | "cancel_run" => {
            only_fields(args, &["run_id"])?;
            let id = required_string(args, "run_id")?;
            match name {
                "get_run" => encoded(manager.get_run(id)?),
                "get_result" => encoded(manager.get_result(id)?),
                _ => encoded(manager.cancel_run(id)?),
            }
        }
        "read_events" => {
            only_fields(args, &["run_id", "cursor", "page_bytes"])?;
            let cursor = optional_string(args, "cursor")?;
            let page = args
                .get("page_bytes")
                .map(|value| {
                    value
                        .as_u64()
                        .and_then(|value| usize::try_from(value).ok())
                        .ok_or_else(|| {
                            RunManagementError::invalid_request(
                                "page_bytes must be a positive integer",
                            )
                        })
                })
                .transpose()?;
            encoded(manager.read_events(required_string(args, "run_id")?, cursor, page)?)
        }
        _ => Err(RunManagementError::invalid_request(
            "unknown execution tool",
        )),
    }
}

fn only_fields(args: &Value, fields: &[&str]) -> Result<(), RunManagementError> {
    let object = args
        .as_object()
        .ok_or_else(|| RunManagementError::invalid_request("arguments must be an object"))?;
    if let Some(field) = object
        .keys()
        .find(|field| !fields.contains(&field.as_str()))
    {
        return Err(RunManagementError::invalid_request(format!(
            "unknown argument {field:?}"
        )));
    }
    Ok(())
}

fn required_string<'a>(args: &'a Value, field: &str) -> Result<&'a str, RunManagementError> {
    optional_string(args, field)?
        .ok_or_else(|| RunManagementError::invalid_request(format!("{field} is required")))
}

fn optional_string<'a>(
    args: &'a Value,
    field: &str,
) -> Result<Option<&'a str>, RunManagementError> {
    args.get(field)
        .map(|value| {
            value
                .as_str()
                .filter(|text| !text.trim().is_empty())
                .ok_or_else(|| {
                    RunManagementError::invalid_request(format!(
                        "{field} must be a nonempty string"
                    ))
                })
        })
        .transpose()
}

fn parse_tools(args: &Value) -> Result<Option<Vec<String>>, RunManagementError> {
    let Some(value) = args.get("external_tools") else {
        return Ok(None);
    };
    let tools = value.as_array().ok_or_else(|| {
        RunManagementError::invalid_request("external_tools must be an array of strings")
    })?;
    let mut result = Vec::with_capacity(tools.len());
    for tool in tools {
        let tool = tool
            .as_str()
            .filter(|tool| !tool.trim().is_empty())
            .ok_or_else(|| {
                RunManagementError::invalid_request("external_tools must contain nonempty strings")
            })?;
        result.push(tool.to_owned());
    }
    Ok(Some(result))
}

fn parse_request(args: &Value) -> Result<RunRequest, RunManagementError> {
    only_fields(
        args,
        &[
            "provider",
            "model",
            "effort",
            "external_tools",
            "prompt",
            "working_directory",
            "access",
            "timeout_seconds",
        ],
    )?;
    let provider = required_string(args, "provider")?.parse().map_err(
        |error: aifuel_core::InvalidProviderKey| {
            RunManagementError::invalid_request(error.to_string())
        },
    )?;
    let access = AccessMode::parse(optional_string(args, "access")?.unwrap_or("read-only"))
        .map_err(RunManagementError::invalid_request)?;
    let timeout = args
        .get("timeout_seconds")
        .map(|value| {
            value
                .as_u64()
                .filter(|value| *value > 0)
                .map(Duration::from_secs)
                .ok_or_else(|| {
                    RunManagementError::invalid_request(
                        "timeout_seconds must be a positive integer",
                    )
                })
        })
        .transpose()?;
    Ok(RunRequest {
        provider,
        model: optional_string(args, "model")?.map(str::to_owned),
        effort: optional_string(args, "effort")?.map(str::to_owned),
        external_tools: parse_tools(args)?,
        account: None,
        prompt: required_string(args, "prompt")?.to_owned(),
        output: OutputFormat::Json,
        working_directory: optional_string(args, "working_directory")?.map(PathBuf::from),
        access,
        resume: None,
        timeout,
    })
}

fn tool_definitions() -> Vec<Value> {
    let mut tools = Vec::new();
    tools.push(json!({"name":"list_agents","description":"List compiled Agent Integrations and independently observed capability evidence.","inputSchema":{"type":"object","additionalProperties":false},"annotations":{"readOnlyHint":true,"idempotentHint":true,"openWorldHint":false}}));
    tools.push(json!({"name":"list_models","description":"List provider model catalog evidence; unknown evidence remains explicit.","inputSchema":{"type":"object","additionalProperties":false,"properties":{"provider":{"type":"string"}}},"annotations":{"readOnlyHint":true,"idempotentHint":true,"openWorldHint":false}}));
    for (name, description) in [
        (
            "resolve_run",
            "Resolve and validate an Agent Run without starting it.",
        ),
        (
            "start_run",
            "Start a connection-owned Agent Run. Never automatically replay this operation.",
        ),
    ] {
        tools.push(json!({"name":name,"description":description,"inputSchema":{
            "type":"object","additionalProperties":false,"required":["provider","prompt"],
            "properties":{
                "provider":{"type":"string"},"model":{"type":"string"},"effort":{"type":"string"},"external_tools":{"type":"array","items":{"type":"string"}},"prompt":{"type":"string"},
                "working_directory":{"type":"string"},"access":{"enum":["read-only","workspace-write"]},
                "timeout_seconds":{"type":"integer","minimum":1}
            }
        },"annotations":{"readOnlyHint":name == "resolve_run","idempotentHint":name == "resolve_run","openWorldHint":true}}));
    }
    tools.push(json!({"name":"resume_session","description":"Resume a same-provider native session owned by this execution connection.","inputSchema":{"type":"object","additionalProperties":false,"required":["session_id","provider","prompt"],"properties":{"session_id":{"type":"string"},"provider":{"type":"string"},"model":{"type":"string"},"effort":{"type":"string"},"prompt":{"type":"string"},"working_directory":{"type":"string"},"access":{"enum":["read-only","workspace-write"]},"timeout_seconds":{"type":"integer","minimum":1}}},"annotations":{"readOnlyHint":false,"idempotentHint":false,"openWorldHint":true}}));
    tools.push(json!({"name":"answer_input","description":"Answer an ordinary provider question when the adapter can distinguish it from a permission request; unsupported integrations reject it.","inputSchema":{"type":"object","additionalProperties":false,"required":["run_id","input_id","response"],"properties":{"run_id":{"type":"string"},"input_id":{"type":"string"},"response":{"type":"string"}}},"annotations":{"readOnlyHint":false,"idempotentHint":false,"openWorldHint":false}}));
    for (name, description) in [
        ("get_run", "Inspect a run owned by this connection."),
        ("get_result", "Read a run result without consuming it."),
        (
            "cancel_run",
            "Cancel an owned run; repeated cancellation preserves terminal outcomes.",
        ),
        (
            "read_events",
            "Read ordered events with an opaque cursor and explicit history gaps.",
        ),
    ] {
        let mut properties = json!({"run_id":{"type":"string"}});
        if name == "read_events" {
            properties["cursor"] = json!({"type":"string"});
            properties["page_bytes"] = json!({"type":"integer","minimum":1,"maximum":1048576});
        }
        tools.push(json!({"name":name,"description":description,"inputSchema":{
            "type":"object","additionalProperties":false,"required":["run_id"],"properties":properties
        },"annotations":{"readOnlyHint":name != "cancel_run","idempotentHint":true,"openWorldHint":false}}));
    }
    tools
}
