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
        "ping" => Ok(json!({})),
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
                },
                "outputSchema": {
                    "type": "object",
                    "required": ["schema_version", "generated_at", "collection", "providers"],
                    "properties": {
                        "schema_version": {"type": "integer"},
                        "generated_at": {"type": "number"},
                        "collection": {"type": "object"},
                        "providers": {"type": "array"}
                    }
                },
                "annotations": {
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "idempotentHint": true,
                    "openWorldHint": false
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
        "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
        "resources/read" => {
            // Resource reads are intentionally cache-only. A cold resource is
            // not_collected until the host calls get_status.
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
            } else if !valid_status_arguments(&request["params"]["arguments"]) {
                Err((-32602, "invalid get_status arguments"))
            } else {
                let text = serde_json::to_string(state).expect("status value is serializable");
                Ok(json!({
                    "content": [{"type": "text", "text": text}],
                    "structuredContent": state,
                    "isError": false
                }))
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
    let mut filtered = state.clone();
    let provider_id = arguments.get("provider_id").and_then(Value::as_str);
    let account_id = arguments.get("account_id").and_then(Value::as_str);
    if provider_id.is_none() && account_id.is_none() {
        return filtered;
    }
    if let Some(providers) = filtered.get_mut("providers").and_then(Value::as_array_mut) {
        providers.retain(|provider| {
            provider_id.is_none_or(|id| provider["key"].as_str() == Some(id))
                && account_id.is_none_or(|id| provider["account_id"].as_str() == Some(id))
        });
        if providers.is_empty() {
            if let Some(collection) = filtered.get_mut("collection") {
                collection["outcome"] = Value::String("failed".to_owned());
                if let Some(errors) = collection.get_mut("errors").and_then(Value::as_array_mut) {
                    errors.push(json!({
                        "provider_id": provider_id,
                        "code": "unavailable",
                        "message": "requested provider or account is not currently discovered"
                    }));
                }
            }
        }
    }
    if let Some(catalog) = filtered.get_mut("catalog").and_then(Value::as_array_mut) {
        catalog.retain(|provider| provider_id.is_none_or(|id| provider["id"].as_str() == Some(id)));
    }
    if let Some(accounts) = filtered.get_mut("accounts").and_then(Value::as_array_mut) {
        accounts.retain(|account| matches_scope(account, provider_id, account_id));
    }
    if let Some(capabilities) = filtered
        .get_mut("capabilities")
        .and_then(Value::as_array_mut)
    {
        capabilities.retain(|capability| matches_scope(capability, provider_id, account_id));
    }
    if let Some(models) = filtered.get_mut("models").and_then(Value::as_array_mut) {
        models.retain(|model| matches_scope(model, provider_id, account_id));
    }
    if let Some(pools) = filtered
        .get_mut("quota_pools")
        .and_then(Value::as_array_mut)
    {
        pools.retain(|pool| matches_scope(pool, provider_id, account_id));
    }
    if let Some(entitlements) = filtered
        .get_mut("entitlements")
        .and_then(Value::as_array_mut)
    {
        entitlements.retain(|entitlement| matches_scope(entitlement, provider_id, account_id));
    }
    if let Some(observations) = filtered
        .get_mut("observations")
        .and_then(Value::as_array_mut)
    {
        observations.retain(|observation| matches_scope(observation, provider_id, account_id));
    }
    if let Some(collection) = filtered.get_mut("collection") {
        if let Some(coverage) = collection.get_mut("coverage").and_then(Value::as_array_mut) {
            coverage.retain(|provider| provider_id.is_none_or(|id| provider.as_str() == Some(id)));
        }
        if let Some(errors) = collection.get_mut("errors").and_then(Value::as_array_mut) {
            errors.retain(|error| {
                provider_id.is_none_or(|id| error["provider_id"].as_str() == Some(id))
                    && account_id.is_none_or(|id| error["account_id"].as_str() == Some(id))
            });
        }
    }
    if let Some(scope) = filtered
        .get_mut("collection")
        .and_then(|collection| collection.get_mut("scope"))
    {
        scope["provider_id"] = provider_id.map_or(Value::Null, |id| Value::String(id.to_owned()));
        scope["account_id"] = account_id.map_or(Value::Null, |id| Value::String(id.to_owned()));
    }
    filtered
}

fn matches_scope(value: &Value, provider_id: Option<&str>, account_id: Option<&str>) -> bool {
    provider_id.is_none_or(|id| value["provider_id"].as_str() == Some(id))
        && account_id.is_none_or(|id| value["account_id"].as_str() == Some(id))
}

fn valid_status_arguments(arguments: &Value) -> bool {
    let Some(arguments) = arguments.as_object() else {
        return true;
    };
    arguments
        .keys()
        .all(|key| matches!(key.as_str(), "provider_id" | "account_id" | "refresh"))
        && arguments
            .get("provider_id")
            .is_none_or(|value| value.as_str().is_some_and(|value| !value.is_empty()))
        && arguments
            .get("account_id")
            .is_none_or(|value| value.as_str().is_some_and(|value| !value.is_empty()))
        && arguments.get("refresh").is_none_or(Value::is_boolean)
}

fn unix_timestamp() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
