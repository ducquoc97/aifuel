use serde_json::{Value, json};

pub(super) fn tool_definitions() -> Vec<Value> {
    let mut tools = Vec::new();
    tools.push(json!({"name":"list_agents","description":"List compiled Agent Integrations with native presence, version, and capability evidence.","inputSchema":{"type":"object","additionalProperties":false,"properties":{"provider":{"type":"string"}}},"annotations":{"readOnlyHint":true,"idempotentHint":true,"openWorldHint":false}}));
    tools.push(json!({"name":"list_models","description":"List provider model catalog evidence and optionally refresh through provider-owned discovery.","inputSchema":{"type":"object","additionalProperties":false,"properties":{"provider":{"type":"string"},"refresh":{"type":"boolean"}}},"annotations":{"readOnlyHint":true,"idempotentHint":true,"openWorldHint":false}}));
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
            "type":"object","additionalProperties":false,"required":["prompt"],
            "properties":{
                "provider":{"type":"string"},"profile":{"type":"string"},"model":{"type":"string"},"effort":{"type":"string"},"external_tools":{"type":"array","items":{"type":"string"}},"prompt":{"type":"string"},
                "working_directory":{"type":"string"},"access":{"enum":["read-only","workspace-write"]},
                "timeout_seconds":{"type":"integer","minimum":1}
            }
        },"annotations":{"readOnlyHint":name == "resolve_run","idempotentHint":name == "resolve_run","openWorldHint":true}}));
    }
    tools.push(json!({"name":"resume_session","description":"Resume a same-provider native session owned by this execution connection.","inputSchema":{"type":"object","additionalProperties":false,"required":["session_id","prompt"],"properties":{"session_id":{"type":"string"},"provider":{"type":"string"},"profile":{"type":"string"},"model":{"type":"string"},"effort":{"type":"string"},"external_tools":{"type":"array","items":{"type":"string"}},"prompt":{"type":"string"},"working_directory":{"type":"string"},"access":{"enum":["read-only","workspace-write"]},"timeout_seconds":{"type":"integer","minimum":1}}},"annotations":{"readOnlyHint":false,"idempotentHint":false,"openWorldHint":true}}));
    tools.push(json!({"name":"answer_input","description":"Answer an ordinary provider question with a string, question-ID answer object, or typed MCP elicitation object; permission requests are rejected because approval is local-only.","inputSchema":{"type":"object","additionalProperties":false,"required":["run_id","input_id","response"],"properties":{"run_id":{"type":"string"},"input_id":{"type":"string"},"response":{"oneOf":[{"type":"string"},{"type":"object","additionalProperties":true}]}}},"annotations":{"readOnlyHint":false,"idempotentHint":false,"openWorldHint":false}}));
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
