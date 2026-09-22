//! Application workflows shared by AI Fuel's executable interfaces.

use aifuel_core::{StatusCollector, StatusReport};
use std::sync::Mutex;
use std::time::{Duration, Instant};

mod agent_mcp_setup;
mod catalog;
mod execution;
mod gateway;
mod run_management;
pub mod selection;
mod workspace_lock;
pub use agent_mcp_setup::{
    AgentMcpSetupAction, AgentMcpSetupError, AgentMcpSetupFacade, AgentMcpSetupOptions,
    AgentMcpSetupResult,
};
pub use catalog::{McpCatalogError, McpCatalogFacade, McpCatalogSelection};
pub use execution::AgentRunFacade;
pub use gateway::{
    BearerTokenAuth, GatewayConfigError, GatewayLimits, McpGatewayFacade, McpServerDefinition,
    NamedSecretHeader, SelectedMcpServer, ServerLimits, StdioServerDefinition,
    StreamableHttpServerDefinition, default_cwd,
};
pub use run_management::{RunManager, RunManagerPolicy};
pub use selection::{
    AccountContext, CatalogEvidenceStore, CatalogFreshness, CatalogLookup, CatalogModel,
    CatalogProvenance, CatalogRefreshResult, CatalogScope, CatalogSnapshot, ContentRetention,
    EffortEvidence, ExecutionPolicy, GLOBAL_SELECTION_SCHEMA_VERSION, GlobalSelectionConfig,
    MODEL_CATALOG_TTL, ModelCatalogError, ModelEvidenceState, ProfileSettings, ResolvedSelection,
    SelectionError, SelectionInputs, SelectionResolver, SelectionSettings, SelectionSource,
    SelectionSources, SelectionStore, SelectionStoreError, StoredSession,
};

const STATUS_CACHE_TTL: Duration = Duration::from_secs(300);

/// Shared monitoring workflow for text, JSON, dashboard, and MCP interfaces.
pub struct MonitoringFacade<C> {
    collector: C,
    cache: Mutex<Option<(Instant, StatusReport)>>,
}

impl<C> MonitoringFacade<C>
where
    C: StatusCollector,
{
    pub fn new(collector: C) -> Self {
        Self {
            collector,
            cache: Mutex::new(None),
        }
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
