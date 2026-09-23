pub use aifuel_core::AgentRunError as LaunchError;
pub use aifuel_core::{
    AccessMode, ExecutionMode, ManagedRunResult, OutputFormat, RunRequest, RunResult, RunStatus,
};
use aifuel_core::{
    AgentInteractionKind, ManagedRun, PendingRunInput, RunInputKind, RunManagementError,
    RunManagementErrorCode,
};
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

/// Execute a request through the shared owner-local Agent Run manager.
pub fn execute(request: &RunRequest) -> Result<ManagedRunResult, LaunchError> {
    let manager = crate::execution_run_manager().map_err(LaunchError::InvalidRequest)?;
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stderr = io::stderr();
    let mut diagnostics = stderr.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    execute_with_manager(
        request,
        &manager,
        &mut input,
        &mut output,
        &mut diagnostics,
        stdin.is_terminal() && stderr.is_terminal(),
    )
}

fn execute_with_manager(
    request: &RunRequest,
    manager: &aifuel_app::RunManager,
    input: &mut impl BufRead,
    output: &mut impl Write,
    diagnostics: &mut impl Write,
    interactive: bool,
) -> Result<ManagedRunResult, LaunchError> {
    let started = if let Some(session_id) = request.resume.as_deref() {
        manager.resume_session(session_id, request.clone())
    } else {
        manager.start_run(request.clone())
    }
    .map_err(|error| map_management_error(error, request.provider))?;
    let run_id = started.run_id;
    let mut announced_approval = None::<String>;
    let mut event_cursor = None;
    let mut last_sequence = 0;
    let mut output_event_seen = false;

    loop {
        consume_events(
            manager,
            &run_id,
            request.output,
            output,
            diagnostics,
            &mut event_cursor,
            &mut last_sequence,
            &mut output_event_seen,
        )?;
        let run = manager
            .get_run(&run_id)
            .map_err(|error| map_management_error(error, request.provider))?;
        if run.state.is_terminal() {
            consume_events(
                manager,
                &run_id,
                request.output,
                output,
                diagnostics,
                &mut event_cursor,
                &mut last_sequence,
                &mut output_event_seen,
            )?;
            let result = manager
                .get_result(&run_id)
                .map_err(|error| map_management_error(error, request.provider));
            manager.shutdown();
            if request.output == OutputFormat::Text
                && !output_event_seen
                && let Ok(result) = &result
                && let Some(output_text) = &result.output
            {
                output.write_all(output_text.as_bytes())?;
                output.flush()?;
            }
            return result;
        }

        if let Some(pending) = run.pending_input.as_ref()
            && announced_approval.as_deref() != Some(pending.input_id.as_str())
        {
            match handle_pending_input(manager, &run, pending, input, diagnostics, interactive) {
                Ok(PendingInputAction::Answered) => announced_approval = None,
                Ok(PendingInputAction::WaitingForApproval) => {
                    announced_approval = Some(pending.input_id.clone());
                }
                Ok(PendingInputAction::Cancelled) => {
                    manager
                        .cancel_run(&run_id)
                        .map_err(|error| map_management_error(error, request.provider))?;
                }
                Err(error) => {
                    let _ = manager.cancel_run(&run_id);
                    manager.shutdown();
                    return Err(error);
                }
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[allow(clippy::too_many_arguments)]
fn consume_events(
    manager: &aifuel_app::RunManager,
    run_id: &str,
    output_format: OutputFormat,
    output: &mut impl Write,
    diagnostics: &mut impl Write,
    cursor: &mut Option<String>,
    last_sequence: &mut u64,
    output_event_seen: &mut bool,
) -> Result<(), LaunchError> {
    loop {
        let page = manager
            .read_events(run_id, cursor.as_deref(), None)
            .map_err(|error| LaunchError::InvalidRequest(error.to_string()))?;
        for event in page.events {
            if event.sequence <= *last_sequence {
                continue;
            }
            if page.gap && event.sequence > (*last_sequence).saturating_add(1) {
                let after_gap_sequence = *last_sequence;
                if output_format == OutputFormat::Jsonl {
                    let gap_record = serde_json::json!({
                        "type":"run_event_gap",
                        "run_id":run_id,
                        "after_sequence":after_gap_sequence
                    });
                    serde_json::to_writer(&mut *output, &gap_record)
                        .map_err(|error| LaunchError::InvalidRequest(error.to_string()))?;
                    output.write_all(b"\n")?;
                    output.flush()?;
                } else {
                    writeln!(
                        diagnostics,
                        "aifuel: run event history has a gap after sequence {after_gap_sequence}"
                    )?;
                    diagnostics.flush()?;
                }
            }
            *last_sequence = event.sequence;
            match output_format {
                OutputFormat::Text if event.kind == aifuel_core::RunEventKind::Output => {
                    if let Some(delta) = &event.data {
                        output.write_all(delta.as_bytes())?;
                        output.flush()?;
                    }
                    *output_event_seen = true;
                }
                OutputFormat::Jsonl => {
                    let is_output = event.kind == aifuel_core::RunEventKind::Output;
                    let record = serde_json::json!({"type":"run_event", "event":event});
                    serde_json::to_writer(&mut *output, &record)
                        .map_err(|error| LaunchError::InvalidRequest(error.to_string()))?;
                    output.write_all(b"\n")?;
                    output.flush()?;
                    if is_output {
                        *output_event_seen = true;
                    }
                }
                _ => {}
            }
        }
        *cursor = page.next_cursor;
        if cursor.is_none() {
            return Ok(());
        }
    }
}

enum PendingInputAction {
    Answered,
    WaitingForApproval,
    Cancelled,
}

fn handle_pending_input(
    manager: &aifuel_app::RunManager,
    run: &ManagedRun,
    pending: &PendingRunInput,
    input: &mut impl BufRead,
    diagnostics: &mut impl Write,
    interactive: bool,
) -> Result<PendingInputAction, LaunchError> {
    match pending.kind {
        RunInputKind::Ordinary => match pending.interaction_kind {
            AgentInteractionKind::OrdinaryInput => {
                answer_tool_questions(manager, run, pending, input, diagnostics, interactive)
            }
            AgentInteractionKind::McpElicitation => {
                answer_mcp_elicitation(manager, run, pending, input, diagnostics, interactive)
            }
            kind => Err(LaunchError::InvalidRequest(format!(
                "provider requested unsupported interactive input ({kind:?})"
            ))),
        },
        RunInputKind::Permission => {
            if !matches!(
                pending.interaction_kind,
                AgentInteractionKind::CommandApproval | AgentInteractionKind::FileChangeApproval
            ) {
                return Err(LaunchError::InvalidRequest(format!(
                    "provider requested permission through unsupported interaction kind {:?}; this request cannot be approved by the current Agent Run policy",
                    pending.interaction_kind
                )));
            }
            writeln!(diagnostics, "Permission requested: {}", pending.description)
                .map_err(LaunchError::Io)?;
            writeln!(
                diagnostics,
                "Approve or reject it with: aifuel approve --run {} --input {} --decision accept|decline|cancel",
                run.run_id, pending.input_id
            )
            .map_err(LaunchError::Io)?;
            diagnostics.flush().map_err(LaunchError::Io)?;
            Ok(PendingInputAction::WaitingForApproval)
        }
    }
}

fn answer_tool_questions(
    manager: &aifuel_app::RunManager,
    run: &ManagedRun,
    pending: &PendingRunInput,
    input: &mut impl BufRead,
    diagnostics: &mut impl Write,
    interactive: bool,
) -> Result<PendingInputAction, LaunchError> {
    if !interactive {
        return Err(LaunchError::InvalidRequest(
            "provider requested input, but aifuel run has no interactive terminal; rerun with a terminal to answer each question"
                .to_owned(),
        ));
    }
    if pending.questions.is_empty() {
        return Err(LaunchError::InvalidRequest(
            "provider's input request did not include question IDs; use its native CLI to answer"
                .to_owned(),
        ));
    }

    let mut answers = serde_json::Map::new();
    for question in &pending.questions {
        writeln!(diagnostics, "{}", question.text).map_err(LaunchError::Io)?;
        write!(diagnostics, "Answer ({}): ", question.id).map_err(LaunchError::Io)?;
        diagnostics.flush().map_err(LaunchError::Io)?;

        let mut answer = String::new();
        if input.read_line(&mut answer).map_err(LaunchError::Io)? == 0 {
            return Ok(PendingInputAction::Cancelled);
        }
        while answer.ends_with(['\n', '\r']) {
            answer.pop();
        }
        answers.insert(question.id.clone(), serde_json::Value::String(answer));
    }
    manager
        .answer_input_value(
            &run.run_id,
            &pending.input_id,
            serde_json::Value::Object(answers),
        )
        .map_err(|error| map_management_error(error, run.provider))?;
    Ok(PendingInputAction::Answered)
}

fn answer_mcp_elicitation(
    manager: &aifuel_app::RunManager,
    run: &ManagedRun,
    pending: &PendingRunInput,
    input: &mut impl BufRead,
    diagnostics: &mut impl Write,
    interactive: bool,
) -> Result<PendingInputAction, LaunchError> {
    if !interactive {
        return Err(LaunchError::InvalidRequest(
            "provider requested MCP form input, but aifuel run has no interactive terminal; rerun with a terminal and enter a JSON object"
                .to_owned(),
        ));
    }
    writeln!(diagnostics, "{}", pending.description).map_err(LaunchError::Io)?;
    if let Some(schema) = pending
        .parameters
        .as_ref()
        .and_then(|parameters| parameters.get("requestedSchema"))
    {
        writeln!(
            diagnostics,
            "Requested JSON schema: {}",
            serde_json::to_string(schema)
                .map_err(|error| LaunchError::InvalidRequest(error.to_string()))?
        )
        .map_err(LaunchError::Io)?;
    }
    write!(diagnostics, "JSON object: ").map_err(LaunchError::Io)?;
    diagnostics.flush().map_err(LaunchError::Io)?;
    let mut answer = String::new();
    if input.read_line(&mut answer).map_err(LaunchError::Io)? == 0 {
        return Ok(PendingInputAction::Cancelled);
    }
    while answer.ends_with(['\n', '\r']) {
        answer.pop();
    }
    let response: serde_json::Value = serde_json::from_str(&answer).map_err(|error| {
        LaunchError::InvalidRequest(format!(
            "MCP elicitation response must be a valid JSON object: {error}"
        ))
    })?;
    if !response.is_object() {
        return Err(LaunchError::InvalidRequest(
            "MCP elicitation response must be a JSON object".to_owned(),
        ));
    }
    manager
        .answer_input_value(&run.run_id, &pending.input_id, response)
        .map_err(|error| map_management_error(error, run.provider))?;
    Ok(PendingInputAction::Answered)
}

fn map_management_error(
    error: RunManagementError,
    provider: aifuel_core::ProviderKey,
) -> LaunchError {
    match error.code {
        RunManagementErrorCode::AgentUnavailable => LaunchError::UnsupportedProvider(provider),
        RunManagementErrorCode::ConnectionTimeout | RunManagementErrorCode::DeadlineExceeded => {
            LaunchError::Timeout(error.message)
        }
        _ => LaunchError::InvalidRequest(error.message),
    }
}

pub fn read_prompt_file(path: &Path) -> Result<String, LaunchError> {
    std::fs::read_to_string(path).map_err(|error| {
        LaunchError::InvalidRequest(format!(
            "could not read prompt file {}: {error}",
            path.display()
        ))
    })
}

pub fn read_stdin_prompt() -> Result<String, LaunchError> {
    let mut prompt = String::new();
    io::stdin()
        .read_to_string(&mut prompt)
        .map_err(LaunchError::Io)?;
    Ok(prompt)
}

#[cfg(test)]
#[path = "launcher_tests/mod.rs"]
mod tests;
