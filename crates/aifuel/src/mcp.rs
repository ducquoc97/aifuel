use aifuel_core::StatusReport;
use aifuel_providers::{CollectionConfig, DiscoveryContext, UsageService};
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn serve() -> Result<(), String> {
    let context = DiscoveryContext::from_environment().map_err(|error| error.to_string())?;
    let service = UsageService::new(context.home_dir(), CollectionConfig::from_environment())?;
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| format!("could not start MCP runtime: {error}"))?;
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    let mut state = serde_json::to_value(StatusReport::cold(unix_timestamp()))
        .map_err(|error| format!("could not create MCP status: {error}"))?;

    for line in stdin.lock().lines() {
        let line = line.map_err(|error| format!("could not read MCP input: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value =
            serde_json::from_str(&line).map_err(|error| format!("invalid MCP JSON: {error}"))?;
        let response_state = if request.get("method").and_then(Value::as_str) == Some("tools/call")
            && request["params"]["name"].as_str() == Some("get_status")
        {
            let refresh = request["params"]["arguments"]["refresh"]
                .as_bool()
                .unwrap_or(false);
            if refresh || state["collection"]["state"] != "collected" {
                let report = runtime.block_on(service.status(refresh));
                state = serde_json::to_value(report)
                    .map_err(|error| format!("could not encode MCP status: {error}"))?;
            }
            filter_status(&state, &request["params"]["arguments"])
        } else {
            state.clone()
        };
        if let Some(response) = dispatch(&request, &response_state) {
            serde_json::to_writer(&mut stdout, &response)
                .map_err(|error| format!("could not encode MCP response: {error}"))?;
            stdout
                .write_all(b"\n")
                .map_err(|error| format!("could not write MCP response: {error}"))?;
            stdout
                .flush()
                .map_err(|error| format!("could not flush MCP response: {error}"))?;
        }
    }
    Ok(())
}

fn dispatch(request: &Value, state: &Value) -> Option<Value> {
    let id = request.get("id").cloned()?;
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");

    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": request["params"]["protocolVersion"].as_str().unwrap_or("2025-11-25"),
            "capabilities": {"tools": {}, "resources": {}},
            "serverInfo": {"name": "aifuel", "version": env!("CARGO_PKG_VERSION")}
        })),
        "tools/list" => Ok(json!({
            "tools": [{
                "name": "get_status",
                "description": "Read provider status, quota, freshness, and errors.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "provider_id": {"type": "string", "minLength": 1},
                        "account_id": {"type": "string", "minLength": 1},
                        "refresh": {"type": "boolean", "default": false}
                    },
                    "additionalProperties": false
                }
            }]
        })),
        "resources/list" => Ok(json!({
            "resources": [{
                "uri": "aifuel://status",
                "name": "AI Fuel status",
                "description": "The latest provider status observations.",
                "mimeType": "application/json"
            }]
        })),
        "resources/read" => {
            let uri = request["params"]["uri"].as_str().unwrap_or("");
            if uri != "aifuel://status" {
                Err((-32602, "unknown resource URI"))
            } else {
                let text = serde_json::to_string(state).expect("status value is serializable");
                Ok(
                    json!({"contents": [{"uri": uri, "mimeType": "application/json", "text": text}]}),
                )
            }
        }
        "tools/call" => {
            let name = request["params"]["name"].as_str().unwrap_or("");
            if name != "get_status" {
                Err((-32602, "unknown tool"))
            } else {
                let text = serde_json::to_string(state).expect("status value is serializable");
                Ok(json!({"content": [{"type": "text", "text": text}], "structuredContent": state}))
            }
        }
        _ => Err((-32601, "method not found")),
    };

    Some(match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message}
        }),
    })
}

fn filter_status(state: &Value, arguments: &Value) -> Value {
    let Some(provider_id) = arguments.get("provider_id").and_then(Value::as_str) else {
        return state.clone();
    };
    let mut filtered = state.clone();
    if let Some(providers) = filtered.get_mut("providers").and_then(Value::as_array_mut) {
        providers.retain(|provider| provider["key"].as_str() == Some(provider_id));
    }
    if let Some(scope) = filtered
        .get_mut("collection")
        .and_then(|collection| collection.get_mut("scope"))
    {
        scope["provider_id"] = Value::String(provider_id.to_owned());
    }
    filtered
}

fn unix_timestamp() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
