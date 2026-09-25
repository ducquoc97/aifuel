use crate::agent_execution::{
    CliExecutionAdapter, ExecutionCapabilities, ParsedProviderOutput, parse_public_output,
};
use aifuel_core::{
    AccessMode, AgentRunError, AgentSetupGuidance, OutputFormat, ProviderKey, RunRequest,
};

pub(crate) static ADAPTER: CliExecutionAdapter = CliExecutionAdapter::new(
    ProviderKey::Devin,
    "devin",
    &["--help"],
    &["--print", "--model", "--permission-mode"],
    build_args,
    parse_output,
    // `devin -p --permission-mode auto` was live-verified as a read-only
    // boundary: a prompt asking to create a file produced "warning: rejected a
    // tool call that requires confirmation. Running in non-interactive mode."
    // and created nothing, while read-only tools were auto-approved.
    ExecutionCapabilities::new(false, false, true, false).with_read_only(),
)
// Use the documented flag; `devin --version` exits cleanly (devin 3000.11.3).
.with_version_probe(&["--version"])
.with_setup_guidance(AgentSetupGuidance {
    install: "macOS/Linux/WSL: `curl -fsSL https://cli.devin.ai/install.sh | bash`; Windows PowerShell: `irm https://static.devin.ai/cli/setup.ps1 | iex`.",
    login: "Run `devin auth login` and complete the browser sign-in prompt.",
    check: "Run `devin --version` to check the install; `devin auth status` reports the signed-in account and plan.",
    documentation_url: "https://docs.devin.ai/cli",
});

fn build_args(request: &RunRequest) -> Result<Vec<String>, AgentRunError> {
    let mut args = vec![
        "-p".to_owned(),
        request.prompt.clone(),
        "--respect-workspace-trust".to_owned(),
        "false".to_owned(),
    ];
    if let Some(model) = &request.model {
        args.extend(["--model".to_owned(), model.clone()]);
    }
    match request.access {
        AccessMode::ReadOnly => {
            args.extend(["--permission-mode".to_owned(), "auto".to_owned()]);
        }
        AccessMode::WorkspaceWrite => {
            args.extend(["--permission-mode".to_owned(), "accept-edits".to_owned()]);
        }
    }
    match request.output {
        OutputFormat::Text => {}
        OutputFormat::Json => {
            return Err(AgentRunError::InvalidRequest(
                "devin cannot provide verified JSON output".to_owned(),
            ));
        }
        OutputFormat::Jsonl => unreachable!("JSONL was rejected during preflight"),
    }
    Ok(args)
}

fn parse_output(format: OutputFormat, text: &str) -> ParsedProviderOutput {
    if format == OutputFormat::Text {
        return ParsedProviderOutput {
            output: text.to_owned(),
            terminal: Some(true),
            ..ParsedProviderOutput::default()
        };
    }
    parse_public_output(format, text)
}
