use crate::cli::next_value;
use aifuel_app::{AgentMcpSetupAction, AgentMcpSetupOptions};

pub fn run(args: &[String]) -> Result<u8, String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("Usage: aifuel mcp setup --agent MCP_HOST_ID [--dry-run] [--remove]");
        println!();
        println!("Applies the AI Fuel Gateway registration by default.");
        println!(
            "--dry-run previews without writing; --remove removes only an unchanged AI Fuel-owned entry."
        );
        return Ok(0);
    }

    let mut agent = None;
    let mut dry_run = false;
    let mut remove = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--agent" => {
                if agent.is_some() {
                    return Err("setup accepts one --agent value".to_owned());
                }
                agent = Some(next_value(args, &mut index, "--agent")?);
            }
            "--dry-run" => dry_run = true,
            "--remove" => remove = true,
            unknown => return Err(format!("unknown argument {unknown:?} for mcp setup")),
        }
        index += 1;
    }
    let agent = agent.ok_or_else(|| "mcp setup requires --agent MCP_HOST_ID".to_owned())?;
    if agent.trim().is_empty() {
        return Err("--agent MCP_HOST_ID cannot be empty".to_owned());
    }

    let setup = aifuel::agent_mcp_setup_facade(&agent)?;
    let result = setup
        .run(AgentMcpSetupOptions { dry_run, remove })
        .map_err(|error| error.to_string())?;
    let message = match result.action {
        AgentMcpSetupAction::Applied => "registration applied",
        AgentMcpSetupAction::Updated => "AI Fuel-managed registration updated",
        AgentMcpSetupAction::AlreadyConfigured => {
            "identical registration already exists; ownership was not adopted"
        }
        AgentMcpSetupAction::WouldApply => "dry run: would apply registration",
        AgentMcpSetupAction::WouldUpdate => {
            "dry run: would update the AI Fuel-managed registration"
        }
        AgentMcpSetupAction::Removed => "registration removed",
        AgentMcpSetupAction::WouldRemove => "dry run: would remove registration",
        AgentMcpSetupAction::AlreadyAbsent => "registration is already absent",
        AgentMcpSetupAction::StaleReceiptCleared => {
            "stale ownership receipt cleared; registration was already absent"
        }
        AgentMcpSetupAction::WouldClearStaleReceipt => {
            "dry run: would clear a stale ownership receipt"
        }
    };
    println!("MCP Host {}: {message}", result.host_id);
    if let Some(backup) = result.backup_file {
        println!("Configuration backup: {}", backup.display());
    }
    if matches!(
        result.action,
        AgentMcpSetupAction::Applied | AgentMcpSetupAction::Updated | AgentMcpSetupAction::Removed
    ) {
        println!("Restart the MCP Host to load the configuration change.");
    }
    Ok(0)
}
