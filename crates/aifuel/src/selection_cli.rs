//! Terminal selection helpers for the shared application selection contract.
//!
//! The picker accepts model entries supplied by a provider adapter/catalog. It
//! never invents model identifiers. When no catalog entry exists, a user may
//! enter an explicit model ID; the returned evidence label keeps that choice
//! visibly unknown to callers.

use aifuel_app::selection::{CatalogModel, CatalogProvenance, SelectionSettings};
use aifuel_core::{CapabilityState, ProviderKey};
use std::fmt;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerModel {
    pub provider: ProviderKey,
    pub model_id: String,
    pub display_label: Option<String>,
    pub provenance: CatalogProvenance,
    pub advertisement: CapabilityState,
    pub entitlement: CapabilityState,
    pub execution: CapabilityState,
    pub scope_label: Option<String>,
    pub catalog_age_seconds: u64,
    pub default_effort: Option<String>,
    pub effort_values: Vec<String>,
    pub effort_state: CapabilityState,
}

impl From<&CatalogModel> for PickerModel {
    fn from(model: &CatalogModel) -> Self {
        Self {
            provider: model.provider,
            model_id: model.model_id.clone(),
            display_label: model.display_label.clone(),
            provenance: model.provenance,
            advertisement: model.advertisement,
            entitlement: model.entitlement,
            execution: model.execution,
            scope_label: None,
            catalog_age_seconds: 0,
            default_effort: model.default_effort.clone(),
            effort_values: model.efforts.values.clone(),
            effort_state: model.efforts.state,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerModelEvidence {
    Catalog,
    ExplicitOverrideUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerOptions {
    pub providers: Vec<ProviderKey>,
    pub models: Vec<PickerModel>,
    pub initial: SelectionSettings,
    pub working_directory: Option<PathBuf>,
}

impl Default for PickerOptions {
    fn default() -> Self {
        Self {
            providers: ProviderKey::ALL.to_vec(),
            models: Vec::new(),
            initial: SelectionSettings::default(),
            working_directory: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerSelection {
    pub settings: SelectionSettings,
    pub working_directory: Option<PathBuf>,
    pub model_evidence: PickerModelEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerOutcome {
    Cancelled,
}

#[derive(Debug)]
pub enum PickerError {
    NotInteractive,
    Io(io::Error),
    InvalidInput(String),
}

impl fmt::Display for PickerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInteractive => f.write_str("selection requires an interactive terminal"),
            Self::Io(error) => write!(f, "selection picker I/O failed: {error}"),
            Self::InvalidInput(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PickerError {}

impl From<io::Error> for PickerError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Run the picker against the process terminal.
pub fn pick_from_terminal(
    options: PickerOptions,
) -> Result<Result<PickerSelection, PickerOutcome>, PickerError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(PickerError::NotInteractive);
    }
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    pick(&mut input, &mut output, options)
}

/// Run the picker with caller-provided streams. This is the seam used by
/// controlled terminal tests and by frontends that own their terminal I/O.
pub fn pick(
    input: &mut impl BufRead,
    output: &mut impl Write,
    options: PickerOptions,
) -> Result<Result<PickerSelection, PickerOutcome>, PickerError> {
    let mut settings = options.initial;
    let providers = if options.providers.is_empty() {
        ProviderKey::ALL.to_vec()
    } else {
        options.providers
    };

    let provider = match settings.provider {
        Some(provider) => provider,
        None => match choose_provider(input, output, &providers)? {
            Some(provider) => provider,
            None => return Ok(Err(PickerOutcome::Cancelled)),
        },
    };
    settings.provider = Some(provider);

    let models = options
        .models
        .into_iter()
        .filter(|model| model.provider == provider)
        .collect::<Vec<_>>();
    let (model, model_evidence, effort_values, effort_state, default_effort) =
        match settings.model.clone() {
            Some(model) => {
                let evidence = models
                    .iter()
                    .find(|candidate| candidate.model_id == model)
                    .map_or(PickerModelEvidence::ExplicitOverrideUnknown, |_| {
                        PickerModelEvidence::Catalog
                    });
                let effort = models
                    .iter()
                    .find(|candidate| candidate.model_id == model)
                    .map(|candidate| {
                        (
                            candidate.effort_values.clone(),
                            candidate.effort_state,
                            candidate.default_effort.clone(),
                        )
                    })
                    .unwrap_or_default();
                (model, evidence, effort.0, effort.1, effort.2)
            }
            None => {
                let Some(selected) = choose_model(input, output, provider, &models)? else {
                    return Ok(Err(PickerOutcome::Cancelled));
                };
                (
                    selected.model_id,
                    selected.evidence,
                    selected.effort_values,
                    selected.effort_state,
                    selected.default_effort,
                )
            }
        };
    settings.model = Some(model);

    if settings.effort.is_none() {
        settings.effort = choose_effort(
            input,
            output,
            &effort_values,
            effort_state,
            default_effort.as_deref(),
        )?;
    }

    let working_directory = match options.working_directory {
        Some(directory) => Some(directory),
        None => match choose_scope(input, output)? {
            ScopeChoice::PromptOnly => None,
            ScopeChoice::Repository(directory) => Some(directory),
            ScopeChoice::Cancelled => return Ok(Err(PickerOutcome::Cancelled)),
        },
    };

    Ok(Ok(PickerSelection {
        settings,
        working_directory,
        model_evidence,
    }))
}

/// Prompt only for a provider, used when resuming a native session whose
/// provider is not otherwise selected. The session remains authoritative for
/// its stored model, effort, and repository scope.
pub fn pick_provider(
    input: &mut impl BufRead,
    output: &mut impl Write,
    providers: &[ProviderKey],
) -> Result<Result<ProviderKey, PickerOutcome>, PickerError> {
    let providers = if providers.is_empty() {
        ProviderKey::ALL.to_vec()
    } else {
        providers.to_vec()
    };
    Ok(match choose_provider(input, output, &providers)? {
        Some(provider) => Ok(provider),
        None => Err(PickerOutcome::Cancelled),
    })
}

struct ChosenModel {
    model_id: String,
    evidence: PickerModelEvidence,
    effort_values: Vec<String>,
    effort_state: CapabilityState,
    default_effort: Option<String>,
}

fn choose_provider(
    input: &mut impl BufRead,
    output: &mut impl Write,
    providers: &[ProviderKey],
) -> Result<Option<ProviderKey>, PickerError> {
    writeln!(output, "Select provider (c to cancel):")?;
    for (index, provider) in providers.iter().enumerate() {
        writeln!(
            output,
            "  {}. {} ({})",
            index + 1,
            provider.display_name(),
            provider
        )?;
    }
    let value = prompt(input, output, "> ")?;
    if is_cancel(&value) {
        return Ok(None);
    }
    let index = parse_index(&value, providers.len(), "provider")?;
    Ok(Some(providers[index]))
}

fn choose_model(
    input: &mut impl BufRead,
    output: &mut impl Write,
    provider: ProviderKey,
    models: &[PickerModel],
) -> Result<Option<ChosenModel>, PickerError> {
    if models.is_empty() {
        writeln!(
            output,
            "No provider-reported models are known for {}. Enter an exact model ID override (c to cancel); support remains unknown.",
            provider.display_name()
        )?;
        let model_id = prompt(input, output, "model ID: ")?;
        if is_cancel(&model_id) {
            return Ok(None);
        }
        if model_id.trim().is_empty() {
            return Err(PickerError::InvalidInput(
                "model ID override cannot be empty".to_owned(),
            ));
        }
        return Ok(Some(ChosenModel {
            model_id,
            evidence: PickerModelEvidence::ExplicitOverrideUnknown,
            effort_values: Vec::new(),
            effort_state: CapabilityState::Unknown,
            default_effort: None,
        }));
    }

    writeln!(
        output,
        "Catalog records keep model advertisement, account entitlement, and execution availability as separate evidence states."
    )?;
    writeln!(output, "Select model (c to cancel):")?;
    for (index, model) in models.iter().enumerate() {
        let label = model.display_label.as_deref().unwrap_or(&model.model_id);
        writeln!(
            output,
            "  {}. {} [{}] | advertisement: {}, entitlement: {}, execution: {}, scope: {} (catalog age: {}s)",
            index + 1,
            label,
            model.model_id,
            capability_state_label(model.advertisement),
            capability_state_label(model.entitlement),
            capability_state_label(model.execution),
            model.scope_label.as_deref().unwrap_or("unknown context"),
            model.catalog_age_seconds
        )?;
    }
    writeln!(
        output,
        "  u. enter an explicit model ID override (support unknown)"
    )?;
    let value = prompt(input, output, "> ")?;
    if is_cancel(&value) {
        return Ok(None);
    }
    if value.eq_ignore_ascii_case("u") {
        let model_id = prompt(input, output, "model ID: ")?;
        if is_cancel(&model_id) || model_id.trim().is_empty() {
            return Ok(None);
        }
        return Ok(Some(ChosenModel {
            model_id,
            evidence: PickerModelEvidence::ExplicitOverrideUnknown,
            effort_values: Vec::new(),
            effort_state: CapabilityState::Unknown,
            default_effort: None,
        }));
    }
    let index = parse_index(&value, models.len(), "model")?;
    let model = &models[index];
    Ok(Some(ChosenModel {
        model_id: model.model_id.clone(),
        evidence: PickerModelEvidence::Catalog,
        effort_values: model.effort_values.clone(),
        effort_state: model.effort_state,
        default_effort: model.default_effort.clone(),
    }))
}

fn capability_state_label(state: CapabilityState) -> &'static str {
    match state {
        CapabilityState::Supported => "supported",
        CapabilityState::Unsupported => "unsupported",
        CapabilityState::Unknown => "unknown",
    }
}

fn choose_effort(
    input: &mut impl BufRead,
    output: &mut impl Write,
    values: &[String],
    state: CapabilityState,
    default_effort: Option<&str>,
) -> Result<Option<String>, PickerError> {
    let default_label = default_effort.map_or_else(
        || "the provider default".to_owned(),
        |effort| format!("the catalog-reported default {effort:?}"),
    );
    if values.is_empty() {
        writeln!(
            output,
            "Effort evidence is {}; press Enter for {default_label} or enter an explicit value (c to cancel).",
            match state {
                CapabilityState::Supported => "known but empty",
                CapabilityState::Unsupported => "unsupported",
                CapabilityState::Unknown => "unknown",
            }
        )?;
        let value = prompt(input, output, "effort: ")?;
        if is_cancel(&value) || value.trim().is_empty() {
            return Ok(None);
        }
        return Ok(Some(value));
    }

    writeln!(
        output,
        "Select effort (Enter for {default_label}, c to cancel):"
    )?;
    for (index, value) in values.iter().enumerate() {
        writeln!(output, "  {}. {}", index + 1, value)?;
    }
    let value = prompt(input, output, "> ")?;
    if is_cancel(&value) {
        return Ok(None);
    }
    if value.trim().is_empty() {
        return Ok(None);
    }
    let index = parse_index(&value, values.len(), "effort")?;
    Ok(Some(values[index].clone()))
}

enum ScopeChoice {
    PromptOnly,
    Repository(PathBuf),
    Cancelled,
}

fn choose_scope(
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<ScopeChoice, PickerError> {
    writeln!(output, "Task scope: [p]rompt-only, [r]epository, [c]ancel")?;
    let value = prompt(input, output, "scope [p]: ")?;
    if is_cancel(&value) {
        return Ok(ScopeChoice::Cancelled);
    }
    if value.trim().is_empty() || value.eq_ignore_ascii_case("p") {
        return Ok(ScopeChoice::PromptOnly);
    }
    if value.eq_ignore_ascii_case("r") {
        let directory = std::env::current_dir()?;
        writeln!(output, "Use repository {}? [y/N/c]", directory.display())?;
        let confirmation = prompt(input, output, "> ")?;
        if is_cancel(&confirmation) {
            return Ok(ScopeChoice::Cancelled);
        }
        if confirmation.eq_ignore_ascii_case("y") || confirmation.eq_ignore_ascii_case("yes") {
            return Ok(ScopeChoice::Repository(directory));
        }
        return Ok(ScopeChoice::PromptOnly);
    }
    Err(PickerError::InvalidInput(
        "scope must be p, r, or c".to_owned(),
    ))
}

fn prompt(
    input: &mut impl BufRead,
    output: &mut impl Write,
    text: &str,
) -> Result<String, PickerError> {
    write!(output, "{text}")?;
    output.flush()?;
    let mut value = String::new();
    if input.read_line(&mut value)? == 0 {
        return Err(PickerError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "terminal input ended",
        )));
    }
    Ok(value.trim().to_owned())
}

fn parse_index(value: &str, length: usize, kind: &str) -> Result<usize, PickerError> {
    let index = value
        .parse::<usize>()
        .ok()
        .filter(|index| (1..=length).contains(index))
        .map(|index| index - 1)
        .ok_or_else(|| PickerError::InvalidInput(format!("invalid {kind} selection {value:?}")))?;
    Ok(index)
}

fn is_cancel(value: &str) -> bool {
    value.eq_ignore_ascii_case("c") || value.eq_ignore_ascii_case("cancel")
}

#[cfg(test)]
#[path = "selection_cli_tests.rs"]
mod tests;
