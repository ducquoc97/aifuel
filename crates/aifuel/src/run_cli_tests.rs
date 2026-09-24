use super::*;
use crate as aifuel;
use aifuel_app::selection::SelectionSource;
use aifuel_core::{ProviderKey, RunState, RunStatus};

#[test]
fn run_has_no_default_overall_deadline() {
    let args = [
        "--provider".to_owned(),
        "gemini".to_owned(),
        "--model".to_owned(),
        "test-model".to_owned(),
        "--prompt".to_owned(),
        "hello".to_owned(),
    ];
    let config = GlobalSelectionConfig::default();
    let mut input = io::Cursor::new(Vec::new());
    let mut output = Vec::new();
    let request = parse_run_args_with_context(
        &args,
        &config,
        Some(Vec::new()),
        &mut input,
        &mut output,
        false,
        false,
    )
    .expect("run arguments should parse");

    assert_eq!(request.request.timeout, None);
}

#[test]
fn cli_records_selection_sources_using_shared_precedence() {
    let mut config = GlobalSelectionConfig {
        defaults: SelectionSettings {
            provider: Some(ProviderKey::Gemini),
            model: Some("global-model".to_owned()),
            effort: Some("low".to_owned()),
            access: Some(aifuel_core::AccessMode::ReadOnly),
            overall_deadline_seconds: Some(90),
        },
        ..GlobalSelectionConfig::default()
    };
    config.profiles.insert(
        "work".to_owned(),
        SelectionSettings {
            provider: Some(ProviderKey::Codex),
            model: Some("profile-model".to_owned()),
            effort: Some("high".to_owned()),
            access: Some(aifuel_core::AccessMode::WorkspaceWrite),
            overall_deadline_seconds: None,
        },
    );
    let args = [
        "--profile",
        "work",
        "--provider",
        "codex",
        "--model",
        "explicit-model",
        "--prompt",
        "hello",
        "--output",
        "json",
    ]
    .map(str::to_owned);
    let request = parse_run_args_with_context(
        &args,
        &config,
        Some(Vec::new()),
        &mut io::Cursor::new(Vec::new()),
        &mut Vec::new(),
        false,
        false,
    )
    .expect("run request should resolve");

    assert_eq!(
        request.selection_sources.provider,
        SelectionSource::Explicit
    );
    assert_eq!(request.selection_sources.model, SelectionSource::Explicit);
    assert_eq!(
        request.selection_sources.effort,
        SelectionSource::Profile("work".to_owned())
    );
    assert_eq!(
        request.selection_sources.access,
        SelectionSource::Profile("work".to_owned())
    );
    assert_eq!(
        request.selection_sources.overall_deadline,
        SelectionSource::GlobalDefault
    );
}

#[test]
fn terminal_picker_preserves_explicit_profile_and_global_precedence() {
    let mut config = GlobalSelectionConfig {
        defaults: SelectionSettings {
            provider: Some(ProviderKey::Claude),
            model: Some("global-model".to_owned()),
            effort: Some("low".to_owned()),
            access: Some(aifuel_core::AccessMode::ReadOnly),
            overall_deadline_seconds: None,
        },
        ..GlobalSelectionConfig::default()
    };
    config.profiles.insert(
        "review".to_owned(),
        SelectionSettings {
            provider: Some(ProviderKey::Codex),
            model: Some("profile-model".to_owned()),
            effort: Some("medium".to_owned()),
            access: Some(aifuel_core::AccessMode::WorkspaceWrite),
            overall_deadline_seconds: None,
        },
    );
    let explicit = SelectionSettings {
        provider: Some(ProviderKey::Gemini),
        model: Some("explicit-model".to_owned()),
        effort: None,
        access: Some(aifuel_core::AccessMode::ReadOnly),
        overall_deadline_seconds: None,
    };
    let mut input = io::Cursor::new(b"p\n".to_vec());
    let mut output = Vec::new();

    let selected = resolve_cli_selection(
        CliSelectionRequest {
            config: &config,
            explicit,
            profile: Some("review"),
            working_directory: None,
            catalog_models: Some(Vec::new()),
            interactive: true,
            resume: false,
        },
        &mut input,
        &mut output,
    )
    .expect("picker should use resolved explicit/profile values");

    assert_eq!(selected.settings.provider, Some(ProviderKey::Gemini));
    assert_eq!(selected.settings.model.as_deref(), Some("explicit-model"));
    assert_eq!(selected.settings.effort.as_deref(), Some("medium"));
    assert_eq!(
        selected.settings.access,
        Some(aifuel_core::AccessMode::ReadOnly)
    );
    let output = String::from_utf8(output).expect("picker output should be UTF-8");
    assert!(output.contains("Task scope:"));
    assert!(!output.contains("Select provider"));
    assert!(!output.contains("Select model"));
    assert!(!output.contains("Select effort"));
}

#[test]
fn terminal_picker_selects_a_provider_reported_model_without_inventing_values() {
    let config = GlobalSelectionConfig::default();
    let providers = aifuel_providers::agent_run_adapters()
        .iter()
        .map(|adapter| adapter.provider())
        .collect::<Vec<_>>();
    let provider_index = providers
        .iter()
        .position(|provider| *provider == ProviderKey::Codex)
        .expect("Codex Agent Integration should be registered")
        + 1;
    let input_text = format!("{provider_index}\n1\n\np\n");
    let mut input = io::Cursor::new(input_text.into_bytes());
    let mut output = Vec::new();
    let models = vec![aifuel::selection_cli::PickerModel {
        provider: ProviderKey::Codex,
        model_id: "exact-catalog-model".to_owned(),
        display_label: Some("Observed model".to_owned()),
        provenance: aifuel_app::selection::CatalogProvenance::ProviderApi,
        advertisement: aifuel_core::CapabilityState::Supported,
        entitlement: aifuel_core::CapabilityState::Unknown,
        execution: aifuel_core::CapabilityState::Unknown,
        scope_label: None,
        catalog_age_seconds: 0,
        default_effort: None,
        effort_values: vec!["high".to_owned()],
        effort_state: aifuel_core::CapabilityState::Supported,
    }];

    let selected = resolve_cli_selection(
        CliSelectionRequest {
            config: &config,
            explicit: SelectionSettings::default(),
            profile: None,
            working_directory: None,
            catalog_models: Some(models),
            interactive: true,
            resume: false,
        },
        &mut input,
        &mut output,
    )
    .expect("the picker should return a terminal selection");

    assert_eq!(selected.settings.provider, Some(ProviderKey::Codex));
    assert_eq!(
        selected.settings.model.as_deref(),
        Some("exact-catalog-model")
    );
    assert_eq!(selected.settings.effort, None);
    assert_eq!(selected.working_directory, None);
}

#[test]
fn noninteractive_missing_provider_error_explains_the_terminal_option() {
    let config = GlobalSelectionConfig::default();
    let mut input = io::Cursor::new(Vec::new());
    let mut output = Vec::new();

    let error = resolve_cli_selection(
        CliSelectionRequest {
            config: &config,
            explicit: SelectionSettings::default(),
            profile: None,
            working_directory: None,
            catalog_models: Some(Vec::new()),
            interactive: false,
            resume: false,
        },
        &mut input,
        &mut output,
    )
    .expect_err("scripted runs cannot prompt for a provider");

    assert!(error.contains("--provider"));
    assert!(error.contains("terminal"));
    assert!(input.get_ref().is_empty());
}

#[test]
fn noninteractive_missing_model_error_explains_how_to_select_one() {
    let mut config = GlobalSelectionConfig::default();
    config.defaults.provider = Some(ProviderKey::Codex);
    let mut input = io::Cursor::new(Vec::new());
    let mut output = Vec::new();

    let error = resolve_cli_selection(
        CliSelectionRequest {
            config: &config,
            explicit: SelectionSettings::default(),
            profile: None,
            working_directory: None,
            catalog_models: Some(Vec::new()),
            interactive: false,
            resume: false,
        },
        &mut input,
        &mut output,
    )
    .expect_err("scripted runs must select a model explicitly");

    assert!(error.contains("--model"));
    assert!(error.contains("terminal"));
    assert!(input.get_ref().is_empty());
}

#[test]
fn explicit_model_without_catalog_match_keeps_unknown_evidence() {
    let config = GlobalSelectionConfig::default();
    let selected = resolve_cli_selection(
        CliSelectionRequest {
            config: &config,
            explicit: SelectionSettings {
                provider: Some(ProviderKey::Codex),
                model: Some("hand-picked-model".to_owned()),
                ..SelectionSettings::default()
            },
            profile: None,
            working_directory: None,
            catalog_models: Some(Vec::new()),
            interactive: false,
            resume: false,
        },
        &mut io::Cursor::new(Vec::new()),
        &mut Vec::new(),
    )
    .expect("an exact explicit model override remains selectable");

    assert_eq!(
        selected.model_evidence,
        Some(aifuel::selection_cli::PickerModelEvidence::ExplicitOverrideUnknown)
    );
}

#[test]
fn terminal_picker_prompts_stay_out_of_structured_run_output() {
    let config = GlobalSelectionConfig {
        defaults: SelectionSettings {
            provider: Some(ProviderKey::Gemini),
            model: Some("configured-model".to_owned()),
            effort: Some("configured-effort".to_owned()),
            access: None,
            overall_deadline_seconds: None,
        },
        ..GlobalSelectionConfig::default()
    };
    let args = ["--prompt", "hello", "--output", "json"].map(str::to_owned);
    let mut input = io::Cursor::new(b"p\n".to_vec());
    let mut picker_stderr = Vec::new();
    let request = parse_run_args_with_context(
        &args,
        &config,
        Some(Vec::new()),
        &mut input,
        &mut picker_stderr,
        true,
        false,
    )
    .expect("terminal selection should complete")
    .request;
    let run_result = ManagedRunResult {
        schema_version: aifuel_core::RUN_MANAGEMENT_SCHEMA_VERSION,
        run_id: "managed-run-2".to_owned(),
        state: RunState::Succeeded,
        provider: request.provider,
        requested_model: request.model,
        requested_effort: request.effort,
        effective_model: None,
        effective_effort: None,
        local_session_id: None,
        session_id: None,
        status: Some(RunStatus::Succeeded),
        exit_code: Some(0),
        output: Some("answer".to_owned()),
        error: None,
        diagnostics: None,
        content_available: true,
        output_truncated: false,
        diagnostics_truncated: false,
        output_bytes: 6,
        diagnostics_bytes: 0,
    };

    let stdout = render_run_result(&run_result, launcher::OutputFormat::Json, None)
        .expect("structured run result should render");
    let stderr = String::from_utf8(picker_stderr).expect("picker prompts should be UTF-8");

    assert!(stdout.starts_with('{'));
    assert!(stdout.contains("managed-run-2"));
    assert!(!stdout.contains("Task scope:"));
    assert!(stderr.contains("Task scope:"));
}

#[test]
fn repeated_external_tools_are_retained_on_explicit_resume_requests() {
    let config = GlobalSelectionConfig::default();
    let args = [
        "--provider",
        "codex",
        "--prompt",
        "continue",
        "--resume",
        "thread-123",
        "--external-tool",
        "docs__search",
        "--external-tool",
        "files__read",
    ]
    .map(str::to_owned);
    let mut input = io::Cursor::new(Vec::new());
    let mut output = Vec::new();

    let request = parse_run_args_with_context(
        &args,
        &config,
        Some(Vec::new()),
        &mut input,
        &mut output,
        false,
        false,
    )
    .expect("external-tool flags should be accepted on resume")
    .request;

    assert_eq!(request.resume.as_deref(), Some("thread-123"));
    assert_eq!(
        request.external_tools,
        Some(vec!["docs__search".to_owned(), "files__read".to_owned()])
    );
    assert_eq!(request.model, None);
    assert_eq!(request.effort, None);
}

#[test]
fn run_json_exposes_the_managed_result_contract_and_model_evidence() {
    let result = ManagedRunResult {
        schema_version: aifuel_core::RUN_MANAGEMENT_SCHEMA_VERSION,
        run_id: "managed-run-1".to_owned(),
        state: RunState::Succeeded,
        provider: ProviderKey::Gemini,
        requested_model: Some("gemini-flash".to_owned()),
        requested_effort: None,
        effective_model: Some("gemini-flash".to_owned()),
        effective_effort: None,
        local_session_id: Some("local-session".to_owned()),
        session_id: None,
        status: Some(RunStatus::Succeeded),
        exit_code: Some(0),
        output: Some("answer".to_owned()),
        error: None,
        diagnostics: None,
        content_available: true,
        output_truncated: false,
        diagnostics_truncated: false,
        output_bytes: 6,
        diagnostics_bytes: 0,
    };

    let sources = SelectionSources {
        provider: aifuel_app::selection::SelectionSource::Profile("work".to_owned()),
        model: aifuel_app::selection::SelectionSource::Explicit,
        effort: aifuel_app::selection::SelectionSource::StoredSession,
        access: aifuel_app::selection::SelectionSource::GlobalDefault,
        overall_deadline: aifuel_app::selection::SelectionSource::NativeDefault,
    };
    let output = render_run_result_with_sources(
        &result,
        launcher::OutputFormat::Json,
        Some(aifuel::selection_cli::PickerModelEvidence::Catalog),
        Some(&sources),
        (false, false),
    )
    .expect("managed run should serialize as JSON");
    let value: serde_json::Value =
        serde_json::from_str(&output).expect("run output should be valid JSON");

    assert_eq!(value["run_id"], "managed-run-1");
    assert_eq!(value["provider"], "gemini");
    assert_eq!(value["state"], "succeeded");
    assert_eq!(value["output"], "answer");
    assert_eq!(value["model_evidence"], "catalog_record");
    assert_eq!(
        value["selection_sources"]["provider"],
        serde_json::json!({"profile":"work"})
    );
    assert_eq!(value["selection_sources"]["model"], "explicit");
    assert_eq!(value["selection_sources"]["effort"], "stored_session");
    assert!(value.get("provider_id").is_none());

    let jsonl = render_run_result_with_sources(
        &result,
        launcher::OutputFormat::Jsonl,
        None,
        Some(&sources),
        (false, false),
    )
    .expect("managed run should serialize as JSONL");
    let event: serde_json::Value = serde_json::from_str(&jsonl).expect("JSONL event should parse");
    assert_eq!(event["selection_sources"]["access"], "global_default");
    assert_eq!(
        event["selection_sources"]["overall_deadline"],
        "native_default"
    );

    let deferred = render_run_result_with_sources(
        &result,
        launcher::OutputFormat::Json,
        None,
        Some(&sources),
        (true, true),
    )
    .expect("resumed run should omit unresolved source values");
    let deferred: serde_json::Value =
        serde_json::from_str(&deferred).expect("deferred source result should parse");
    assert!(deferred["selection_sources"].get("model").is_none());
    assert!(deferred["selection_sources"].get("effort").is_none());
    assert_eq!(
        deferred["selection_sources"]["provider"],
        serde_json::json!({"profile":"work"})
    );

    let unknown_output = render_run_result(
        &result,
        launcher::OutputFormat::Json,
        Some(aifuel::selection_cli::PickerModelEvidence::ExplicitOverrideUnknown),
    )
    .expect("explicit override evidence should serialize");
    let unknown_value: serde_json::Value =
        serde_json::from_str(&unknown_output).expect("JSON output should parse");
    assert_eq!(unknown_value["model_evidence"], "explicit_override_unknown");
}
