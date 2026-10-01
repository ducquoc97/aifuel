//! Application workflows shared by AI Fuel's executable interfaces.

use aifuel_core::{StatusCollector, StatusReport};
use std::sync::Mutex;
use std::time::{Duration, Instant};

mod agent_mcp_setup;
#[cfg(unix)]
mod approval_ipc;
#[cfg(windows)]
#[path = "approval_ipc_windows.rs"]
mod approval_ipc;
#[cfg(any(unix, windows))]
mod approval_ipc_protocol;
mod catalog;
mod content_store;
mod execution;
mod gateway;
mod run_management;
mod run_store;
pub mod selection;
mod session_store;
#[cfg(test)]
mod test_support;
mod webhooks;
mod workspace_lock;
pub use agent_mcp_setup::{
    AgentMcpSetupAction, AgentMcpSetupError, AgentMcpSetupFacade, AgentMcpSetupOptions,
    AgentMcpSetupResult,
};
#[cfg(any(unix, windows))]
pub use approval_ipc::submit_local_approval;
#[cfg(any(unix, windows))]
pub use approval_ipc_protocol::LocalApprovalDecision;
pub use catalog::{McpCatalogError, McpCatalogFacade, McpCatalogSelection};
pub use execution::AgentRunFacade;
pub use gateway::{
    BearerTokenAuth, GatewayConfigError, GatewayLimits, McpGatewayFacade, McpServerDefinition,
    NamedSecretHeader, SelectedMcpServer, ServerLimits, StdioServerDefinition,
    StreamableHttpServerDefinition, default_cwd,
};
pub use run_management::{AgentExecutionAdapters, RunManager, RunManagerPolicy};
pub use run_store::{
    OwnerGuard, ReplayPage, RunStore, RunStoreError, StoredAgentSession, warn_store_write,
};
pub use selection::{
    AccountContext, CatalogEvidenceStore, CatalogFreshness, CatalogLookup, CatalogModel,
    CatalogProvenance, CatalogRefreshResult, CatalogScope, CatalogSnapshot, ContentRetention,
    EffortEvidence, ExecutionPolicy, GLOBAL_SELECTION_SCHEMA_VERSION, GlobalSelectionConfig,
    MODEL_CATALOG_TTL, ModelCatalogError, ModelEvidenceState, ProfileSettings, ResolvedSelection,
    SelectionError, SelectionInputs, SelectionResolver, SelectionSettings, SelectionSource,
    SelectionSources, SelectionStore, SelectionStoreError, StoredSession,
};
pub use webhooks::{WEBHOOKS_FILE_NAME, WebhookConfigError, WebhookNotifier};

const STATUS_CACHE_TTL: Duration = Duration::from_secs(300);

/// Shared monitoring workflow for text, JSON, dashboard, and MCP interfaces.
pub struct MonitoringFacade<C> {
    collector: C,
    cache: Mutex<Option<(Instant, StatusReport)>>,
    notifier: Option<WebhookNotifier>,
}

impl<C> MonitoringFacade<C>
where
    C: StatusCollector,
{
    pub fn new(collector: C) -> Self {
        Self {
            collector,
            cache: Mutex::new(None),
            notifier: None,
        }
    }

    /// Attach the configured webhook notifier. Each fresh collection is then
    /// evaluated for quota events before it is cached, so deliveries run on
    /// the same cadence as collection across every interface.
    pub fn with_notifier(mut self, notifier: Option<WebhookNotifier>) -> Self {
        self.notifier = notifier;
        self
    }

    /// Return cached monitoring status when it remains fresh, or collect and
    /// cache a new report. `refresh` always performs a new collection.
    pub async fn status(&self, refresh: bool) -> StatusReport {
        if !refresh
            && let Some((created_at, report)) =
                self.cache.lock().expect("status cache mutex").as_ref()
            && created_at.elapsed() < STATUS_CACHE_TTL
        {
            return report.clone();
        }

        let report = self.collector.collect_status().await;
        if let Some(notifier) = &self.notifier {
            notifier.deliver(&report).await;
        }
        *self.cache.lock().expect("status cache mutex") = Some((Instant::now(), report.clone()));
        report
    }

    pub async fn collect(&self) -> StatusReport {
        self.status(true).await
    }

    /// Return the latest collected report without starting a collection.
    ///
    /// This supports cache-only callers such as the monitoring MCP resource.
    /// A cached report may be older than the refresh threshold because this
    /// operation deliberately never performs network or credential reads.
    pub fn cached_status(&self) -> Option<StatusReport> {
        self.cache
            .lock()
            .expect("status cache mutex")
            .as_ref()
            .map(|(_, report)| report.clone())
    }
}
