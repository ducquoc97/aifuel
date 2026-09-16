use aifuel_core::StatusReport;
use aifuel_providers::{CollectionConfig, DiscoveryContext, UsageService};
use std::env;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

mod dashboard;
mod launcher;
mod mcp;

fn main() -> ExitCode {
    match run(env::args().skip(1)) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("aifuel: {error}");
            ExitCode::from(2)
        }
    }
}

fn run<I>(args: I) -> Result<u8, String>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();
    if args.first().map(String::as_str) == Some("run") {
        return run_launcher(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("mcp") {
        if args.len() > 1 {
            return Err(format!("unknown argument {:?} after mcp", args[1]));
        }
        mcp::serve()?;
        return Ok(0);
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

    if !json && !text {
        dashboard::serve(&host, port, !no_browser)?;
        return Ok(0);
    }

    let context = DiscoveryContext::from_environment().map_err(|error| error.to_string())?;
    let service = UsageService::new(context.home_dir(), CollectionConfig::from_environment())?;
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| format!("could not start collection runtime: {error}"))?;
    let report = runtime.block_on(service.collect());

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

fn print_help() {
    println!("aifuel - monitor and explicitly launch AI coding providers");
    println!();
    println!("Usage: aifuel [--text | --json]");
    println!("       aifuel run --provider PROVIDER_ID [OPTIONS]");
    println!("       aifuel mcp");
    println!();
    println!("The default command discovers provider-owned local source metadata.");
    println!("run delegates one explicit prompt to a verified provider CLI.");
    println!("mcp serves read-only status over standard input and output.");
}

fn run_launcher(args: &[String]) -> Result<u8, String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_run_help();
        return Ok(0);
    }
    let request = parse_run_args(args)?;
    let output_format = request.output;
    let result = match launcher::execute(&request) {
        Ok(result) => result,
        Err(launcher::LaunchError::UnsupportedProvider(provider)) => {
            eprintln!("aifuel: provider {provider} has no verified agent integration");
            return Ok(3);
        }
        Err(error) => {
            eprintln!("aifuel: {error}");
            return Ok(2);
        }
    };

    match output_format {
        launcher::OutputFormat::Text => {
            print!("{}", result.output);
            if let Some(error) = &result.error {
                eprint!("{error}");
            }
        }
        launcher::OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&result)
                    .map_err(|error| format!("could not encode run result: {error}"))?
            );
        }
        launcher::OutputFormat::Jsonl => {
            println!(
                "{}",
                serde_json::json!({"type": "result", "result": result})
            );
        }
    }

    if result.timed_out {
        Ok(5)
    } else if result.status == "succeeded" {
        Ok(0)
    } else {
        Ok(4)
    }
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
            if provider.status == "ok" {
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
    chrono::DateTime::from_timestamp(timestamp as i64, 0)
        .map(|value| {
            value
                .with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "unknown time".to_owned())
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

fn parse_run_args(args: &[String]) -> Result<launcher::RunRequest, String> {
    let mut provider = None;
    let mut model = None;
    let mut account = None;
    let mut prompt = None;
    let mut prompt_file = None;
    let mut prompt_count = 0;
    let mut prompt_file_count = 0;
    let mut output = launcher::OutputFormat::Text;
    let mut working_directory: Option<PathBuf> = None;
    let mut access = launcher::AccessMode::ReadOnly;
    let mut resume = None;
    let mut timeout = Some(Duration::from_secs(600));

    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        match argument {
            "--help" | "-h" => {
                print_run_help();
                return Err(String::new());
            }
            "--provider" => {
                provider = Some(next_value(args, &mut index, argument)?);
            }
            "--model" => model = Some(next_value(args, &mut index, argument)?),
            "--account" => account = Some(next_value(args, &mut index, argument)?),
            "--prompt" => {
                prompt_count += 1;
                prompt = Some(next_value(args, &mut index, argument)?)
            }
            "--prompt-file" => {
                prompt_file_count += 1;
                prompt_file = Some(PathBuf::from(next_value(args, &mut index, argument)?))
            }
            "--output" => {
                output = launcher::OutputFormat::parse(&next_value(args, &mut index, argument)?)?;
            }
            "--working-directory" | "--cwd" => {
                working_directory = Some(PathBuf::from(next_value(args, &mut index, argument)?));
            }
            "--access" => {
                access = launcher::AccessMode::parse(&next_value(args, &mut index, argument)?)?
            }
            "--resume" => resume = Some(next_value(args, &mut index, argument)?),
            "--timeout" => timeout = parse_timeout(&next_value(args, &mut index, argument)?)?,
            unknown => return Err(format!("unknown argument {unknown:?} for run")),
        }
        index += 1;
    }

    let provider = provider.ok_or_else(|| "run requires --provider PROVIDER_ID".to_owned())?;
    let provider = provider
        .parse()
        .map_err(|error: aifuel_core::InvalidProviderKey| error.to_string())?;
    if prompt_count > 1 || prompt_file_count > 1 {
        return Err("provide exactly one value for the selected prompt option".to_owned());
    }
    if prompt.is_some() && prompt_file.is_some() {
        return Err("use exactly one of --prompt or --prompt-file".to_owned());
    }
    let prompt = match (prompt, prompt_file) {
        (Some(prompt), None) => prompt,
        (None, Some(path)) => {
            launcher::read_prompt_file(&path).map_err(|error| error.to_string())?
        }
        (None, None) if std::io::stdin().is_terminal() => {
            return Err("run requires --prompt, --prompt-file, or piped stdin".to_owned());
        }
        (None, None) => launcher::read_stdin_prompt().map_err(|error| error.to_string())?,
        (Some(_), Some(_)) => unreachable!("prompt sources are checked above"),
    };

    Ok(launcher::RunRequest {
        provider,
        model,
        account,
        prompt,
        output,
        working_directory,
        access,
        resume,
        timeout,
    })
}

fn next_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_timeout(value: &str) -> Result<Option<Duration>, String> {
    if value == "0" {
        return Ok(None);
    }
    let (number, multiplier) = value
        .strip_suffix('s')
        .map(|value| (value, 1))
        .or_else(|| value.strip_suffix('m').map(|value| (value, 60)))
        .or_else(|| value.strip_suffix('h').map(|value| (value, 60 * 60)))
        .ok_or_else(|| {
            "timeout must use seconds, minutes, or hours (for example 30s)".to_owned()
        })?;
    let number: u64 = number
        .parse()
        .map_err(|_| format!("invalid timeout {value:?}"))?;
    Ok(Some(Duration::from_secs(number.saturating_mul(multiplier))))
}

fn print_run_help() {
    println!("Usage: aifuel run --provider PROVIDER_ID [OPTIONS]");
    println!();
    println!("  --prompt TEXT                         prompt text");
    println!("  --prompt-file PATH                    read prompt from a file");
    println!("  --model MODEL_ID                      explicit model");
    println!("  --account ACCOUNT_ID                  explicit account");
    println!("  --output text|json|jsonl              result format (default: text)");
    println!("  --working-directory PATH              optional project directory");
    println!("  --access read-only|workspace-write    permission profile");
    println!("  --resume SESSION_ID                   explicit session continuation");
    println!("  --timeout DURATION                    default: 10m");
}

#[cfg(test)]
mod tests {
    use super::*;
    use aifuel_core::STATUS_SCHEMA_VERSION;

    #[test]
    fn text_output_has_an_intentional_empty_state() {
        let output = render_status_text(&StatusReport::cold(0.0));

        assert!(output.contains("No provider-specific logins found."));
    }

    #[test]
    fn json_output_has_the_normalized_status_schema() {
        let value = serde_json::to_value(StatusReport::cold(0.0))
            .expect("status report should be serializable");

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
}
