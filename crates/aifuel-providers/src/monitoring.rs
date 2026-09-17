use crate::usage_helpers::unix_timestamp;
use aifuel_core::{StatusCollector, StatusReport};
use futures_util::future::join_all;
use reqwest::Client;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct CollectionConfig {
    pub claude_usage_url: String,
    pub codex_usage_url: String,
    pub copilot_user_url: String,
    pub copilot_token_url: String,
    pub gemini_api_url: String,
}

impl Default for CollectionConfig {
    fn default() -> Self {
        Self {
            claude_usage_url: "https://api.anthropic.com/api/oauth/usage".to_owned(),
            codex_usage_url: "https://chatgpt.com/backend-api/codex/usage".to_owned(),
            copilot_user_url: "https://api.github.com/copilot_internal/user".to_owned(),
            copilot_token_url: "https://api.github.com/copilot_internal/v2/token".to_owned(),
            gemini_api_url: "https://cloudcode-pa.googleapis.com/v1internal:".to_owned(),
        }
    }
}

impl CollectionConfig {
    pub fn from_environment() -> Self {
        let mut config = Self::default();
        replace_from_env(&mut config.claude_usage_url, "AIFUEL_CLAUDE_USAGE_URL");
        replace_from_env(&mut config.codex_usage_url, "AIFUEL_CODEX_USAGE_URL");
        replace_from_env(&mut config.copilot_user_url, "AIFUEL_COPILOT_USER_URL");
        replace_from_env(&mut config.copilot_token_url, "AIFUEL_COPILOT_TOKEN_URL");
        replace_from_env(&mut config.gemini_api_url, "AIFUEL_GEMINI_API_URL");
        config
    }
}

fn replace_from_env(target: &mut String, name: &str) {
    if let Ok(value) = std::env::var(name) {
        if !value.trim().is_empty() {
            *target = value;
        }
    }
}

/// Provider-owned implementation of the read-only monitoring contract.
/// Caching belongs to the application facade, while this adapter recomputes
/// Provider Discovery and quota status for each requested collection.
pub struct ProviderMonitoring {
    pub(crate) home_dir: PathBuf,
    pub(crate) config: CollectionConfig,
    pub(crate) client: Client,
}

impl ProviderMonitoring {
    pub fn new(home_dir: impl Into<PathBuf>, config: CollectionConfig) -> Result<Self, String> {
        let client = Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|error| format!("could not create HTTP client: {error}"))?;
        Ok(Self {
            home_dir: home_dir.into(),
            config,
            client,
        })
    }

    pub fn home_dir(&self) -> &Path {
        &self.home_dir
    }

    async fn collect_live(&self) -> StatusReport {
        let discovery_context = crate::DiscoveryContext::new(&self.home_dir);
        let selection = crate::default_registry().discover_and_initialize(&discovery_context);
        let discovery_errors = selection.report().discovery_errors.clone();
        let monitoring_registry = crate::default_monitoring_registry();
        let providers = join_all(
            selection
                .providers()
                .copied()
                .map(|provider| monitoring_registry.collect(provider, self)),
        )
        .await;

        let mut report = StatusReport::from_usage(unix_timestamp(), providers, discovery_errors);
        report.catalog = crate::catalog::statuses();
        report
    }
}

impl StatusCollector for ProviderMonitoring {
    fn collect_status(&self) -> Pin<Box<dyn Future<Output = StatusReport> + Send + '_>> {
        Box::pin(self.collect_live())
    }
}
