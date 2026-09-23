mod capabilities;
mod cli;
mod inspection;
mod output;
mod process;

pub(crate) use capabilities::ExecutionCapabilities;
pub(crate) use cli::CliExecutionAdapter;
pub(crate) use output::{ParsedProviderOutput, parse_public_output};
pub(crate) use process::{
    MAX_CAPTURE_BYTES, kill_and_wait, owned_command, program_candidates, read_bounded,
};

#[cfg(test)]
#[path = "agent_execution_tests.rs"]
mod tests;
