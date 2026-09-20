pub use aifuel_core::AgentRunError as LaunchError;
use aifuel_core::RunCancellationToken;
pub use aifuel_core::{AccessMode, ExecutionMode, OutputFormat, RunRequest, RunResult, RunStatus};
use std::io::{self, Read};
use std::path::Path;

/// Execute a request through the registered Agent Run adapters.
pub fn execute(request: &RunRequest) -> Result<RunResult, LaunchError> {
    crate::agent_run_facade().execute(request, &RunCancellationToken::new())
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
