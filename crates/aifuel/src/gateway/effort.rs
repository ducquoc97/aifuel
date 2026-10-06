//! Effort selection for `/v1` requests: the `<selector>@<effort>` suffix
//! and the catalog-evidence check a requested effort passes before it
//! reaches an attempt.

use crate::selection_cli::PickerModel;
use aifuel_core::{CapabilityState, ProviderKey};

/// Split `selector@effort`: the suffix pins the effort next to the model
/// it belongs to, so a pinned spelling always names a concrete value.
/// An `@` with an empty side is not a suffix - the string passes through
/// untouched and the provider's own error answers it.
pub(crate) fn split(selector: &str) -> (&str, Option<&str>) {
    match selector.rsplit_once('@') {
        Some((model, effort)) if !model.is_empty() && !effort.is_empty() => {
            (model, Some(effort))
        }
        _ => (selector, None),
    }
}

/// Reject an effort the provider's catalog evidence does not list for the
/// pinned model. Evidence gaps - no catalog entry, unreported effort -
/// never reject: a refusal needs positive evidence that the model offers
/// other values.
pub(crate) fn check(
    models: &[PickerModel],
    provider: ProviderKey,
    model: Option<&str>,
    effort: &str,
) -> Result<(), (u16, String)> {
    let Some(model) = model else {
        return Ok(());
    };
    let Some(entry) = models
        .iter()
        .find(|entry| entry.provider == provider && entry.model_id == model)
    else {
        return Ok(());
    };
    if entry.effort_state != CapabilityState::Supported
        || entry.effort_values.iter().any(|value| value == effort)
    {
        return Ok(());
    }
    Err((
        400,
        if entry.effort_values.is_empty() {
            format!("{model:?} reports no effort choices on {provider}; drop the effort request")
        } else {
            format!(
                "effort {effort:?} is not advertised for {model:?} on {provider}; supported: {}",
                entry.effort_values.join(", ")
            )
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_reads_the_effort_suffix() {
        assert_eq!(split("codex/gpt-5@high"), ("codex/gpt-5", Some("high")));
        assert_eq!(split("devin/swe-2"), ("devin/swe-2", None));
        // Effort applies after the last path segment, so model ids with
        // slashes keep their own `@` split.
        assert_eq!(split("auto/gpt-5@low"), ("auto/gpt-5", Some("low")));
    }

    #[test]
    fn split_rejects_empty_sides() {
        assert_eq!(split("@high"), ("@high", None));
        assert_eq!(split("codex/gpt-5@"), ("codex/gpt-5@", None));
    }

    fn model(effort_state: CapabilityState, effort_values: &[&str]) -> PickerModel {
        PickerModel {
            provider: ProviderKey::Codex,
            model_id: "gpt-5.6".to_owned(),
            display_label: None,
            provenance: aifuel_app::selection::CatalogProvenance::BundledCatalog,
            advertisement: CapabilityState::Supported,
            entitlement: CapabilityState::Unknown,
            execution: CapabilityState::Unknown,
            scope_label: None,
            catalog_age_seconds: 0,
            default_effort: None,
            effort_values: effort_values.iter().map(|value| value.to_string()).collect(),
            effort_state,
        }
    }

    #[test]
    fn check_enforces_listed_values_only() {
        let models = vec![model(
            CapabilityState::Supported,
            &["low", "high"],
        )];
        assert!(check(&models, ProviderKey::Codex, Some("gpt-5.6"), "high").is_ok());
        let (status, message) =
            check(&models, ProviderKey::Codex, Some("gpt-5.6"), "ultra").unwrap_err();
        assert_eq!(status, 400);
        assert!(message.contains("low, high"));
    }

    #[test]
    fn check_rejects_effort_when_the_model_reports_no_axis() {
        let models = vec![model(CapabilityState::Supported, &[])];
        assert!(check(&models, ProviderKey::Codex, Some("gpt-5.6"), "high").is_err());
    }

    #[test]
    fn check_passes_when_evidence_is_missing() {
        let models = vec![model(CapabilityState::Unknown, &[])];
        assert!(check(&models, ProviderKey::Codex, Some("gpt-5.6"), "high").is_ok());
        // No catalog entry, no model pinned, other provider: all unknown.
        assert!(check(&models, ProviderKey::Codex, Some("other-model"), "high").is_ok());
        assert!(check(&models, ProviderKey::Codex, None, "high").is_ok());
        assert!(check(&models, ProviderKey::Devin, Some("gpt-5.6"), "high").is_ok());
    }
}
