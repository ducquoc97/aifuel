use super::{CatalogProvider, MonitoringFuture, ProviderMonitoring};
use crate::usage_helpers::{read_json, value_string};
use aifuel_core::{ProviderKey, ProviderUsage};

mod agent_run;
pub(crate) use agent_run::ADAPTER as AGENT_RUN_ADAPTER;
mod registration;
pub(crate) use registration::ADAPTER as MCP_REGISTRATION_ADAPTER;

pub static DEFINITION: CatalogProvider = CatalogProvider::directory_sources(
    ProviderKey::Antigravity,
    &[".gemini/antigravity", ".gemini/antigravity-cli"],
);

// The Antigravity client's public installed-app OAuth client (bundled with
// the IDE), used only to renew stored tokens in memory.
const RECOVERY: crate::code_assist::CredentialRecovery = crate::code_assist::CredentialRecovery {
    client_id: "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com",
    client_secret: "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",
    reauth: "antigravity",
};

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
        &RECOVERY,
    )
    .await
}
