use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

fn main() {
    let mut args = env::args().skip(1);
    if args.next().as_deref() == Some("--grandchild") {
        let marker = args.next().expect("grandchild marker path");
        let started = args.next().expect("grandchild started marker path");
        let _ = fs::write(started, "started");
        thread::sleep(Duration::from_secs(3));
        let _ = fs::write(marker, "late child survived gateway shutdown");
        return;
    }

    if env::var_os("MCP_FIXTURE_OFFLINE_MARKER").is_some_and(|path| fs::metadata(path).is_ok()) {
        return;
    }

    if let Some(path) = env::var_os("MCP_FIXTURE_START_LOG") {
        let cwd = env::current_dir().expect("fixture cwd should be available");
        let args: Vec<_> = env::args().skip(1).collect();
        let log = format!(
            "cwd={}\nargs={args:?}\npath_present={}\nunlisted_secret_present={}",
            cwd.display(),
            env::var_os("PATH").is_some(),
            env::var_os("AIFUEL_TEST_UNLISTED_SECRET").is_some()
        );
        let log = format!(
            "{log}\nsource_value={}",
            env::var("MCP_FIXTURE_SOURCE").unwrap_or_else(|_| "missing".to_owned())
        );
        let _ = fs::write(path, log);
    }

    if let Some(late_marker) = env::var_os("MCP_FIXTURE_LATE_MARKER") {
        if let Ok(executable) = env::current_exe() {
            let started_marker = env::var_os("MCP_FIXTURE_CHILD_STARTED");
            let grandchild_started_marker =
                env::var_os("MCP_FIXTURE_GRANDCHILD_STARTED").expect("grandchild marker");
            let child = Command::new(executable)
                .arg("--grandchild")
                .arg(late_marker)
                .arg(grandchild_started_marker)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            if child.is_ok() {
                if let Some(marker) = started_marker {
                    let _ = fs::write(marker, "started");
                }
            }
        }
    }

    let mut tool_list_changed = false;
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { return };
        let Some(method) = string_field(&line, "method") else {
            continue;
        };
        if method == "notifications/cancelled" {
            if let Some(path) = env::var_os("MCP_FIXTURE_CANCEL_LOG") {
                let _ = fs::write(path, line);
            }
            continue;
        }
        let Some(id) = raw_field(&line, "id") else {
            continue;
        };
        let response = match method.as_str() {
            "initialize" => format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"{}","capabilities":{{"tools":{{"listChanged":true}}}},"serverInfo":{{"name":"local-fixture","version":"1"}}}}}}"#,
                env::var("MCP_FIXTURE_VERSION").unwrap_or_else(|_| "2025-11-25".to_owned())
            ),
            "tools/list" => format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[{}]}}}}"#,
                listed_tools(tool_list_changed)
            ),
            "tools/call" => {
                if env::var_os("MCP_FIXTURE_CHANGE_TOOL_LIST").is_some() {
                    tool_list_changed = true;
                    let count = env::var("MCP_FIXTURE_LIST_CHANGED_COUNT")
                        .ok()
                        .and_then(|value| value.parse::<usize>().ok())
                        .unwrap_or(1);
                    for _ in 0..count {
                        if !emit(r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#)
                        {
                            return;
                        }
                    }
                }
                let mut diagnostics = format!(
                    "{line}\nprogress={:?}\ndelay={:?}",
                    env::var_os("MCP_FIXTURE_PROGRESS"),
                    env::var_os("MCP_FIXTURE_DELAY_MS")
                );
                if env::var_os("MCP_FIXTURE_PROGRESS").is_some() {
                    if let Some(token) = raw_field(&line, "progressToken") {
                        let count = env::var("MCP_FIXTURE_PROGRESS_COUNT")
                            .ok()
                            .and_then(|value| value.parse::<usize>().ok())
                            .unwrap_or(1);
                        for index in 0..count {
                            let progress_token = if index == 0 {
                                token.clone()
                            } else {
                                format!("\"fixture-progress-{index}\"")
                            };
                            let notification = format!(
                                r#"{{"jsonrpc":"2.0","method":"notifications/progress","params":{{"progressToken":{progress_token},"progress":1,"total":1,"message":"working"}}}}"#
                            );
                            if index == 0 {
                                diagnostics.push_str(&format!("\nnotification={notification}"));
                            }
                            if !emit(&notification) {
                                return;
                            }
                        }
                        if let Some(path) = env::var_os("MCP_FIXTURE_LOG") {
                            let _ = fs::write(path, &diagnostics);
                        }
                    }
                }
                if let Some(path) = env::var_os("MCP_FIXTURE_LOG") {
                    let _ = fs::write(path, &diagnostics);
                }
                let error = env::var_os("MCP_FIXTURE_TOOL_ERROR").is_some();
                let message = if error {
                    "fixture-error".to_owned()
                } else if env::var_os("MCP_FIXTURE_RESPONSE_ID").is_some() {
                    std::process::id().to_string()
                } else {
                    env::var("MCP_FIXTURE_RESULT")
                        .ok()
                        .or_else(|| {
                            env::var("MCP_FIXTURE_RESULT_BYTES")
                                .ok()
                                .and_then(|value| value.parse::<usize>().ok())
                                .map(|count| "x".repeat(count))
                        })
                        .unwrap_or_else(|| "fixture-result".to_owned())
                };
                diagnostics.push_str(&format!("\nresultBytes={}", message.len()));
                if let Some(path) = env::var_os("MCP_FIXTURE_LOG") {
                    let _ = fs::write(path, &diagnostics);
                }
                let response = if env::var_os("MCP_FIXTURE_RPC_ERROR").is_some() {
                    format!(
                        r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32042,"message":"upstream denied"}}}}"#
                    )
                } else {
                    [
                        r#"{"jsonrpc":"2.0","id":"#,
                        &id,
                        r#","result":{"content":[{"type":"text","text":""#,
                        &message,
                        r#""}],"isError":"#,
                        if error { "true" } else { "false" },
                        "}}",
                    ]
                    .concat()
                };
                let delay_ms = env::var("MCP_FIXTURE_DELAY_MS")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or_default();
                if delay_ms > 0 {
                    let late_response_log = env::var_os("MCP_FIXTURE_LATE_RESPONSE_LOG");
                    thread::spawn(move || {
                        thread::sleep(Duration::from_millis(delay_ms));
                        let sent = emit(&response);
                        if let Some(path) = late_response_log {
                            let _ = fs::write(path, if sent { "sent" } else { "failed" });
                        }
                    });
                    continue;
                }
                response
            }
            _ => format!(
                r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32601,"message":"unsupported fixture method"}}}}"#
            ),
        };
        if !emit(&response) {
            return;
        }
        if method == "tools/call" && env::var_os("MCP_FIXTURE_EXIT_AFTER_CALL").is_some() {
            if let Some(path) = env::var_os("MCP_FIXTURE_EXIT_AFTER_CALL_MARKER") {
                let _ = fs::write(path, "exited after tool call");
            }
            return;
        }
    }
    if let Some(path) = env::var_os("MCP_FIXTURE_EXIT") {
        let _ = fs::write(path, "stdin closed");
    }
}

fn listed_tools(additional_tool: bool) -> String {
    let count = env::var("MCP_FIXTURE_TOOL_COUNT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1);
    let count = count + usize::from(additional_tool);
    let description = env::var("MCP_FIXTURE_DESCRIPTION_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|count| "x".repeat(count))
        .unwrap_or_else(|| "Echoes request input".to_owned());
    let mut tools = Vec::with_capacity(count);
    for index in 0..count {
        let name = if index == 0 {
            "echo".to_owned()
        } else {
            format!("tool-{index}")
        };
        let mut tool = format!(
            r#"{{"name":"{name}","description":"{description}","inputSchema":{{"type":"object","properties":{{"message":{{"type":"string"}}}},"required":["message"]}},"annotations":{{"readOnlyHint":false}},"execution":{{"taskSupport":"optional"}}}}"#
        );
        if env::var_os("MCP_FIXTURE_TASK_REQUIRED").is_some() && index == count.saturating_sub(1) {
            tool = tool.replace("optional", "required");
        }
        tools.push(tool);
    }
    tools.join(",")
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
