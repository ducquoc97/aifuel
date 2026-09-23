//! Connection-owned Agent Runs. This endpoint never grants permissions.

mod catalog;
mod tools;

use self::catalog::list_models;
use self::tools::tool_definitions;

use aifuel_app::RunManager;
use aifuel_app::selection::{
    GlobalSelectionConfig, SelectionInputs, SelectionSettings, SelectionSource, SelectionSources,
    StoredSession,
};
use aifuel_core::{
    AccessMode, OutputFormat, ProviderKey, RunManagementError, RunRequest, StoredSessionSelection,
};
use serde_json::{Value, json};
use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;

const MAX_FRAME_BYTES: u64 = 1024 * 1024;

pub fn serve(manager: RunManager) -> Result<(), String> {
    serve_with_catalog(manager, Vec::new())
}

pub fn serve_with_catalog(manager: RunManager, catalog: Vec<Value>) -> Result<(), String> {
    serve_with_selection_and_catalog_refresh(
        manager,
        GlobalSelectionConfig::default(),
        catalog,
        |_| Err("model catalog refresh is unavailable".to_owned()),
    )
}

/// Serve the execution endpoint with application-owned selection and catalog
/// discovery. The MCP layer only filters and reports provider-owned evidence.
pub fn serve_with_selection_and_catalog_refresh<F>(
    manager: RunManager,
    selection: GlobalSelectionConfig,
    catalog: Vec<Value>,
    refresh_catalog: F,
) -> Result<(), String>
where
    F: Fn(Option<ProviderKey>) -> Result<Vec<Value>, String>,
{
    let mut catalog = catalog;
    let result = serve_connection(&manager, &selection, &mut catalog, &refresh_catalog);
    manager.shutdown();
    result
}

fn serve_connection<F>(
    manager: &RunManager,
    selection: &GlobalSelectionConfig,
    catalog: &mut Vec<Value>,
    refresh_catalog: &F,
) -> Result<(), String>
where
    F: Fn(Option<ProviderKey>) -> Result<Vec<Value>, String>,
{
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
                let result = call(
                    manager,
                    selection,
                    &request["params"],
                    catalog,
                    refresh_catalog,
                );
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

fn call<F>(
    manager: &RunManager,
    selection: &GlobalSelectionConfig,
    params: &Value,
    catalog: &mut Vec<Value>,
    refresh_catalog: &F,
) -> Result<Value, RunManagementError>
where
    F: Fn(Option<ProviderKey>) -> Result<Vec<Value>, String>,
{
    let name = params["name"]
        .as_str()
        .ok_or_else(|| RunManagementError::invalid_request("tool name is required"))?;
    let empty = json!({});
    let args = params.get("arguments").unwrap_or(&empty);
    match name {
        "list_agents" => {
            only_fields(args, &["provider"])?;
            let provider = optional_string(args, "provider")?
                .map(str::parse)
                .transpose()
                .map_err(|error: aifuel_core::InvalidProviderKey| {
                    RunManagementError::invalid_request(error.to_string())
                })?;
            Ok(json!({
                "schema_version": 1,
                "agents": encoded(manager.list_agents(provider))?
            }))
        }
        "list_models" => list_models(args, catalog, refresh_catalog),
        "start_run" => {
            let (request, _) = resolve_request(args, selection, false, None)?;
            encoded(manager.start_run(request)?)
        }
        "resolve_run" => {
            let (request, sources) = resolve_request(args, selection, false, None)?;
            let mut resolved = encoded(manager.resolve_run(&request)?)?;
            resolved["selection_sources"] = encoded(sources)?;
            Ok(resolved)
        }
        "resume_session" => {
            only_fields(
                args,
                &[
                    "session_id",
                    "profile",
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
            let session_id = required_string(args, "session_id")?;
            let stored_session = manager.session_selection(session_id)?;
            let mut request_args = args.clone();
            request_args
                .as_object_mut()
                .expect("arguments object was validated")
                .remove("session_id");
            let (request, _) =
                resolve_request(&request_args, selection, true, Some(&stored_session))?;
            encoded(manager.resume_session(session_id, request)?)
        }
        "answer_input" => {
            only_fields(args, &["run_id", "input_id", "response"])?;
            let run_id = required_string(args, "run_id")?;
            let input_id = required_string(args, "input_id")?;
            let response = args
                .get("response")
                .cloned()
                .ok_or_else(|| RunManagementError::invalid_request("response is required"))?;
            encoded(manager.answer_input_value(run_id, input_id, response)?)
        }
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

fn resolve_request(
    args: &Value,
    selection: &GlobalSelectionConfig,
    resume: bool,
    stored_session: Option<&StoredSessionSelection>,
) -> Result<(RunRequest, SelectionSources), RunManagementError> {
    only_fields(
        args,
        &[
            "profile",
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
    let provider = optional_string(args, "provider")?
        .map(str::parse)
        .transpose()
        .map_err(|error: aifuel_core::InvalidProviderKey| {
            RunManagementError::invalid_request(error.to_string())
        })?;
    let model = optional_string(args, "model")?.map(str::to_owned);
    let effort = optional_string(args, "effort")?.map(str::to_owned);
    let access = optional_string(args, "access")?
        .map(AccessMode::parse)
        .transpose()
        .map_err(RunManagementError::invalid_request)?;
    let timeout_seconds = args
        .get("timeout_seconds")
        .map(|value| {
            value.as_u64().filter(|value| *value > 0).ok_or_else(|| {
                RunManagementError::invalid_request("timeout_seconds must be a positive integer")
            })
        })
        .transpose()?;
    let profile = optional_string(args, "profile")?.map(str::to_owned);
    let explicit = SelectionSettings {
        provider,
        model,
        effort,
        access,
        overall_deadline_seconds: timeout_seconds,
    };
    let inputs = SelectionInputs {
        explicit: explicit.clone(),
        profile: profile.clone(),
        interactive: false,
        deadline_override: None,
    };
    let session = stored_session.map(|stored| {
        let mut session = StoredSession::new(stored.session_id.clone(), stored.provider);
        session.model = stored.requested_model.clone();
        session.effort = stored.requested_effort.clone();
        session
    });
    let resolved = selection
        .resolve(&inputs, session.as_ref())
        .map_err(|error| RunManagementError::invalid_request(error.to_string()))?;
    if !resume && resolved.model.is_none() {
        return Err(RunManagementError::invalid_request(
            "model selection is required; pass model or select a profile/default",
        ));
    }
    let policy_resolved = if resume {
        let mut policy_explicit = explicit;
        if policy_explicit.provider.is_none() {
            policy_explicit.provider = stored_session.map(|stored| stored.provider);
        }
        selection
            .resolve(
                &SelectionInputs {
                    explicit: policy_explicit,
                    profile,
                    interactive: false,
                    deadline_override: None,
                },
                None,
            )
            .map_err(|error| RunManagementError::invalid_request(error.to_string()))?
    } else {
        resolved.clone()
    };
    let mut sources = resolved.sources.clone();
    sources.access = policy_resolved.sources.access;
    sources.overall_deadline = policy_resolved.sources.overall_deadline;
    let model = if resume
        && matches!(
            &resolved.sources.model,
            SelectionSource::StoredSession | SelectionSource::NativeDefault
        ) {
        None
    } else {
        resolved.model
    };
    let effort = if resume
        && matches!(
            &resolved.sources.effort,
            SelectionSource::StoredSession | SelectionSource::NativeDefault
        ) {
        None
    } else {
        resolved.effort
    };
    Ok((
        RunRequest {
            provider: resolved.provider,
            model,
            effort,
            external_tools: parse_tools(args)?,
            account: None,
            prompt: required_string(args, "prompt")?.to_owned(),
            output: OutputFormat::Json,
            working_directory: optional_string(args, "working_directory")?.map(PathBuf::from),
            access: policy_resolved.access,
            resume: None,
            timeout: policy_resolved.overall_deadline,
            interaction_handler: None,
        },
        sources,
    ))
}
