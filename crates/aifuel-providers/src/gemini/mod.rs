use super::{CatalogProvider, MonitoringFuture, ProviderMonitoring};
use crate::usage_helpers::project_from_environment;
use aifuel_core::{ProviderKey, ProviderUsage};

pub static DEFINITION: CatalogProvider =
    CatalogProvider::file_source(ProviderKey::Gemini, ".gemini/oauth_creds.json");

pub(crate) fn collect(service: &ProviderMonitoring) -> MonitoringFuture<'_> {
    Box::pin(collect_live(service))
}

async fn collect_live(service: &ProviderMonitoring) -> ProviderUsage {
    crate::code_assist::collect(
        service,
        ProviderKey::Gemini,
        ".gemini/oauth_creds.json",
        project_from_environment(),
        "gemini-cli/usage-monitor",
        Some("daily"),
    )
    .await
}
