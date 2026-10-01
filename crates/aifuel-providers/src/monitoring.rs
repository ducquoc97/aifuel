use crate::usage_helpers::unix_timestamp;
use aifuel_core::{
    ApiKeySource, AuthBinding, ExecutionConfig, KeyDelivery, ObservationState, StatusCollector,
    StatusError, StatusErrorCode, StatusObservation, StatusReport,
};
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
    pub devin_api_server_url: String,
    pub gemini_api_url: String,
    pub openrouter_key_url: String,
    pub zai_quota_url: String,
    pub deepseek_balance_url: String,
    pub siliconflow_balance_url: String,
}

impl Default for CollectionConfig {
    fn default() -> Self {
        Self {
            claude_usage_url: "https://api.anthropic.com/api/oauth/usage".to_owned(),
            codex_usage_url: "https://chatgpt.com/backend-api/codex/usage".to_owned(),
            copilot_user_url: "https://api.github.com/copilot_internal/user".to_owned(),
            copilot_token_url: "https://api.github.com/copilot_internal/v2/token".to_owned(),
            devin_api_server_url: "https://server.codeium.com".to_owned(),
            gemini_api_url: "https://cloudcode-pa.googleapis.com/v1internal:".to_owned(),
            openrouter_key_url: "https://openrouter.ai/api/v1/key".to_owned(),
            zai_quota_url: "https://api.z.ai/api/monitor/usage/quota/limit".to_owned(),
            deepseek_balance_url: "https://api.deepseek.com/user/balance".to_owned(),
            siliconflow_balance_url: "https://api.siliconflow.com/v1/user/info".to_owned(),
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
        replace_from_env(
            &mut config.devin_api_server_url,
            "AIFUEL_DEVIN_API_SERVER_URL",
        );
        replace_from_env(&mut config.gemini_api_url, "AIFUEL_GEMINI_API_URL");
        replace_from_env(
            &mut config.openrouter_key_url,
            "AIFUEL_OPENROUTER_USAGE_URL",
        );
        replace_from_env(&mut config.zai_quota_url, "AIFUEL_ZAI_USAGE_URL");
        replace_from_env(
            &mut config.deepseek_balance_url,
            "AIFUEL_DEEPSEEK_USAGE_URL",
        );
        replace_from_env(
            &mut config.siliconflow_balance_url,
            "AIFUEL_SILICONFLOW_USAGE_URL",
        );
        config
    }
}

fn replace_from_env(target: &mut String, name: &str) {
    if let Ok(value) = std::env::var(name)
        && !value.trim().is_empty()
    {
        *target = value;
    }
}

/// Provider-owned implementation of the read-only monitoring contract.
/// Caching belongs to the application facade, while this adapter recomputes
/// Provider Discovery and quota status for each requested collection.
pub struct ProviderMonitoring {
    pub(crate) home_dir: PathBuf,
    pub(crate) config: CollectionConfig,
    pub(crate) client: Client,
    pub(crate) integrations: Vec<crate::IntegrationDescriptor>,
    pub(crate) credentials: Option<crate::CredentialStore>,
    /// A registry-construction failure recorded by the caller. Integration
    /// collection then reports it as a collection error rather than silently
    /// producing an empty integration section - a malformed registry must
    /// surface, not narrow.
    pub(crate) registry_error: Option<String>,
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
            integrations: Vec::new(),
            credentials: None,
            registry_error: None,
        })
    }

    pub fn home_dir(&self) -> &Path {
        &self.home_dir
    }

    /// Attach the runtime Integration set so integrations declaring a
    /// Monitoring Collection Contract produce typed observations alongside
    /// the catalog-provider collection.
    pub fn with_integrations(
        mut self,
        integrations: Vec<crate::IntegrationDescriptor>,
        credentials: crate::CredentialStore,
    ) -> Self {
        self.integrations = integrations;
        self.credentials = Some(credentials);
        self
    }

    /// Record that the runtime registry could not be built. The failure is
    /// reported as a collection error in the next report so status output is
    /// honest about the missing integration coverage.
    pub fn with_registry_error(mut self, error: String) -> Self {
        self.registry_error = Some(error);
        self
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

        let collected_at = unix_timestamp();
        let mut report = StatusReport::from_usage(collected_at, providers, discovery_errors);
        let (observations, errors) = self.collect_integrations(collected_at).await;
        report.observations.extend(observations);
        if !errors.is_empty() {
            report.collection.errors.extend(errors);
            if report.collection.outcome == Some(aifuel_core::CollectionOutcome::Complete) {
                report.collection.outcome = Some(aifuel_core::CollectionOutcome::Partial);
            }
        }
        report.catalog = crate::catalog::statuses();
        report
    }

    /// Run the Monitoring Collection Contract of every configured
    /// integration. An HTTP integration with no contract reports
    /// `Unsupported` (a CLI integration's monitoring lives on its catalog
    /// provider, so it is not repeated here). Each observation is recorded
    /// under the integration's own identity; credential problems are
    /// Unauthenticated observations, transport and shape problems are
    /// Unavailable, and the same failure is also reported as a collection
    /// error so CLI/MCP diagnostics can surface it.
    async fn collect_integrations(&self, now: f64) -> (Vec<StatusObservation>, Vec<StatusError>) {
        let mut observations = Vec::new();
        let mut errors = Vec::new();
        if let Some(error) = &self.registry_error {
            errors.push(StatusError {
                provider_id: None,
                account_id: None,
                code: StatusErrorCode::CollectionFailed,
                message: format!("integration registry did not build: {error}"),
            });
        }
        let Some(credentials) = &self.credentials else {
            return (observations, errors);
        };
        for descriptor in &self.integrations {
            let integration_id = &descriptor.integration.id;
            let provider_id = &descriptor.integration.provider;
            let Some(monitoring) = &descriptor.integration.monitoring else {
                // A wire integration without a monitoring contract reports
                // Unsupported honestly rather than vanishing from the report.
                if let ExecutionConfig::Http { .. } = &descriptor.integration.execution {
                    observations.push(crate::quota::unobserved(
                        integration_id,
                        provider_id,
                        now,
                        ObservationState::Unsupported,
                        "monitoring",
                        "Quota monitoring",
                    ));
                }
                continue;
            };
            let Some(collector) = crate::quota::spec(monitoring.collector.as_str()) else {
                errors.push(integration_error(
                    provider_id,
                    format!("unknown monitoring collector {}", monitoring.collector),
                ));
                observations.push(crate::quota::unobserved(
                    integration_id,
                    provider_id,
                    now,
                    ObservationState::Unsupported,
                    "monitoring",
                    "Quota monitoring",
                ));
                continue;
            };
            let url = monitoring
                .endpoint
                .as_ref()
                .map(|endpoint| endpoint.base_url.clone())
                .unwrap_or_else(|| (collector.default_url)(&self.config));
            // A dedicated monitoring credential is a Bearer API key; absent
            // one, the observation reuses the integration's execution binding.
            let binding = match &monitoring.credential {
                Some(credential) => AuthBinding::ApiKey {
                    source: ApiKeySource::Store {
                        credential: credential.clone(),
                    },
                    delivery: KeyDelivery::Bearer,
                },
                None => match &descriptor.integration.execution {
                    ExecutionConfig::Http { auth, .. } => auth.clone(),
                    ExecutionConfig::Cli { .. } => AuthBinding::None,
                },
            };
            // Credential resolution is blocking file I/O; spec rule 8 keeps
            // it off the async worker threads.
            let store = credentials.clone();
            let resolve_integration = integration_id.clone();
            let resolved =
                tokio::task::spawn_blocking(move || store.resolve(&binding, &resolve_integration))
                    .await
                    .map_err(|error| {
                        integration_error(
                            provider_id,
                            format!("credential resolution could not run: {error}"),
                        )
                    });
            let outcome = match resolved {
                Err(error) => Err((ObservationState::Unavailable, error.message)),
                Ok(Err(error)) => Err((
                    ObservationState::Unauthenticated,
                    format!("the declared credential did not resolve: {error}"),
                )),
                Ok(Ok(auth)) => {
                    match (collector.collect)(
                        &self.client,
                        &url,
                        monitoring.endpoint.as_ref(),
                        &auth,
                        integration_id,
                        provider_id,
                        now,
                    )
                    .await
                    {
                        Ok(collected) => Ok(collected),
                        Err(crate::quota::QuotaError::Unauthenticated(detail)) => {
                            Err((ObservationState::Unauthenticated, detail))
                        }
                        Err(crate::quota::QuotaError::Unavailable(detail)) => {
                            Err((ObservationState::Unavailable, detail))
                        }
                    }
                }
            };
            match outcome {
                Ok(collected) => observations.extend(collected),
                Err((state, detail)) => {
                    observations.push((collector.unobserved)(
                        integration_id,
                        provider_id,
                        now,
                        state,
                    ));
                    // An absent or rejected credential is an Unauthenticated
                    // observation, not a collection failure - the integration
                    // may simply be unconfigured. Transport, shape, and
                    // unknown-collector failures are collection errors.
                    if state != ObservationState::Unauthenticated {
                        errors.push(integration_error(provider_id, detail));
                    }
                }
            }
        }
        (observations, errors)
    }
}

fn integration_error(provider_id: &aifuel_core::ProviderId, message: String) -> StatusError {
    StatusError {
        provider_id: Some(provider_id.as_str().to_owned()),
        account_id: None,
        code: StatusErrorCode::CollectionFailed,
        message,
    }
}

impl StatusCollector for ProviderMonitoring {
    fn collect_status(&self) -> Pin<Box<dyn Future<Output = StatusReport> + Send + '_>> {
        Box::pin(self.collect_live())
    }
}
