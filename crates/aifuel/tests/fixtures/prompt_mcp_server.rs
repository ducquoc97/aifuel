use std::env;
use std::io::{self, BufRead, Write};

fn main() {
    let stdin = io::stdin();
    let mut prompts_changed = false;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { return };
        let Some(method) = string_field(&line, "method") else {
            continue;
        };
        if method == "notifications/cancelled" {
            if let Some(path) = env::var_os("MCP_PROMPT_CANCEL_LOG") {
                let _ = std::fs::write(path, line);
            }
            continue;
        }
        let Some(id) = raw_field(&line, "id") else {
            continue;
        };
        let response = match method.as_str() {
            "initialize" => format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2025-11-25","capabilities":{{"prompts":{{"listChanged":true}},"completions":{{}}}},"serverInfo":{{"name":"prompt-fixture","version":"1"}}}}}}"#
            ),
            "prompts/list" => {
                let suffix = if prompts_changed { "-updated" } else { "" };
                format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"prompts":[{{"name":"summarize{suffix}","title":"Summarize","description":"A prompt fixture","arguments":[{{"name":"topic","required":true}}]}}]}}}}"#
                )
            }
            "prompts/get" => {
                if env::var_os("MCP_PROMPT_CHANGE_LIST").is_some() && !prompts_changed {
                    prompts_changed = true;
                    if !emit(r#"{"jsonrpc":"2.0","method":"notifications/prompts/list_changed"}"#) {
                        return;
                    }
                }
                if let Some(path) = env::var_os("MCP_PROMPT_REQUEST_LOG") {
                    let _ = std::fs::write(path, &line);
                }
                format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"description":"fixture prompt","messages":[{{"role":"user","content":{{"type":"text","text":"topic"}}}},{{"role":"assistant","content":{{"type":"image","data":"AQI=","mimeType":"image/png"}}}},{{"role":"user","content":{{"type":"resource","resource":{{"uri":"file:///guide.md","mimeType":"text/plain","text":"embedded guide"}}}}}},{{"role":"assistant","content":{{"type":"resource_link","uri":"https://example.test/direct","name":"direct link"}}}}]}}}}"#
                )
            }
            "completion/complete" => {
                if let Some(path) = env::var_os("MCP_PROMPT_COMPLETION_LOG") {
                    let _ = std::fs::write(path, &line);
                }
                format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"completion":{{"values":["alpha","beta"],"total":2,"hasMore":false}}}}}}"#
                )
            }
            _ => format!(
                r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32601,"message":"unsupported fixture method"}}}}"#
            ),
        };
        if !emit(&response) {
            return;
        }
    }
}

fn emit(response: &str) -> bool {
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    writeln!(writer, "{response}").is_ok() && writer.flush().is_ok()
}

fn raw_field(line: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let value = line.split_once(&needle)?.1.trim_start();
    let end = value.find([',', '}']).unwrap_or(value.len());
    Some(value[..end].trim().to_owned())
}

fn string_field(line: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let value = line.split_once(&needle)?.1;
    let end = value.find('"')?;
    Some(value[..end].to_owned())
}
