use crate as aifuel;
use crate::launcher;
mod selection;

use aifuel_app::selection::{
    GlobalSelectionConfig, SelectionInputs, SelectionSettings, SelectionSources,
};
use aifuel_core::{ManagedRunResult, RunState, RunStatus};
use selection::{CliSelectionRequest, resolve_cli_selection};
use std::collections::HashSet;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

pub(crate) fn next_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

pub fn run(args: &[String]) -> Result<u8, String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_run_help();
        return Ok(0);
    }
    let selected = parse_run_args(args)?;
    let request = selected.request;
    if let (Some(evidence), Some(model)) = (selected.model_evidence, request.model.as_deref()) {
        let message = match evidence {
            aifuel::selection_cli::PickerModelEvidence::Catalog => format!(
                "aifuel: model {model:?} has a cached catalog record; advertisement, account entitlement, and execution availability remain separate evidence"
            ),
            aifuel::selection_cli::PickerModelEvidence::ExplicitOverrideUnknown => format!(
                "aifuel: model {model:?} is an explicit override without catalog evidence; account entitlement and execution availability remain unknown"
            ),
        };
        eprintln!("{message}");
    }
    let output_format = request.output;
    let result = match launcher::execute(&request) {
        Ok(result) => result,
        Err(launcher::LaunchError::UnsupportedProvider(provider)) => {
            eprintln!("aifuel: provider {provider} has no verified agent integration");
            return Ok(3);
        }
        Err(launcher::LaunchError::Timeout(error)) => {
            eprintln!("aifuel: {error}");
            return Ok(5);
        }
        Err(error) => {
            eprintln!("aifuel: {error}");
            return Ok(2);
        }
    };

    print!(
        "{}",
        render_run_result_with_sources(
            &result,
            output_format,
            selected.model_evidence,
            Some(&selected.selection_sources),
            (
                request.resume.is_some() && request.model.is_none(),
                request.resume.is_some() && request.effort.is_none(),
            ),
        )?
    );

    if result.state == RunState::TimedOut || result.status == Some(RunStatus::Timeout) {
        Ok(5)
    } else if result.status == Some(RunStatus::Succeeded) {
        Ok(0)
    } else {
        Ok(4)
    }
}

#[cfg(test)]
fn render_run_result(
    result: &ManagedRunResult,
    output_format: launcher::OutputFormat,
    model_evidence: Option<aifuel::selection_cli::PickerModelEvidence>,
) -> Result<String, String> {
    render_run_result_with_sources(result, output_format, model_evidence, None, (false, false))
}

fn render_run_result_with_sources(
    result: &ManagedRunResult,
    output_format: launcher::OutputFormat,
    model_evidence: Option<aifuel::selection_cli::PickerModelEvidence>,
    selection_sources: Option<&SelectionSources>,
    deferred_sources: (bool, bool),
) -> Result<String, String> {
    match output_format {
        launcher::OutputFormat::Text => {
            if let Some(diagnostics) = &result.diagnostics {
                eprint!("{diagnostics}");
            } else if let Some(error) = &result.error {
                eprint!("{error}");
            }
            Ok(String::new())
        }
        launcher::OutputFormat::Json => {
            let mut value = serde_json::to_value(result)
                .map_err(|error| format!("could not encode run result: {error}"))?;
            if let Some(evidence) = model_evidence {
                value["model_evidence"] =
                    serde_json::Value::String(model_evidence_name(evidence).to_owned());
            }
            if let Some(sources) = selection_sources {
                value["selection_sources"] = serde_json::to_value(sources)
                    .map_err(|error| format!("could not encode selection sources: {error}"))?;
                omit_deferred_sources(&mut value["selection_sources"], deferred_sources);
            }
            let encoded = serde_json::to_string_pretty(&value)
                .map_err(|error| format!("could not encode run result: {error}"))?;
            Ok(format!("{encoded}\n"))
        }
        launcher::OutputFormat::Jsonl => {
            let mut result_event = serde_json::json!({"type": "run_result", "result": result});
            if let Some(evidence) = model_evidence {
                result_event["model_evidence"] =
                    serde_json::Value::String(model_evidence_name(evidence).to_owned());
            }
            if let Some(sources) = selection_sources {
                result_event["selection_sources"] = serde_json::to_value(sources)
                    .map_err(|error| format!("could not encode selection sources: {error}"))?;
                omit_deferred_sources(&mut result_event["selection_sources"], deferred_sources);
            }
            let encoded = serde_json::to_string(&result_event)
                .map_err(|error| format!("could not encode run result: {error}"))?;
            Ok(format!("{encoded}\n"))
        }
    }
}

fn omit_deferred_sources(sources: &mut serde_json::Value, deferred: (bool, bool)) {
    if let Some(sources) = sources.as_object_mut() {
        if deferred.0 {
            sources.remove("model");
        }
        if deferred.1 {
            sources.remove("effort");
        }
    }
}

fn model_evidence_name(evidence: aifuel::selection_cli::PickerModelEvidence) -> &'static str {
    match evidence {
        aifuel::selection_cli::PickerModelEvidence::Catalog => "catalog_record",
        aifuel::selection_cli::PickerModelEvidence::ExplicitOverrideUnknown => {
            "explicit_override_unknown"
        }
    }
}

struct ParsedRunRequest {
    request: launcher::RunRequest,
    model_evidence: Option<aifuel::selection_cli::PickerModelEvidence>,
    selection_sources: SelectionSources,
}

fn parse_run_args(args: &[String]) -> Result<ParsedRunRequest, String> {
    let config = aifuel_app::selection::SelectionStore::load(aifuel::execution_config_path()?)
        .map_err(|error| error.to_string())?;
    let stdin = io::stdin();
    let stderr = io::stderr();
    let stdin_is_terminal = stdin.is_terminal();
    let interactive = stdin_is_terminal && stderr.is_terminal();
    let mut input = stdin.lock();
    let mut output = stderr.lock();
    parse_run_args_with_context(
        args,
        &config,
        None,
        &mut input,
        &mut output,
        interactive,
        stdin_is_terminal,
    )
}

fn parse_run_args_with_context(
    args: &[String],
    config: &GlobalSelectionConfig,
    catalog_models: Option<Vec<aifuel::selection_cli::PickerModel>>,
    picker_input: &mut impl BufRead,
    picker_output: &mut impl Write,
    interactive: bool,
    stdin_is_terminal: bool,
) -> Result<ParsedRunRequest, String> {
    let mut provider = None;
    let mut model = None;
    let mut effort = None;
    let mut external_tools = Vec::new();
    let mut account = None;
    let mut prompt = None;
    let mut prompt_file = None;
    let mut prompt_count = 0;
    let mut prompt_file_count = 0;
    let mut output_format = launcher::OutputFormat::Text;
    let mut working_directory: Option<PathBuf> = None;
    let mut access = launcher::AccessMode::ReadOnly;
    let mut resume = None;
    let mut timeout = None;
    let mut profile = None;
    let mut access_explicit = false;
    let mut timeout_explicit = false;

    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        match argument {
            "--help" | "-h" => {
                print_run_help();
                return Err(String::new());
            }
            "--provider" => provider = Some(next_value(args, &mut index, argument)?),
            "--model" => model = Some(next_value(args, &mut index, argument)?),
            "--effort" => effort = Some(next_value(args, &mut index, argument)?),
            "--external-tool" => external_tools.push(next_value(args, &mut index, argument)?),
            "--account" => account = Some(next_value(args, &mut index, argument)?),
            "--profile" => profile = Some(next_value(args, &mut index, argument)?),
            "--prompt" => {
                prompt_count += 1;
                prompt = Some(next_value(args, &mut index, argument)?);
            }
            "--prompt-file" => {
                prompt_file_count += 1;
                prompt_file = Some(PathBuf::from(next_value(args, &mut index, argument)?));
            }
            "--output" => {
                output_format =
                    launcher::OutputFormat::parse(&next_value(args, &mut index, argument)?)?;
            }
            "--working-directory" | "--cwd" => {
                working_directory = Some(PathBuf::from(next_value(args, &mut index, argument)?));
            }
            "--access" => {
                access = launcher::AccessMode::parse(&next_value(args, &mut index, argument)?)?;
                access_explicit = true;
            }
            "--resume" => resume = Some(next_value(args, &mut index, argument)?),
            "--timeout" => {
                timeout = parse_timeout(&next_value(args, &mut index, argument)?)?;
                timeout_explicit = true;
            }
            unknown => return Err(format!("unknown argument {unknown:?} for run")),
        }
        index += 1;
    }

    if external_tools.iter().any(|tool| tool.trim().is_empty()) {
        return Err("--external-tool requires a non-empty Gateway tool name".to_owned());
    }
    let mut unique_tools = HashSet::new();
    if external_tools
        .iter()
        .any(|tool| !unique_tools.insert(tool.as_str()))
    {
        return Err("--external-tool names must not be repeated".to_owned());
    }
    let external_tools = (!external_tools.is_empty()).then_some(external_tools);

    let provider = provider
        .map(|provider| provider.parse())
        .transpose()
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
        (None, None) if stdin_is_terminal => {
            return Err("run requires --prompt, --prompt-file, or piped stdin".to_owned());
        }
        (None, None) => {
            let mut prompt = String::new();
            picker_input
                .read_to_string(&mut prompt)
                .map_err(|error| format!("could not read prompt from stdin: {error}"))?;
            prompt
        }
        (Some(_), Some(_)) => unreachable!("prompt sources are checked above"),
    };

    let explicit_selection = SelectionSettings {
        provider,
        model,
        effort,
        access: access_explicit.then_some(access),
        overall_deadline_seconds: None,
    };
    let selection = resolve_cli_selection(
        CliSelectionRequest {
            config,
            explicit: explicit_selection,
            profile: profile.as_deref(),
            working_directory,
            catalog_models,
            interactive,
            resume: resume.is_some(),
        },
        picker_input,
        picker_output,
    )?;
    let model_evidence = selection.model_evidence;
    let mut request = launcher::RunRequest {
        provider: selection
            .settings
            .provider
            .ok_or_else(|| "run requires a provider selection".to_owned())?,
        model: selection.settings.model.clone(),
        effort: selection.settings.effort.clone(),
        external_tools,
        account,
        prompt,
        output: output_format,
        working_directory: selection.working_directory,
        access: selection
            .settings
            .access
            .unwrap_or(launcher::AccessMode::ReadOnly),
        resume,
        timeout: None,
        interaction_handler: None,
    };
    let resolved = config
        .resolve(
            &SelectionInputs {
                explicit: SelectionSettings {
                    provider: selection.settings.provider,
                    model: selection.settings.model.clone(),
                    effort: selection.settings.effort.clone(),
                    access: access_explicit.then_some(access),
                    overall_deadline_seconds: None,
                },
                profile: profile.clone(),
                interactive: false,
                deadline_override: timeout_explicit.then_some(timeout.map(|value| value.as_secs())),
            },
            None,
        )
        .map_err(|error| format!("could not resolve run selection: {error}"))?;
    request.provider = resolved.provider;
    if request.resume.is_none() {
        request.model = resolved.model;
        request.effort = resolved.effort;
    }
    request.access = resolved.access;
    request.timeout = resolved.overall_deadline;
    let mut selection_sources = selection.sources;
    selection_sources.access = resolved.sources.access;
    selection_sources.overall_deadline = resolved.sources.overall_deadline;

    Ok(ParsedRunRequest {
        request,
        model_evidence,
        selection_sources,
    })
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
    println!("  --effort LEVEL                        requested model effort");
    println!("  --external-tool TOOL_NAME             exact Gateway tool (repeatable)");
    println!("  --profile NAME                        named global selection profile");
    println!("  --account ACCOUNT_ID                  explicit account");
    println!("  --output text|json|jsonl              result format (default: text)");
    println!("  --working-directory PATH              optional project directory");
    println!("  --access read-only|workspace-write    permission profile");
    println!(
        "  --resume SESSION_ID                   continue a known session; explicit model/effort override stored values"
    );
    println!("  --timeout DURATION                    optional deadline; no deadline by default");
}

#[cfg(test)]
#[path = "run_cli_tests.rs"]
mod tests;
