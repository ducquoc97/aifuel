use crate::cli::next_value;
use aifuel_app::{AgentMcpSetupAction, AgentMcpSetupOptions, AgentMcpSetupResult};

pub fn run(args: &[String]) -> Result<u8, String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("Usage: aifuel mcp setup [--agent MCP_HOST_ID ...] [--dry-run] [--remove]");
        println!();
        println!("Applies the AI Fuel Gateway registration by default.");
        println!(
            "--dry-run previews without writing; --remove removes only an unchanged AI Fuel-owned entry."
        );
        println!(
            "--agent may repeat; with --remove and no --agent, every known MCP Host is cleaned."
        );
        return Ok(0);
    }

    let mut agents = Vec::new();
    let mut dry_run = false;
    let mut remove = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--agent" => agents.push(next_value(args, &mut index, "--agent")?),
            "--dry-run" => dry_run = true,
            "--remove" => remove = true,
            unknown => return Err(format!("unknown argument {unknown:?} for mcp setup")),
        }
        index += 1;
    }
    if agents.iter().any(|agent| agent.trim().is_empty()) {
        return Err("--agent MCP_HOST_ID cannot be empty".to_owned());
    }
    let all_hosts = agents.is_empty();
    if all_hosts {
        if !remove {
            return Err("mcp setup requires --agent MCP_HOST_ID".to_owned());
        }
        agents = aifuel_providers::agent_mcp_registration_host_ids()
            .map(str::to_owned)
            .collect();
    }

    let options = AgentMcpSetupOptions { dry_run, remove };
    let mut failures = Vec::new();
    let mut restart_hint = false;
    for agent in &agents {
        let result = match aifuel::agent_mcp_setup_facade(agent) {
            Ok(setup) => setup.run(options).map_err(|error| error.to_string()),
            // A host whose configuration home cannot be resolved has nowhere
            // for a registration to live; only an explicit --agent makes a
            // missing home an error worth failing on.
            Err(error) if all_hosts => {
                println!("MCP Host {agent}: skipped ({error})");
                continue;
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(result) => {
                restart_hint |= matches!(
                    result.action,
                    AgentMcpSetupAction::Applied
                        | AgentMcpSetupAction::Updated
                        | AgentMcpSetupAction::Removed
                );
                print_result(&result);
            }
            Err(error) => {
                eprintln!("MCP Host {agent}: {error}");
                failures.push(agent.clone());
            }
        }
    }
    if !failures.is_empty() {
        return Err(format!("mcp setup failed for {}", failures.join(", ")));
    }
    if restart_hint {
        println!("Restart the MCP Host to load the configuration change.");
    }
    Ok(0)
}

fn print_result(result: &AgentMcpSetupResult) {
    let message = match result.action {
        AgentMcpSetupAction::Applied => "registration applied",
        AgentMcpSetupAction::Updated => "AI Fuel-managed registration updated",
        AgentMcpSetupAction::AlreadyConfigured => {
            "identical registration already exists; no configuration change was needed"
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
    if let Some(backup) = &result.backup_file {
        println!("Configuration backup: {}", backup.display());
    }
}
