use super::{CatalogProvider, MonitoringFuture, ProviderMonitoring};
use crate::usage_helpers::{read_json, value_string};
use aifuel_core::{ProviderKey, ProviderUsage};

pub static DEFINITION: CatalogProvider = CatalogProvider::directory_sources(
    ProviderKey::Antigravity,
    &[".gemini/antigravity", ".gemini/antigravity-cli"],
);

pub(crate) fn collect(service: &ProviderMonitoring) -> MonitoringFuture<'_> {
    Box::pin(collect_live(service))
}

async fn collect_live(service: &ProviderMonitoring) -> ProviderUsage {
    let project = service
        .home_dir
        .join(".gemini/antigravity-cli/settings.json");
    let project = read_json(&project).ok().and_then(|value| {
        value
            .get("gcp")
            .and_then(|gcp| gcp.get("project"))
            .and_then(value_string)
    });
    crate::code_assist::collect(
        service,
        ProviderKey::Antigravity,
        ".gemini/antigravity-cli/antigravity-oauth-token",
        project,
        "antigravity/usage-monitor",
        None,
    )
    .await
}
