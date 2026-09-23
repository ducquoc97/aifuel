use crate as aifuel;
use aifuel_app::selection::CatalogModel;

pub(crate) fn model_evidence_for_model(
    provider: Option<aifuel_core::ProviderKey>,
    model_id: &str,
    cached_models: Option<&[aifuel::selection_cli::PickerModel]>,
) -> aifuel::selection_cli::PickerModelEvidence {
    let loaded_models;
    let models = if let Some(models) = cached_models {
        models
    } else {
        loaded_models = load_picker_models().unwrap_or_default();
        &loaded_models
    };
    if models
        .iter()
        .any(|model| Some(model.provider) == provider && model.model_id == model_id)
    {
        aifuel::selection_cli::PickerModelEvidence::Catalog
    } else {
        aifuel::selection_cli::PickerModelEvidence::ExplicitOverrideUnknown
    }
}

pub(crate) fn load_picker_models() -> Result<Vec<aifuel::selection_cli::PickerModel>, String> {
    let snapshots = aifuel::model_catalog_snapshot()?;
    let mut models = Vec::new();
    let mut stale_ages = Vec::new();
    for snapshot in snapshots {
        let age_seconds = snapshot
            .get("age_seconds")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default();
        let scope_label = catalog_scope_label(&snapshot);
        if snapshot
            .get("freshness")
            .and_then(serde_json::Value::as_str)
            == Some("stale")
        {
            stale_ages.push(age_seconds);
        }
        for model in snapshot
            .get("models")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            let model: CatalogModel = serde_json::from_value(model.clone())
                .map_err(|error| format!("could not read model catalog entry: {error}"))?;
            let mut picker_model = aifuel::selection_cli::PickerModel::from(&model);
            picker_model.scope_label = scope_label.clone();
            picker_model.catalog_age_seconds = age_seconds;
            models.push(picker_model);
        }
    }
    if !stale_ages.is_empty() {
        let oldest_age = stale_ages.iter().copied().max().unwrap_or_default();
        eprintln!(
            "aifuel: cached model catalog includes stale evidence (oldest scope age: {oldest_age}s); model advertisement does not establish account entitlement or execution availability"
        );
    }
    Ok(models)
}

fn catalog_scope_label(snapshot: &serde_json::Value) -> Option<String> {
    let scope = snapshot.get("scope")?;
    let integration = scope
        .get("integration_version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let platform = scope
        .get("platform")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let account = scope.get("account");
    let account_scope = if account.and_then(|value| value.get("Known")).is_some() {
        "identified"
    } else {
        "unknown"
    };
    Some(format!(
        "integration={integration}, platform={platform}, account={account_scope}"
    ))
}
