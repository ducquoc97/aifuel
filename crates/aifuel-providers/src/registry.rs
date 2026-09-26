use crate::discovery::{DiscoveryContext, SourceKind};
use crate::monitoring::ProviderMonitoring;
use aifuel_core::{
    DiscoveryError, DiscoveryFailure, DiscoveryReport, DiscoveryState, ProviderDescriptor,
    ProviderKey, ProviderUsage,
};
use std::future::Future;
use std::pin::Pin;

pub(crate) type MonitoringFuture<'a> = Pin<Box<dyn Future<Output = ProviderUsage> + Send + 'a>>;
pub(crate) type ProviderCollector = for<'a> fn(&'a ProviderMonitoring) -> MonitoringFuture<'a>;

/// The provider-owned source markers used by a Catalog Provider definition.
enum CredentialSources {
    File(&'static str),
    Files(&'static [&'static str]),
    Directories(&'static [&'static str]),
}

/// A static Catalog Provider definition.
pub struct CatalogProvider {
    descriptor: ProviderDescriptor,
    sources: CredentialSources,
}

impl CatalogProvider {
    pub const fn file_source(key: aifuel_core::ProviderKey, source: &'static str) -> Self {
        Self {
            descriptor: ProviderDescriptor::for_key(key),
            sources: CredentialSources::File(source),
        }
    }

    pub const fn files_source(
        key: aifuel_core::ProviderKey,
        sources: &'static [&'static str],
    ) -> Self {
        Self {
            descriptor: ProviderDescriptor::for_key(key),
            sources: CredentialSources::Files(sources),
        }
    }

    pub const fn directory_sources(
        key: aifuel_core::ProviderKey,
        sources: &'static [&'static str],
    ) -> Self {
        Self {
            descriptor: ProviderDescriptor::for_key(key),
            sources: CredentialSources::Directories(sources),
        }
    }
}

impl CatalogProviderDefinition for CatalogProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        self.descriptor
    }

    fn discover(&self, context: &DiscoveryContext) -> Result<DiscoveryState, DiscoveryError> {
        match self.sources {
            CredentialSources::File(source) => context.inspect_source(source, SourceKind::File),
            CredentialSources::Files(sources) => context.inspect_any(
                sources
                    .iter()
                    .copied()
                    .map(|source| (source, SourceKind::File)),
            ),
            CredentialSources::Directories(sources) => context.inspect_any(
                sources
                    .iter()
                    .copied()
                    .map(|source| (source, SourceKind::Directory)),
            ),
        }
    }

    fn initialize(&self) -> InitializedProvider {
        initialized_provider(self.descriptor)
    }
}

/// A Catalog Provider object initialized after its source was discovered.
///
/// This handle records which Catalog Provider passed discovery without reading
/// or retaining credential content. Capability registries decide what it can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitializedProvider {
    descriptor: ProviderDescriptor,
}

impl InitializedProvider {
    pub fn descriptor(&self) -> ProviderDescriptor {
        self.descriptor
    }
}

/// A static Catalog Provider definition.
///
/// The shared implementation keeps this discovery seam focused on the
/// provider lifecycle. It does not expose quota or execution behavior.
pub trait CatalogProviderDefinition: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    fn discover(&self, context: &DiscoveryContext) -> Result<DiscoveryState, DiscoveryError>;
    fn initialize(&self) -> InitializedProvider;
}

/// The result of discovery plus the initialized provider identities for present
/// sources.
///
/// The concrete handles make the initialization boundary observable now, while
/// keeping quota collection outside this issue.
pub struct InitializedProviders {
    providers: Vec<InitializedProvider>,
    report: DiscoveryReport,
}

impl InitializedProviders {
    pub fn report(&self) -> &DiscoveryReport {
        &self.report
    }

    pub fn providers(&self) -> impl Iterator<Item = &InitializedProvider> {
        self.providers.iter()
    }
}

/// A registry over an explicit Catalog Provider catalog supplied by the application.
pub struct ProviderRegistry<'a> {
    definitions: &'a [&'a dyn CatalogProviderDefinition],
}

/// One provider-specific implementation of the monitoring capability.
struct MonitoringAdapter {
    provider: ProviderKey,
    collect: ProviderCollector,
}

/// Registry for the optional monitoring capability. It is intentionally
/// separate from the Catalog Provider registry so catalog identity and
/// Provider Discovery do not imply monitoring support.
pub(crate) struct MonitoringRegistry {
    adapters: &'static [MonitoringAdapter],
}

impl MonitoringRegistry {
    const fn new(adapters: &'static [MonitoringAdapter]) -> Self {
        Self { adapters }
    }

    pub(crate) fn collect<'a>(
        &self,
        provider: InitializedProvider,
        monitoring: &'a ProviderMonitoring,
    ) -> MonitoringFuture<'a> {
        if let Some(adapter) = self
            .adapters
            .iter()
            .find(|adapter| adapter.provider == provider.descriptor().key)
        {
            return (adapter.collect)(monitoring);
        }
        let key = provider.descriptor().key;
        Box::pin(async move { ProviderUsage::error(key, "provider has no monitoring capability") })
    }
}

impl<'a> ProviderRegistry<'a> {
    pub const fn new(definitions: &'a [&'a dyn CatalogProviderDefinition]) -> Self {
        Self { definitions }
    }

    /// Recompute discovery and initialize only the current Discovered Providers.
    pub fn discover_and_initialize(&self, context: &DiscoveryContext) -> InitializedProviders {
        let mut report = DiscoveryReport::new();
        let mut providers = Vec::new();

        for definition in self.definitions {
            let descriptor = definition.descriptor();
            match definition.discover(context) {
                Ok(DiscoveryState::Present) => {
                    report.providers.push(descriptor);
                    providers.push(definition.initialize());
                }
                Ok(DiscoveryState::Absent) => {}
                Err(error) => report
                    .discovery_errors
                    .push(DiscoveryFailure::from_error(descriptor, error)),
            }
        }

        InitializedProviders { providers, report }
    }
}

/// The explicit catalog of the six current Catalog Provider identities.
pub static CATALOG_PROVIDERS: &[&dyn CatalogProviderDefinition] = &[
    &crate::claude::DEFINITION,
    &crate::codex::DEFINITION,
    &crate::copilot::DEFINITION,
    &crate::gemini::DEFINITION,
    &crate::antigravity::DEFINITION,
    &crate::devin::DEFINITION,
];

static MONITORING_ADAPTERS: &[MonitoringAdapter] = &[
    MonitoringAdapter {
        provider: ProviderKey::Claude,
        collect: crate::claude::collect,
    },
    MonitoringAdapter {
        provider: ProviderKey::Codex,
        collect: crate::codex::collect,
    },
    MonitoringAdapter {
        provider: ProviderKey::Copilot,
        collect: crate::copilot::collect,
    },
    MonitoringAdapter {
        provider: ProviderKey::Gemini,
        collect: crate::gemini::collect,
    },
    MonitoringAdapter {
        provider: ProviderKey::Antigravity,
        collect: crate::antigravity::collect,
    },
    MonitoringAdapter {
        provider: ProviderKey::Devin,
        collect: crate::devin::collect,
    },
];

pub(crate) fn default_registry() -> ProviderRegistry<'static> {
    ProviderRegistry::new(CATALOG_PROVIDERS)
}

pub(crate) fn default_monitoring_registry() -> MonitoringRegistry {
    MonitoringRegistry::new(MONITORING_ADAPTERS)
}

pub(crate) fn initialized_provider(descriptor: ProviderDescriptor) -> InitializedProvider {
    InitializedProvider { descriptor }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aifuel_core::{ProviderDescriptor, ProviderKey};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    static NEXT_HOME: AtomicU64 = AtomicU64::new(0);

    struct TestHome {
        path: PathBuf,
    }

    impl TestHome {
        fn new() -> Self {
            let suffix = NEXT_HOME.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aifuel-discovery-test-{}-{}",
                std::process::id(),
                suffix
            ));
            fs::create_dir_all(&path).expect("test home should be creatable");
            Self { path }
        }

        fn context(&self) -> DiscoveryContext {
            DiscoveryContext::new(&self.path)
        }

        fn write_file(&self, relative: &str, contents: &[u8]) {
            let path = self.path.join(relative);
            fs::create_dir_all(path.parent().expect("marker has a parent"))
                .expect("marker parent should be creatable");
            fs::write(path, contents).expect("marker should be writable");
        }

        fn create_directory(&self, relative: &str) {
            fs::create_dir_all(self.path.join(relative)).expect("marker should be creatable");
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn keys(selection: &InitializedProviders) -> Vec<ProviderKey> {
        selection
            .providers()
            .map(|provider| provider.descriptor().key)
            .collect()
    }

    #[test]
    fn catalog_is_explicit_and_keeps_the_six_provider_identities() {
        let keys: Vec<_> = CATALOG_PROVIDERS
            .iter()
            .map(|provider| provider.descriptor().key)
            .collect();

        assert_eq!(
            keys,
            vec![
                ProviderKey::Claude,
                ProviderKey::Codex,
                ProviderKey::Copilot,
                ProviderKey::Gemini,
                ProviderKey::Antigravity,
                ProviderKey::Devin,
            ]
        );
    }

    #[test]
    fn each_provider_requires_its_own_source_marker() {
        let cases = [
            (ProviderKey::Claude, ".claude/.credentials.json"),
            (ProviderKey::Codex, ".codex/auth.json"),
            (ProviderKey::Copilot, ".copilot/config.json"),
            (ProviderKey::Gemini, ".gemini/oauth_creds.json"),
        ];

        for (key, marker) in cases {
            let home = TestHome::new();
            home.write_file(marker, b"malformed but present");

            let selection = default_registry().discover_and_initialize(&home.context());

            assert_eq!(keys(&selection), vec![key]);
            assert!(selection.report().discovery_errors.is_empty());
        }
    }

    #[test]
    fn antigravity_accepts_either_provider_owned_directory() {
        for marker in [".gemini/antigravity", ".gemini/antigravity-cli"] {
            let home = TestHome::new();
            home.create_directory(marker);

            let selection = default_registry().discover_and_initialize(&home.context());

            assert_eq!(keys(&selection), vec![ProviderKey::Antigravity]);
        }
    }

    #[test]
    fn devin_accepts_any_platform_credential_file() {
        for marker in [
            ".local/share/devin/credentials.toml",
            "Library/Application Support/devin/credentials.toml",
            "AppData/Roaming/devin/credentials.toml",
        ] {
            let home = TestHome::new();
            home.write_file(marker, b"present but unparsed");

            let selection = default_registry().discover_and_initialize(&home.context());

            assert_eq!(keys(&selection), vec![ProviderKey::Devin]);
        }
    }

    #[test]
    fn malformed_source_contents_are_not_read_during_discovery() {
        let home = TestHome::new();
        home.write_file(".copilot/config.json", &[0, 159, 146, 150]);

        let selection = default_registry().discover_and_initialize(&home.context());

        assert_eq!(keys(&selection), vec![ProviderKey::Copilot]);
    }

    #[test]
    fn an_uninspectable_source_is_reported_as_a_discovery_failure() {
        let home = TestHome::new();
        let gemini = CatalogProvider::file_source(ProviderKey::Gemini, "\0");
        let antigravity = CatalogProvider::file_source(ProviderKey::Antigravity, "\0");
        let definitions: [&dyn CatalogProviderDefinition; 2] = [&gemini, &antigravity];

        let selection =
            ProviderRegistry::new(&definitions).discover_and_initialize(&home.context());
        let failures: Vec<_> = selection
            .report()
            .discovery_errors
            .iter()
            .map(|failure| failure.provider.key)
            .collect();

        assert!(selection.providers().next().is_none());
        assert_eq!(
            failures,
            vec![ProviderKey::Gemini, ProviderKey::Antigravity]
        );
    }

    #[test]
    fn source_shape_errors_are_reported_without_initializing_the_provider() {
        let home = TestHome::new();
        home.create_directory(".copilot/config.json");
        home.write_file(".gemini/antigravity", b"file where directory is expected");

        let selection = default_registry().discover_and_initialize(&home.context());
        let failures: Vec<_> = selection
            .report()
            .discovery_errors
            .iter()
            .map(|failure| failure.provider.key)
            .collect();

        assert!(selection.providers().next().is_none());
        assert_eq!(
            failures,
            vec![ProviderKey::Copilot, ProviderKey::Antigravity]
        );
    }

    struct ControlledDefinition {
        descriptor: ProviderDescriptor,
        state: Result<DiscoveryState, DiscoveryError>,
        initialized: Arc<AtomicUsize>,
    }

    impl CatalogProviderDefinition for ControlledDefinition {
        fn descriptor(&self) -> ProviderDescriptor {
            self.descriptor
        }

        fn discover(&self, _context: &DiscoveryContext) -> Result<DiscoveryState, DiscoveryError> {
            self.state
        }

        fn initialize(&self) -> InitializedProvider {
            self.initialized.fetch_add(1, Ordering::Relaxed);
            initialized_provider(self.descriptor)
        }
    }

    #[test]
    fn only_present_sources_are_initialized_and_failures_are_separate() {
        let present_count = Arc::new(AtomicUsize::new(0));
        let absent_count = Arc::new(AtomicUsize::new(0));
        let failed_count = Arc::new(AtomicUsize::new(0));
        let present = ControlledDefinition {
            descriptor: ProviderDescriptor::for_key(ProviderKey::Codex),
            state: Ok(DiscoveryState::Present),
            initialized: Arc::clone(&present_count),
        };
        let absent = ControlledDefinition {
            descriptor: ProviderDescriptor::for_key(ProviderKey::Claude),
            state: Ok(DiscoveryState::Absent),
            initialized: Arc::clone(&absent_count),
        };
        let failed = ControlledDefinition {
            descriptor: ProviderDescriptor::for_key(ProviderKey::Gemini),
            state: Err(DiscoveryError::SourceUnavailable),
            initialized: Arc::clone(&failed_count),
        };
        let definitions: [&dyn CatalogProviderDefinition; 3] = [&present, &absent, &failed];
        let selection = ProviderRegistry::new(&definitions)
            .discover_and_initialize(&DiscoveryContext::new(Path::new("/unused")));

        assert_eq!(keys(&selection), vec![ProviderKey::Codex]);
        assert_eq!(selection.report().providers.len(), 1);
        assert_eq!(selection.report().discovery_errors.len(), 1);
        assert_eq!(
            selection.report().discovery_errors[0].provider.key,
            ProviderKey::Gemini
        );
        assert_eq!(present_count.load(Ordering::Relaxed), 1);
        assert_eq!(absent_count.load(Ordering::Relaxed), 0);
        assert_eq!(failed_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn discovery_is_recomputed_for_each_collection() {
        let home = TestHome::new();
        let first = default_registry().discover_and_initialize(&home.context());
        assert!(first.providers().next().is_none());

        home.write_file(".gemini/oauth_creds.json", b"not parsed");
        let second = default_registry().discover_and_initialize(&home.context());
        assert_eq!(keys(&second), vec![ProviderKey::Gemini]);

        fs::remove_file(home.path.join(".gemini/oauth_creds.json"))
            .expect("test marker should be removable");
        let third = default_registry().discover_and_initialize(&home.context());
        assert!(third.providers().next().is_none());
    }
}
