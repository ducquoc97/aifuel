//! Codex App Server execution with a bidirectional approval and input channel.

use crate::agent_execution::{
    MAX_CAPTURE_BYTES, kill_and_wait, owned_command, program_candidates, read_bounded,
};
use crate::codex::interaction;
mod mcp;
mod protocol;
mod settings;
use aifuel_core::{
    AccessMode, AgentRunError, AgentRunOutputHandler, ExecutionMode, RunCancellationToken,
    RunRequest, RunResult, RunStatus,
};
use mcp::{app_server_config, wait_for_mcp_tools};
use process_wrap::tokio::TokioChildWrapper;
use protocol::{expect_successful_response, protocol_error, read_message, send};
use serde_json::{Value, json};
use settings::reported_run_settings;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::time::timeout;

const SETUP_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const APP_SERVER_ARGS: &[&str] = &[
    "app-server",
    "--stdio",
    "--disable",
    "plugins",
    "--disable",
    "remote_plugin",
    "--config",
    "mcp_servers={}",
];

struct ProtocolDeadlines {
    setup: Instant,
    task: Option<Instant>,
}

pub(crate) async fn execute(
    request: &RunRequest,
    cancellation: &RunCancellationToken,
    output_handler: Option<&dyn AgentRunOutputHandler>,
) -> Result<RunResult, AgentRunError> {
    let started = Instant::now();
    let task_deadline = request.timeout.map(|duration| started + duration);
    let setup_deadline = Instant::now() + SETUP_TIMEOUT;
    let setup_deadline =
        task_deadline.map_or(setup_deadline, |deadline| deadline.min(setup_deadline));
    let temporary = if request.working_directory.is_none() {
        Some(TemporaryDirectory::new()?)
    } else {
        None
    };
    let cwd = request
        .working_directory
        .as_deref()
        .or_else(|| temporary.as_ref().map(TemporaryDirectory::path))
        .expect("Codex App Server requires a working directory");

    let mut child = spawn_app_server(cwd)?;
    let stdout = child.stdout().take().expect("App Server stdout was piped");
    let stderr = child.stderr().take().expect("App Server stderr was piped");
    let mut stdout = BufReader::new(stdout);
    let stderr_reader = tokio::spawn(read_bounded(stderr, MAX_CAPTURE_BYTES));
    let mut stdin = child.stdin().take().expect("App Server stdin was piped");

    let result = run_protocol(
        &mut stdin,
        &mut stdout,
        request,
        cancellation,
        output_handler,
        ProtocolDeadlines {
            setup: setup_deadline,
            task: task_deadline,
        },
        cwd,
    )
    .await;
    if result.is_err() || !child.try_wait().map_err(AgentRunError::Io)?.is_some() {
        let _ = kill_and_wait(&mut child).await;
    }
    let stderr = timeout(SHUTDOWN_TIMEOUT, stderr_reader)
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(Result::ok)
        .map(|captured| captured.text)
        .unwrap_or_default();
    result.map(|mut result| {
        if !stderr.trim().is_empty() {
            result.diagnostics = Some(stderr);
        }
        result
    })
}

fn spawn_app_server(cwd: &Path) -> Result<Box<dyn TokioChildWrapper>, AgentRunError> {
    for candidate in program_candidates("codex") {
        let mut command = owned_command(&candidate, |command| {
            command
                .args(APP_SERVER_ARGS)
                .current_dir(cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        });
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(AgentRunError::Io(error)),
        }
    }
    Err(AgentRunError::InvalidRequest(
        "provider executable \"codex\" was not found".to_owned(),
    ))
}

async fn run_protocol<W, R>(
    stdin: &mut W,
    stdout: &mut BufReader<R>,
    request: &RunRequest,
    cancellation: &RunCancellationToken,
    output_handler: Option<&dyn AgentRunOutputHandler>,
    deadlines: ProtocolDeadlines,
    cwd: &Path,
) -> Result<RunResult, AgentRunError>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let ProtocolDeadlines {
        setup: setup_deadline,
        task: task_deadline,
    } = deadlines;
    send(stdin, json!({
        "id": 0,
        "method": "initialize",
        "params": {
            "clientInfo": {"name":"aifuel","title":"AI Fuel","version":env!("CARGO_PKG_VERSION")},
            "capabilities": {"experimentalApi": true}
        }
    }), Some(setup_deadline)).await?;
    expect_successful_response(stdout, 0, setup_deadline, cancellation).await?;
    send(
        stdin,
        json!({"method":"initialized","params":{}}),
        Some(setup_deadline),
    )
    .await?;

    let sandbox = match request.access {
        AccessMode::ReadOnly => "read-only",
        AccessMode::WorkspaceWrite => "workspace-write",
    };
    let config = app_server_config(request)?;
    let thread_method = if request.resume.is_some() {
        "thread/resume"
    } else {
        "thread/start"
    };
    let thread_params = match request.resume.as_deref() {
        Some(thread_id) => json!({
            "threadId":thread_id,
            "cwd":cwd,
            "model":request.model,
            "approvalPolicy":"on-request",
            "sandbox":sandbox,
            "config":config,
            "excludeTurns":true
        }),
        None => json!({
            "cwd":cwd,
            "model":request.model,
            "ephemeral":false,
            "approvalPolicy":"on-request",
            "sandbox":sandbox,
            "config":config
        }),
    };
    send(
        stdin,
        json!({
            "id":1,
            "method":thread_method,
            "params":thread_params
        }),
        Some(setup_deadline),
    )
    .await?;
    let thread_response =
        expect_successful_response(stdout, 1, setup_deadline, cancellation).await?;
    let thread_id = thread_response
        .get("result")
        .and_then(|result| result.get("thread"))
        .and_then(|thread| thread.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| protocol_error("Codex App Server did not return a thread ID"))?
        .to_owned();
    let reported_settings = reported_run_settings(
        &thread_response,
        request.model.as_deref(),
        request.effort.as_deref(),
    );
    let mut effective_model = reported_settings.model;
    let mut effective_effort = reported_settings.effort;

    if let Some(tools) = request
        .external_tools
        .as_deref()
        .filter(|tools| !tools.is_empty())
    {
        wait_for_mcp_tools(
            stdin,
            stdout,
            &thread_id,
            tools,
            setup_deadline,
            cancellation,
        )
        .await?;
    }

    send(stdin, json!({
        "id":2,
        "method":"turn/start",
        "params":{
            "threadId":thread_id,
            "input":[{"type":"text","text":request.prompt}],
            "model":request.model,
            "effort":request.effort,
            "approvalPolicy":"on-request",
            "sandboxPolicy": if request.access == AccessMode::ReadOnly { json!({"type":"readOnly"}) } else { json!({"type":"workspaceWrite"}) },
            "cwd":cwd
        }
    }), Some(setup_deadline)).await?;
    expect_successful_response(stdout, 2, setup_deadline, cancellation).await?;

    let mut answer = String::new();
    let mut saw_answer_output = false;
    let (terminal_status, diagnostics) = loop {
        if cancellation.is_cancelled() {
            break (
                Some("cancelled".to_owned()),
                Some("Agent Run was cancelled".to_owned()),
            );
        }
        if task_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break (
                Some("timeout".to_owned()),
                Some("Agent Run exceeded its deadline".to_owned()),
            );
        }
        let message = match read_message(stdout, task_deadline, cancellation).await {
            Ok(Some(message)) => message,
            Ok(None) if cancellation.is_cancelled() => {
                break (
                    Some("cancelled".to_owned()),
                    Some("Agent Run was cancelled".to_owned()),
                );
            }
            Ok(None) => {
                break (
                    Some("failed".to_owned()),
                    Some("Codex App Server closed its output".to_owned()),
                );
            }
            Err(AgentRunError::Cancelled) => {
                break (
                    Some("cancelled".to_owned()),
                    Some("Agent Run was cancelled".to_owned()),
                );
            }
            Err(AgentRunError::Timeout(error)) => break (Some("timeout".to_owned()), Some(error)),
            Err(error) => break (Some("failed".to_owned()), Some(error.to_string())),
        };
        if message.get("id").is_some() && message.get("method").is_some() {
            let Some(pending) = interaction::parse_pending(&message) else {
                break (
                    Some("failed".to_owned()),
                    Some("Codex App Server sent an unsupported server request".to_owned()),
                );
            };
            let Some(handler) = request.interaction_handler.as_ref() else {
                break (
                    Some("failed".to_owned()),
                    Some("Codex App Server requested input without an owner handler".to_owned()),
                );
            };
            let interaction_request = interaction::application_request(&pending);
            let handler = Arc::clone(handler);
            let cancellation = cancellation.clone();
            let interaction_result = tokio::task::spawn_blocking(move || {
                handler.interact(interaction_request, &cancellation)
            })
            .await
            .map_err(|_| "interaction handler stopped unexpectedly".to_owned());
            let response = match interaction_result {
                Ok(Ok(response)) => response,
                Ok(Err(AgentRunError::Cancelled)) => {
                    break (
                        Some("cancelled".to_owned()),
                        Some("Agent Run was cancelled".to_owned()),
                    );
                }
                Ok(Err(AgentRunError::Timeout(error))) => {
                    break (Some("timeout".to_owned()), Some(error));
                }
                Ok(Err(error)) => break (Some("failed".to_owned()), Some(error.to_string())),
                Err(error) => break (Some("failed".to_owned()), Some(error)),
            };
            let response = match interaction::response_message(&pending, response) {
                Ok(response) => response,
                Err(error) => break (Some("failed".to_owned()), Some(error)),
            };
            if let Err(error) = send(stdin, response, task_deadline).await {
                let status = match error {
                    AgentRunError::Cancelled => "cancelled",
                    AgentRunError::Timeout(_) => "timeout",
                    _ => "failed",
                };
                break (Some(status.to_owned()), Some(error.to_string()));
            }
            continue;
        }
        match message.get("method").and_then(Value::as_str) {
            Some("item/agentMessage/delta") => {
                if let Some(delta) = message["params"]["delta"].as_str() {
                    if !delta.is_empty() {
                        saw_answer_output = true;
                    }
                    capture_answer_delta(output_handler, &mut answer, delta);
                }
            }
            Some("item/completed") => {
                let item = &message["params"]["item"];
                if item["type"] == "agentMessage" && !saw_answer_output && answer.is_empty() {
                    if let Some(text) = item["text"].as_str() {
                        capture_answer_delta(output_handler, &mut answer, text);
                    } else if let Some(parts) = item["content"].as_array() {
                        for part in parts {
                            if part["type"] == "outputText"
                                && let Some(text) = part["text"].as_str()
                            {
                                capture_answer_delta(output_handler, &mut answer, text);
                            }
                        }
                    }
                }
            }
            Some("turn/completed") => {
                let turn = &message["params"]["turn"];
                break (
                    turn["status"].as_str().map(str::to_owned),
                    turn["error"]["message"].as_str().map(str::to_owned),
                );
            }
            Some("model/rerouted") => {
                effective_model = None;
                effective_effort = None;
            }
            Some("error") => {
                break (
                    Some("failed".to_owned()),
                    message["params"]["error"]["message"]
                        .as_str()
                        .map(str::to_owned),
                );
            }
            _ => {}
        }
        if let Some(error) = message
            .get("error")
            .and_then(|value| value.get("message"))
            .and_then(Value::as_str)
        {
            break (Some("failed".to_owned()), Some(error.to_owned()));
        }
    };
    let status = match terminal_status.as_deref() {
        Some("completed") => RunStatus::Succeeded,
        Some("cancelled") | Some("interrupted") if cancellation.is_cancelled() => {
            RunStatus::Cancelled
        }
        Some("cancelled") => RunStatus::Cancelled,
        Some("timeout") => RunStatus::Timeout,
        Some("interrupted") => RunStatus::Failed,
        _ => RunStatus::Failed,
    };
    Ok(RunResult {
        run_id: format!("codex-app-{}-{}", std::process::id(), now_nanos()),
        local_session_id: thread_id.clone(),
        session_id: Some(thread_id),
        resumed_from: request.resume.clone(),
        provider_id: request.provider,
        requested_model: request.model.clone(),
        requested_effort: request.effort.clone(),
        effective_model,
        effective_effort,
        requested_account_id: request.account.clone(),
        account_id: None,
        execution_mode: if request.working_directory.is_some() {
            ExecutionMode::Project
        } else {
            ExecutionMode::PromptOnly
        },
        permission_profile: request.access,
        status,
        exit_code: Some(if status == RunStatus::Succeeded { 0 } else { 1 }),
        output: answer,
        error: diagnostics.clone(),
        diagnostics,
        timed_out: status == RunStatus::Timeout,
        working_directory: cwd.to_path_buf(),
    })
}

fn capture_answer_delta(
    output_handler: Option<&dyn AgentRunOutputHandler>,
    answer: &mut String,
    delta: &str,
) {
    if delta.is_empty() {
        return;
    }
    if let Some(output_handler) = output_handler {
        output_handler.on_output(delta);
        return;
    }
    let remaining = MAX_CAPTURE_BYTES.saturating_sub(answer.len());
    let retained = utf8_prefix_len(delta, remaining);
    answer.push_str(&delta[..retained]);
}

fn utf8_prefix_len(value: &str, limit: usize) -> usize {
    let mut end = limit.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn new() -> io::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "aifuel-codex-app-{}-{}",
            std::process::id(),
            now_nanos()
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.0);
    }
}

#[cfg(test)]
#[path = "app_server/tests.rs"]
mod tests;
