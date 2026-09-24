use crate as aifuel;
use aifuel_app::selection::{
    GlobalSelectionConfig, SelectionInputs, SelectionSettings, SelectionSources,
};
use std::io::{BufRead, Write};
use std::path::PathBuf;

#[derive(Debug)]
pub(super) struct CliRunSelection {
    pub(super) settings: SelectionSettings,
    pub(super) working_directory: Option<PathBuf>,
    pub(super) model_evidence: Option<aifuel::selection_cli::PickerModelEvidence>,
    pub(super) sources: SelectionSources,
}

pub(super) struct CliSelectionRequest<'a> {
    pub(super) config: &'a GlobalSelectionConfig,
    pub(super) explicit: SelectionSettings,
    pub(super) profile: Option<&'a str>,
    pub(super) working_directory: Option<PathBuf>,
    pub(super) catalog_models: Option<Vec<aifuel::selection_cli::PickerModel>>,
    pub(super) interactive: bool,
    pub(super) resume: bool,
}

pub(super) fn resolve_cli_selection(
    request: CliSelectionRequest<'_>,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<CliRunSelection, String> {
    let CliSelectionRequest {
        config,
        explicit,
        profile,
        working_directory,
        catalog_models,
        interactive,
        resume,
    } = request;
    let profile_settings = profile
        .map(|name| {
            config
                .profiles
                .get(name)
                .ok_or_else(|| format!("selection profile {name:?} was not found"))
        })
        .transpose()?;
    let explicit_model = explicit.model.clone();
    let inherited = profile_settings
        .cloned()
        .unwrap_or_default()
        .merge_over(&config.defaults);
    let mut initial = explicit.merge_over(&inherited);
    if resume {
        initial.provider = explicit
            .provider
            .or_else(|| profile_settings.and_then(|settings| settings.provider));
        initial.model = explicit
            .model
            .clone()
            .or_else(|| profile_settings.and_then(|settings| settings.model.clone()));
        initial.effort = explicit
            .effort
            .clone()
            .or_else(|| profile_settings.and_then(|settings| settings.effort.clone()));
    }

    if !interactive && initial.provider.is_none() {
        let guidance = if resume {
            "resume requires --provider or a provider in the selected profile; global provider defaults are ignored"
        } else {
            "run requires --provider, a provider in the selected profile, or a global provider default; connect stdin and stderr to a terminal to choose one"
        };
        return Err(guidance.to_owned());
    }
    if !interactive && !resume && initial.model.is_none() {
        return Err("run requires --model, a model in the selected profile or global defaults, or a terminal to choose a model".to_owned());
    }

    if resume {
        if initial.provider.is_none() {
            let providers = aifuel_providers::agent_run_adapters()
                .iter()
                .map(|adapter| adapter.provider())
                .collect::<Vec<_>>();
            initial.provider = Some(
                aifuel::selection_cli::pick_provider(input, output, &providers)
                    .map_err(|error| error.to_string())?
                    .map_err(|_| "run selection cancelled".to_owned())?,
            );
        }
        let model_evidence = explicit_model.as_deref().map(|model| {
            crate::run_selection::model_evidence_for_model(
                initial.provider,
                model,
                catalog_models.as_deref(),
            )
        });
        let sources = resolve_cli_sources(config, &explicit, profile, &initial, true)?;
        return Ok(CliRunSelection {
            settings: initial,
            working_directory,
            model_evidence,
            sources,
        });
    }

    let needs_picker = initial.provider.is_none()
        || initial.model.is_none()
        || initial.effort.is_none()
        || working_directory.is_none();
    if !interactive || !needs_picker {
        let model_evidence = explicit_model.as_deref().map(|model| {
            crate::run_selection::model_evidence_for_model(
                initial.provider,
                model,
                catalog_models.as_deref(),
            )
        });
        let sources = resolve_cli_sources(config, &explicit, profile, &initial, false)?;
        return Ok(CliRunSelection {
            settings: initial,
            working_directory,
            model_evidence,
            sources,
        });
    }

    let inherited_model = initial.model.clone();
    let models = match catalog_models {
        Some(models) => models,
        None => crate::run_selection::load_picker_models()?,
    };
    let providers = aifuel_providers::agent_run_adapters()
        .iter()
        .map(|adapter| adapter.provider())
        .collect();
    let picked = aifuel::selection_cli::pick(
        input,
        output,
        aifuel::selection_cli::PickerOptions {
            providers,
            models,
            initial,
            working_directory,
        },
    )
    .map_err(|error| error.to_string())?
    .map_err(|_| "run selection cancelled".to_owned())?;
    let model_evidence = match picked.model_evidence {
        aifuel::selection_cli::PickerModelEvidence::Catalog => {
            Some(aifuel::selection_cli::PickerModelEvidence::Catalog)
        }
        aifuel::selection_cli::PickerModelEvidence::ExplicitOverrideUnknown
            if explicit_model.is_some() || inherited_model.is_none() =>
        {
            Some(aifuel::selection_cli::PickerModelEvidence::ExplicitOverrideUnknown)
        }
        aifuel::selection_cli::PickerModelEvidence::ExplicitOverrideUnknown => None,
    };
    let sources = resolve_cli_sources(config, &explicit, profile, &picked.settings, false)?;
    Ok(CliRunSelection {
        settings: picked.settings,
        working_directory: picked.working_directory,
        model_evidence,
        sources,
    })
}

fn resolve_cli_sources(
    config: &GlobalSelectionConfig,
    explicit: &SelectionSettings,
    profile: Option<&str>,
    selected: &SelectionSettings,
    resume: bool,
) -> Result<SelectionSources, String> {
    let profile_settings = profile
        .map(|name| {
            config
                .profiles
                .get(name)
                .ok_or_else(|| format!("selection profile {name:?} was not found"))
        })
        .transpose()?;
    let mut resolver_config = config.clone();
    if resume {
        resolver_config.defaults.provider = None;
        resolver_config.defaults.model = None;
        resolver_config.defaults.effort = None;
    }
    let mut explicit = explicit.clone();
    if explicit.provider.is_none()
        && profile_settings
            .and_then(|settings| settings.provider)
            .is_none()
        && resolver_config.defaults.provider.is_none()
    {
        explicit.provider = selected.provider;
    }
    if explicit.model.is_none()
        && profile_settings
            .and_then(|settings| settings.model.as_ref())
            .is_none()
        && resolver_config.defaults.model.is_none()
    {
        explicit.model = selected.model.clone();
    }
    if explicit.effort.is_none()
        && profile_settings
            .and_then(|settings| settings.effort.as_ref())
            .is_none()
        && resolver_config.defaults.effort.is_none()
    {
        explicit.effort = selected.effort.clone();
    }
    let resolved = resolver_config
        .resolve(
            &SelectionInputs {
                explicit,
                profile: profile.map(str::to_owned),
                interactive: false,
                deadline_override: None,
            },
            None,
        )
        .map_err(|error| format!("could not resolve selection sources: {error}"))?
        .sources;
    Ok(resolved)
}
