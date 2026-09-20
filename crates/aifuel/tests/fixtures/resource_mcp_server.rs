use std::env;
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::thread;
use std::time::Duration;

fn main() {
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { return };
        let Some(method) = string_field(&line, "method") else {
            continue;
        };
        if method == "notifications/initialized" {
            continue;
        }
        let Some(id) = raw_field(&line, "id") else {
            continue;
        };
        let response = match method.as_str() {
            "initialize" => format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2025-11-25","capabilities":{{"tools":{{"listChanged":true}},"resources":{{"listChanged":true,"subscribe":true}}}},"serverInfo":{{"name":"resource-fixture","version":"1"}}}}}}"#
            ),
            "tools/list" => format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[{{"name":"echo","description":"resource link fixture","inputSchema":{{"type":"object"}}}}]}}}}"#
            ),
            "tools/call" => format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"resource_link","uri":"https://example.com/direct","name":"direct"}},{{"type":"resource_link","uri":"file:///private","name":"private"}}],"isError":false}}}}"#
            ),
            "resources/list" => format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"resources":[{{"uri":"file:///docs/guide.md","name":"guide","description":"fixture guide","mimeType":"text/plain"}},{{"uri":"https://[::1]/direct","name":"direct","mimeType":"text/plain"}}]}}}}"#
            ),
            "resources/templates/list" => format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"resourceTemplates":[{{"uriTemplate":"file:///docs/{{path}}","name":"guide-template","description":"fixture template","mimeType":"text/plain"}},{{"uriTemplate":"custom+scheme:///{{+path}}","name":"reserved-template"}}]}}}}"#
            ),
            "resources/read" => {
                let uri = string_field(&line, "uri").unwrap_or_else(|| "file:///unknown".to_owned());
                append_log(&format!("read:{uri}"));
                format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"contents":[{{"uri":"{uri}","mimeType":"text/plain","text":"fixture resource"}}]}}}}"#
                )
            }
            "resources/subscribe" => {
                let uri = string_field(&line, "uri").unwrap_or_else(|| "file:///unknown".to_owned());
                append_log(&format!("subscribe:{uri}"));
                let response = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{}}}}"#);
                if !emit(&response) {
                    return;
                }
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(50));
                    let notification = format!(
                        r#"{{"jsonrpc":"2.0","method":"notifications/resources/updated","params":{{"uri":"{uri}"}}}}"#
                    );
                    let _ = emit(&notification);
                });
                continue;
            }
            "resources/unsubscribe" => {
                let uri = string_field(&line, "uri").unwrap_or_else(|| "file:///unknown".to_owned());
                append_log(&format!("unsubscribe:{uri}"));
                format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{}}}}"#)
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

fn append_log(line: &str) {
    let Some(path) = env::var_os("MCP_RESOURCE_LOG") else {
        return;
    };
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = writeln!(file, "{line}");
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
