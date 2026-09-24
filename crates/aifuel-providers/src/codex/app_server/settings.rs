use serde_json::Value;

pub(super) struct ReportedRunSettings {
    pub model: Option<String>,
    pub effort: Option<String>,
}

pub(super) fn reported_run_settings(
    thread_response: &Value,
    requested_model: Option<&str>,
    requested_effort: Option<&str>,
) -> ReportedRunSettings {
    let reported_model = thread_response["result"]["model"].as_str();
    let model = match (requested_model, reported_model) {
        (None, Some(reported)) => Some(reported.to_owned()),
        (Some(requested), Some(reported)) if requested == reported => Some(reported.to_owned()),
        _ => None,
    };
    let effort = requested_effort
        .is_none()
        .then(|| {
            thread_response["result"]["reasoningEffort"]
                .as_str()
                .map(str::to_owned)
        })
        .flatten();
    ReportedRunSettings { model, effort }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn uses_provider_reported_settings_without_claiming_unmatched_overrides() {
        let response = json!({
            "result":{"model":"gpt-6-astra","reasoningEffort":"max"}
        });
        let native_defaults = reported_run_settings(&response, None, None);
        assert_eq!(native_defaults.model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(native_defaults.effort.as_deref(), Some("max"));

        let exact_model = reported_run_settings(&response, Some("gpt-6-astra"), None);
        assert_eq!(exact_model.model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(exact_model.effort.as_deref(), Some("max"));

        let explicit_effort = reported_run_settings(&response, Some("gpt-6-astra"), Some("high"));
        assert_eq!(explicit_effort.model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(explicit_effort.effort, None);

        let unmatched_model = reported_run_settings(&response, Some("different-model"), None);
        assert_eq!(unmatched_model.model, None);
    }
}
