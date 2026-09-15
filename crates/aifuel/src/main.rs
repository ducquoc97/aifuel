use aifuel_core::{DISCOVERY_SCHEMA_VERSION, DiscoveryReport, ProviderDescriptor};
use aifuel_providers::{DiscoveryContext, default_registry};
use serde::Serialize;
use std::env;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
struct JsonOutput<'a> {
    schema_version: u32,
    providers: &'a [ProviderDescriptor],
    discovery_errors: &'a [aifuel_core::DiscoveryFailure],
}

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
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "--text" => json = false,
            "--help" | "-h" => {
                print_help();
                return Ok(0);
            }
            unknown => return Err(format!("unknown argument {unknown:?}; use --help")),
        }
    }

    let context = DiscoveryContext::from_environment().map_err(|error| error.to_string())?;
    let selection = default_registry().discover_and_initialize(&context);
    let report = selection.report();

    if json {
        println!(
            "{}",
            render_json(report).map_err(|error| format!("could not encode JSON: {error}"))?
        );
    } else {
        print!("{}", render_text(report));
    }
    print_diagnostics(report);

    Ok(if report.discovery_errors.is_empty() {
        0
    } else {
        1
    })
}

fn render_json(report: &DiscoveryReport) -> Result<String, serde_json::Error> {
    let output = JsonOutput {
        schema_version: DISCOVERY_SCHEMA_VERSION,
        providers: &report.providers,
        discovery_errors: &report.discovery_errors,
    };
    serde_json::to_string_pretty(&output)
}

fn render_text(report: &DiscoveryReport) -> String {
    let mut output = String::from("aifuel\n\n");

    if report.providers.is_empty() {
        output.push_str("No provider-specific logins found.\n");
    } else {
        output.push_str("Discovered providers:\n");
        for provider in &report.providers {
            output.push_str(&format!("- {} ({})\n", provider.name, provider.key));
        }
    }

    if !report.discovery_errors.is_empty() {
        output.push_str("\nProvider Discovery failures:\n");
        for failure in &report.discovery_errors {
            output.push_str(&format!(
                "- {}: {}\n",
                failure.provider.name, failure.detail
            ));
        }
    }

    output
}

fn print_diagnostics(report: &DiscoveryReport) {
    for failure in &report.discovery_errors {
        eprintln!(
            "Provider discovery failed for {}: {}",
            failure.provider.name, failure.detail
        );
    }
}

fn print_help() {
    println!("aifuel - discover locally configured AI coding providers");
    println!();
    println!("Usage: aifuel [--text | --json]");
    println!();
    println!("Discovery inspects provider-owned local source metadata only.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use aifuel_core::{DiscoveryFailure, ProviderKey};

    #[test]
    fn text_output_has_an_intentional_empty_state() {
        let output = render_text(&DiscoveryReport::new());

        assert!(output.contains("No provider-specific logins found."));
    }

    #[test]
    fn json_output_keeps_discovery_errors_separate_from_providers() {
        let mut report = DiscoveryReport::new();
        report
            .providers
            .push(ProviderDescriptor::for_key(ProviderKey::Codex));
        report.discovery_errors.push(DiscoveryFailure::from_error(
            ProviderDescriptor::for_key(ProviderKey::Gemini),
            aifuel_core::DiscoveryError::SourceUnavailable,
        ));

        let value: serde_json::Value = serde_json::from_str(&render_json(&report).unwrap())
            .expect("rendered discovery output should be JSON");

        assert_eq!(value["schema_version"], DISCOVERY_SCHEMA_VERSION);
        assert_eq!(value["providers"][0]["key"], "codex");
        assert_eq!(value["discovery_errors"][0]["provider"]["key"], "gemini");
    }
}
