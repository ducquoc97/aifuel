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
    // Catalog-advertised models pin under every executable integration of
    // their provider. Advertisement is provider-scoped evidence; the
    // composite id keeps it honest (never a bare model id that `auto`
    // would have to guess at). Reported effort evidence rides the entry
    // as `reasoning`/`default_reasoning` so a client picker can offer
    // model and effort independently.
    if let Ok(models) = crate::run_selection::load_picker_models() {
        for model in models
            .iter()
            .filter(|model| model.advertisement == aifuel_core::CapabilityState::Supported)
        {
            for adapter in gateway.adapters() {
                let advertised = adapter
                    .provider()
                    .as_str()
                    .parse::<aifuel_core::ProviderKey>()
                    .is_ok_and(|key| key == model.provider);
                if !advertised {
                    continue;
                }
                let id = format!("{}/{}", adapter.integration().as_str(), model.model_id);
                if seen.insert(id.clone()) {
                    let mut entry = entry(id, adapter.provider().as_str());
                    if model.effort_state == aifuel_core::CapabilityState::Supported
                        && !model.effort_values.is_empty()
                    {
                        entry["reasoning"] = json!(model.effort_values);
                        if let Some(default) = &model.default_effort {
                            entry["default_reasoning"] = json!(default);
                        }
                    }
                    data.push(entry);
                }
            }
        }
    }
    json!({"object": "list", "data": data})
}
