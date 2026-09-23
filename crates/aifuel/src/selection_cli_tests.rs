use super::*;
use std::io::Cursor;

#[test]
fn picker_uses_catalog_model_and_prompt_only_scope_without_inventing_ids() {
    let options = PickerOptions {
        models: vec![PickerModel {
            provider: ProviderKey::Codex,
            model_id: "provider-exact-id".to_owned(),
            display_label: Some("Observed model".to_owned()),
            provenance: CatalogProvenance::ProviderApi,
            advertisement: CapabilityState::Supported,
            entitlement: CapabilityState::Unknown,
            execution: CapabilityState::Unknown,
            scope_label: Some("test scope".to_owned()),
            catalog_age_seconds: 120,
            default_effort: Some("medium".to_owned()),
            effort_values: vec!["high".to_owned()],
            effort_state: CapabilityState::Supported,
        }],
        ..PickerOptions::default()
    };
    let mut input = Cursor::new(b"2\n1\n\np\n".to_vec());
    let mut output = Vec::new();
    let result = pick(&mut input, &mut output, options)
        .expect("controlled terminal should be accepted")
        .expect("picker should return a selection");
    assert_eq!(result.settings.provider, Some(ProviderKey::Codex));
    assert_eq!(result.settings.model.as_deref(), Some("provider-exact-id"));
    assert_eq!(result.settings.effort, None);
    assert_eq!(result.working_directory, None);
    assert_eq!(result.model_evidence, PickerModelEvidence::Catalog);
    let output = String::from_utf8(output).expect("picker output should be UTF-8");
    assert!(output.contains("advertisement: supported"));
    assert!(output.contains("entitlement: unknown"));
    assert!(output.contains("execution: unknown"));
    assert!(output.contains("catalog age: 120s"));
    assert!(output.contains("test scope"));
    assert!(output.contains("catalog-reported default \"medium\""));
}

#[test]
fn picker_labels_explicit_model_override_as_unknown() {
    let options = PickerOptions::default();
    let mut input = Cursor::new(b"1\nmy-deliberate-model\n\np\n".to_vec());
    let mut output = Vec::new();
    let result = pick(&mut input, &mut output, options)
        .expect("controlled terminal should be accepted")
        .expect("picker should return a selection");
    assert_eq!(
        result.model_evidence,
        PickerModelEvidence::ExplicitOverrideUnknown
    );
    assert_eq!(
        result.settings.model.as_deref(),
        Some("my-deliberate-model")
    );
    assert!(
        String::from_utf8(output)
            .expect("picker output is text")
            .contains("support remains unknown")
    );
}

#[test]
fn picker_cancel_is_explicit() {
    let mut input = Cursor::new(b"c\n".to_vec());
    let mut output = Vec::new();
    assert_eq!(
        pick(&mut input, &mut output, PickerOptions::default())
            .expect("controlled terminal should be accepted"),
        Err(PickerOutcome::Cancelled)
    );
}

#[test]
fn provider_only_picker_does_not_request_model_or_scope() {
    let mut input = Cursor::new(b"2\n".to_vec());
    let mut output = Vec::new();
    let selected = pick_provider(
        &mut input,
        &mut output,
        &[ProviderKey::Claude, ProviderKey::Codex],
    )
    .expect("controlled terminal should be accepted")
    .expect("provider should be selected");

    assert_eq!(selected, ProviderKey::Codex);
    let output = String::from_utf8(output).expect("picker output should be UTF-8");
    assert!(output.contains("Select provider"));
    assert!(!output.contains("Select model"));
    assert!(!output.contains("Task scope"));
}
