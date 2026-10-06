//! `GET /v1/models`: the aggregated list an OpenAI-compatible app's model
//! picker reads. Entries name only what the executable adapter set can
//! serve - catalog advertisement narrows `auto` routing but never lists a
//! model no integration runs.

use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

/// `{"object":"list","data":[...]}` over `auto`, every executable
/// integration, and each catalog-advertised model pinned under its
/// integration id (`<integration>/<model>` - the inbound addressing
/// convention, so a picker selection round-trips through
/// `/v1/chat/completions`).
pub(crate) fn list(gateway: &super::Gateway) -> Value {
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let entry = |id: String, owned_by: &str| json!({"id": id, "object": "model", "created": created, "owned_by": owned_by});

    let mut seen = BTreeSet::new();
    let mut data = vec![entry(aifuel_core::AUTO_PROVIDER.to_owned(), "aifuel")];
    seen.insert(aifuel_core::AUTO_PROVIDER.to_owned());
    data.push(entry(super::execute::DECIDE_SELECTOR.to_owned(), "aifuel"));
    seen.insert(super::execute::DECIDE_SELECTOR.to_owned());
    for adapter in gateway.adapters() {
        let id = adapter.integration().as_str().to_owned();
        if seen.insert(id.clone()) {
            data.push(entry(id, adapter.provider().as_str()));
        }
    }
    // Declared models pin under their integration id. gateway.json's
    // optional `models` map is the single advertised source client
    // pickers read - the provider-discovered catalog still feeds effort
    // validation elsewhere and marks each declaration `verified` when it
    // lists the same model id. Declared effort values ride the entry as
    // `reasoning`/`default_reasoning` so a picker can offer model and
    // effort independently.
    let declared = super::route_config::declared_models();
    if !declared.is_empty() {
        let catalog = crate::run_selection::load_picker_models().unwrap_or_default();
        for adapter in gateway.adapters() {
            let Some(models) = declared.get(adapter.integration().as_str()) else {
                continue;
            };
            let provider = adapter
                .provider()
                .as_str()
                .parse::<aifuel_core::ProviderKey>()
                .ok();
            for model in models {
                if model.id().trim().is_empty() {
                    continue;
                }
                let id = format!("{}/{}", adapter.integration().as_str(), model.id());
                if !seen.insert(id.clone()) {
                    continue;
                }
                let mut entry = entry(id, adapter.provider().as_str());
                if let Some(label) = model.label() {
                    entry["label"] = json!(label);
                }
                if !model.efforts().is_empty() {
                    entry["reasoning"] = json!(model.efforts());
                }
                if let Some(default) = model.default_effort() {
                    entry["default_reasoning"] = json!(default);
                }
                entry["verified"] = json!(provider.is_some_and(|provider| {
                    catalog.iter().any(|catalog_model| {
                        catalog_model.provider == provider
                            && catalog_model.model_id == model.id()
                            && catalog_model.advertisement
                                == aifuel_core::CapabilityState::Supported
                    })
                }));
                data.push(entry);
            }
        }
    }
    json!({"object": "list", "data": data})
}
