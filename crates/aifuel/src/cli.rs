use aifuel_core::{ProviderKey, ProviderStatus, StatusReport};

use crate::dashboard;

pub fn run<I>(args: I) -> Result<u8, String>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();
    if args.first().map(String::as_str) == Some("run") {
        return aifuel::run_cli::run(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("profile") {
        return aifuel::profile::run(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("model") {
        return aifuel::model_cli::run(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("approve") {
        return run_local_approval(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("mcp") {
        return match args.get(1).map(String::as_str) {
            None => {
                aifuel_mcp::serve(aifuel::monitoring_facade()?)?;
                Ok(0)
            }
            Some("gateway") => run_mcp_gateway(&args[2..]),
            Some("servers") => aifuel::mcp_catalog::run(&args[2..]),
            Some("execution") => {
                if !args[2..].is_empty() {
                    return Err("Usage: aifuel mcp execution".to_owned());
                }
                let selection =
                    aifuel_app::selection::SelectionStore::load(aifuel::execution_config_path()?)
                        .map_err(|error| error.to_string())?;
                let manager = aifuel::execution_run_manager()?;
                let catalog = aifuel::model_catalog_snapshot()?;
                let runtime = tokio::runtime::Runtime::new()
                    .map_err(|error| format!("could not start model catalog runtime: {error}"))?;
                let refresh_catalog = move |provider: Option<ProviderKey>| {
                    let providers = provider.map_or_else(|| ProviderKey::ALL.to_vec(), |p| vec![p]);
                    providers
                        .into_iter()
                        .map(|provider| runtime.block_on(aifuel::refresh_model_catalog(provider)))
                        .collect::<Result<Vec<_>, _>>()
                };
                aifuel_mcp::execution::serve_with_selection_and_catalog_refresh(
                    manager,
                    selection,
                    catalog,
                    refresh_catalog,
                )?;
                Ok(0)
            }
            Some("setup") => crate::mcp_setup::run(&args[2..]),
            Some(unknown) => Err(format!("unknown MCP command {unknown:?}; use --help")),
        };
    }

    let mut json = false;
    let mut text = false;
    let mut no_browser = false;
    let mut host = "127.0.0.1".to_owned();
    let mut port = 8787_u16;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => json = true,
            "--text" => text = true,
            "--no-browser" => no_browser = true,
            "--host" => host = next_value(&args, &mut index, "--host")?,
            "--port" => {
                port = next_value(&args, &mut index, "--port")?
                    .parse()
                    .map_err(|_| "port must be a number between 0 and 65535".to_owned())?;
            }
            "--help" | "-h" => {
                print_help();
                return Ok(0);
            }
            unknown => return Err(format!("unknown argument {unknown:?}; use --help")),
        }
        index += 1;
    }

    if json && text {
        return Err("choose either --json or --text, not both".to_owned());
    }
    if !json && !text {
        dashboard::serve(&host, port, !no_browser, aifuel::monitoring_facade()?)?;
        return Ok(0);
    }

    let facade = aifuel::monitoring_facade()?;
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| format!("could not start collection runtime: {error}"))?;
    let report = runtime.block_on(facade.collect());

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|error| format!("could not encode JSON: {error}"))?
        );
    } else {
        print!("{}", render_status_text(&report));
    }
    print_status_diagnostics(&report);

    Ok(if report.collection.errors.is_empty() {
        0
    } else {
        1
    })
}

fn run_local_approval(args: &[String]) -> Result<u8, String> {
    #[cfg(any(unix, windows))]
    {
        run_local_approval_supported(args)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = args;
        Err("local approval IPC is not supported on this platform yet".to_owned())
    }
}

#[cfg(any(unix, windows))]
fn run_local_approval_supported(args: &[String]) -> Result<u8, String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "Usage: aifuel approve --run RUN_ID --input INPUT_ID --decision accept|decline|cancel"
        );
        println!("Delivers one pending permission decision to its owning local Agent Run process.");
        return Ok(0);
    }
    let mut run_id = None;
    let mut input_id = None;
    let mut decision = None;
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        index += 1;
        let value = args
            .get(index)
            .ok_or_else(|| format!("{flag} requires a value"))?;
        match flag {
            "--run" => run_id = Some(value.clone()),
            "--input" => input_id = Some(value.clone()),
            "--decision" => decision = Some(value.clone()),
            _ => return Err(format!("unknown approval argument {flag:?}")),
        }
        index += 1;
    }
    let run_id = run_id.ok_or_else(|| "approve requires --run RUN_ID".to_owned())?;
    let input_id = input_id.ok_or_else(|| "approve requires --input INPUT_ID".to_owned())?;
    let decision = match decision.as_deref() {
        Some("accept") => aifuel_app::LocalApprovalDecision::Accept,
        Some("decline") => aifuel_app::LocalApprovalDecision::Decline,
        Some("cancel") => aifuel_app::LocalApprovalDecision::Cancel,
        _ => return Err("--decision must be accept, decline, or cancel".to_owned()),
    };
    aifuel::submit_local_approval(&run_id, &input_id, decision)?;
    println!("Approval response delivered.");
    Ok(0)
}

fn print_help() {
    println!("aifuel - monitor and explicitly launch AI coding providers");
    println!();
    println!("Usage: aifuel [--text | --json]");
    println!("       aifuel run --provider PROVIDER_ID [OPTIONS]");
    println!("       aifuel profile list|save|remove");
    println!("       aifuel model list|refresh [--provider PROVIDER_ID] [--json]");
    println!("       aifuel approve --run RUN_ID --input INPUT_ID --decision DECISION");
    println!("       aifuel mcp");
    println!("       aifuel mcp execution");
    println!("       aifuel mcp gateway --agent MCP_HOST_ID [--tool GATEWAY_TOOL_NAME ...]");
    println!("       aifuel mcp setup --agent MCP_HOST_ID [--dry-run] [--remove]");
    println!("       aifuel mcp servers list|validate|add|remove|select");
    println!();
    println!("The default command collects live status for discovered providers.");
    println!("run delegates one explicit prompt to a verified provider CLI.");
    println!("mcp serves read-only status over standard input and output.");
    println!("mcp execution manages Agent Runs owned by its standard-input connection.");
    println!("mcp gateway serves selected external MCP tools over standard input and output.");
    println!("mcp setup previews, applies, or removes an AI Fuel Gateway registration.");
}

fn run_mcp_gateway(args: &[String]) -> Result<u8, String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("Usage: aifuel mcp gateway --agent MCP_HOST_ID [--tool GATEWAY_TOOL_NAME ...]");
        println!();
        println!("Serves the selected external MCP server over standard input and output.");
        println!("The central catalog is aifuel/mcp.json in the user config directory.");
        println!("When --tool is supplied, only those exact Gateway tool names are exposed.");
        return Ok(0);
    }

    let mut agent = None;
    let mut allowed_tools = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--agent" => {
                if agent.is_some() {
                    return Err("gateway accepts one --agent value".to_owned());
                }
                agent = Some(next_value(args, &mut index, "--agent")?);
            }
            "--tool" => allowed_tools.push(next_value(args, &mut index, "--tool")?),
            unknown => return Err(format!("unknown argument {unknown:?} for mcp gateway")),
        }
        index += 1;
    }
    let agent = agent.ok_or_else(|| "mcp gateway requires --agent MCP_HOST_ID".to_owned())?;
    if agent.trim().is_empty() {
        return Err("--agent MCP_HOST_ID cannot be empty".to_owned());
    }

    let facade = aifuel::mcp_gateway_facade(&agent)?;
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| format!("could not start MCP Gateway runtime: {error}"))?;
    let allowed_tools = (!allowed_tools.is_empty()).then_some(allowed_tools);
    runtime.block_on(aifuel_mcp::gateway::serve_with_tool_allowlist(
        facade,
        allowed_tools,
    ))?;
    Ok(0)
}

fn render_status_text(report: &StatusReport) -> String {
    let mut output = format!("aifuel   updated {}\n", format_clock(report.generated_at));
    if report.providers.is_empty() {
        output.push_str("\nNo provider-specific logins found.\n");
    }
    for provider in &report.providers {
        output.push('\n');
        output.push_str(&format!(
            "{}  {}  {}\n",
            if provider.status == ProviderStatus::Ok {
                "●"
            } else {
                "○"
            },
            provider.name,
            provider.status
        ));
        if let Some(detail) = &provider.detail {
            output.push_str(&format!("  {detail}\n"));
        }
        if let Some(reset_credits) = &provider.reset_credits {
            let noun = if reset_credits.available_count == 1 {
                "usage limit reset"
            } else {
                "usage limit resets"
            };
            output.push_str("  Redeem usage limit reset\n");
            output.push_str(&format!(
                "  You have {} {noun} available.\n",
                reset_credits.available_count
            ));
            for credit in &reset_credits.credits {
                let title = credit
                    .title
                    .as_deref()
                    .or(credit.reset_type.as_deref())
                    .unwrap_or("Usage limit reset");
                let expiry = credit
                    .expires_at
                    .map(format_credit_expiry)
                    .unwrap_or_else(|| "expiry unavailable".to_owned());
                output.push_str(&format!("  {title:<28} expires {expiry}\n"));
            }
        }
        for window in &provider.windows {
            let remaining = window
                .remaining_percent
                .map(|value| format!("{value:.1}% left"))
                .unwrap_or_else(|| "n/a".to_owned());
            let reset = window
                .resets_at
                .map(|value| {
                    format!(
                        "resets in {}",
                        format_countdown(value - report.generated_at)
                    )
                })
                .unwrap_or_else(|| "no reset".to_owned());
            output.push_str(&format!("  {:<28} {remaining:<10} {reset}\n", window.label));
        }
    }
    output
}

fn print_status_diagnostics(report: &StatusReport) {
    for error in &report.collection.errors {
        eprintln!(
            "Provider status failed for {}: {}",
            error.provider_id.as_deref().unwrap_or("unknown provider"),
            error.message
        );
    }
}

fn format_clock(timestamp: f64) -> String {
    format_local_timestamp(timestamp, "%H:%M:%S")
}

fn format_local_timestamp(timestamp: f64, pattern: &str) -> String {
    chrono::DateTime::from_timestamp(timestamp as i64, 0)
        .map(|value| {
            value
                .with_timezone(&chrono::Local)
                .format(pattern)
                .to_string()
        })
        .unwrap_or_else(|| "unknown time".to_owned())
}

fn format_credit_expiry(timestamp: f64) -> String {
    format_local_timestamp(timestamp, "%Y-%m-%d %H:%M")
}

fn format_countdown(seconds: f64) -> String {
    if seconds <= 0.0 {
        return "resetting".to_owned();
    }
    let seconds = seconds.round() as u64;
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;
    if days > 0 {
        format!("{days}d {hours:02}h {minutes:02}m")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else {
        format!("{minutes}m {seconds:02}s")
    }
}

pub(super) fn next_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aifuel_core::{
        ProviderKey, ProviderUsage, ResetCredit, ResetCredits, STATUS_SCHEMA_VERSION,
    };

    #[test]
    fn text_output_has_an_intentional_empty_state() {
        assert!(
            render_status_text(&StatusReport::cold(0.0))
                .contains("No provider-specific logins found.")
        );
    }

    #[test]
    fn json_output_has_the_normalized_status_schema() {
        let value =
            serde_json::to_value(StatusReport::cold(0.0)).expect("status report should serialize");
        assert_eq!(value["schema_version"], STATUS_SCHEMA_VERSION);
        assert_eq!(value["collection"]["state"], "not_collected");
        assert!(
            value["providers"]
                .as_array()
                .expect("providers array")
                .is_empty()
        );
    }

    #[test]
    fn status_renderer_uses_countdowns_for_reset_windows() {
        assert_eq!(format_countdown(90_061.0), "1d 01h 01m");
        assert_eq!(format_countdown(0.0), "resetting");
    }

    #[test]
    fn text_output_includes_codex_reset_credits_and_expiry() {
        let mut provider = ProviderUsage::success(ProviderKey::Codex, Vec::new());
        provider.reset_credits = Some(ResetCredits {
            available_count: 2,
            credits: vec![ResetCredit {
                reset_type: Some("codexRateLimits".to_owned()),
                title: Some("Full reset".to_owned()),
                description: Some("Ready to redeem".to_owned()),
                expires_at: Some(1_900_000_000.0),
            }],
        });
        let report = StatusReport::from_usage(1_800_000_000.0, vec![provider], Vec::new());
        let output = render_status_text(&report);
        assert!(output.contains("Redeem usage limit reset"));
        assert!(output.contains("You have 2 usage limit resets available."));
        assert!(output.contains("Full reset"));
        assert!(output.contains("expires"));
    }
}
