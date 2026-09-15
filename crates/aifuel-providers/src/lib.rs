//! Built-in provider discovery and initialization.
//!
//! This crate only inspects provider-owned source metadata in this slice. It
//! does not parse credentials, refresh tokens, contact provider APIs, or write
//! user state. Quota adapters are added by later implementation issues.

mod discovery;

mod antigravity;
mod claude;
mod codex;
mod copilot;
mod gemini;

pub use discovery::{DiscoveryContext, DiscoveryContextError};

use aifuel_core::{
    DiscoveryError, DiscoveryFailure, DiscoveryReport, DiscoveryState, ProviderDescriptor,
};

/// A provider object that was initialized after its source was discovered.
///
/// The object carries identity only until its provider-specific quota adapter
/// is implemented. Constructing one has no network or credential side effect.
pub trait ProviderAdapter: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
}

/// A static Supported Provider definition.
pub trait ProviderDefinition: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    fn discover(&self, context: &DiscoveryContext) -> Result<DiscoveryState, DiscoveryError>;
    fn initialize(&self) -> Box<dyn ProviderAdapter>;
}

/// The result of discovery plus the adapters created for present sources.
pub struct InitializedProviders {
    adapters: Vec<Box<dyn ProviderAdapter>>,
    report: DiscoveryReport,
}

impl InitializedProviders {
    pub fn report(&self) -> &DiscoveryReport {
        &self.report
    }

    pub fn adapters(&self) -> impl Iterator<Item = &dyn ProviderAdapter> {
        self.adapters.iter().map(Box::as_ref)
    }

    pub fn len(&self) -> usize {
        self.adapters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }
}

/// A registry over an explicit catalog supplied by the application.
pub struct ProviderRegistry<'a> {
    definitions: &'a [&'a dyn ProviderDefinition],
}

impl<'a> ProviderRegistry<'a> {
    pub const fn new(definitions: &'a [&'a dyn ProviderDefinition]) -> Self {
        Self { definitions }
    }

    /// Recompute discovery and initialize only the current Discovered Providers.
    pub fn discover_and_initialize(&self, context: &DiscoveryContext) -> InitializedProviders {
        let mut report = DiscoveryReport::new();
        let mut adapters = Vec::new();

        for definition in self.definitions {
            let descriptor = definition.descriptor();
            match definition.discover(context) {
                Ok(DiscoveryState::Present) => {
                    report.providers.push(descriptor);
                    adapters.push(definition.initialize());
                }
                Ok(DiscoveryState::Absent) => {}
                Err(error) => report
                    .discovery_errors
                    .push(DiscoveryFailure::from_error(descriptor, error)),
            }
        }

        InitializedProviders { adapters, report }
    }
}

/// The explicit catalog of the five currently supported provider identities.
pub static SUPPORTED_PROVIDERS: &[&dyn ProviderDefinition] = &[
    &claude::DEFINITION,
    &codex::DEFINITION,
    &copilot::DEFINITION,
    &gemini::DEFINITION,
    &antigravity::DEFINITION,
];

pub fn default_registry() -> ProviderRegistry<'static> {
    ProviderRegistry::new(SUPPORTED_PROVIDERS)
}

struct InitializedAdapter {
    descriptor: ProviderDescriptor,
}

impl ProviderAdapter for InitializedAdapter {
    fn descriptor(&self) -> ProviderDescriptor {
        self.descriptor
    }
}

fn initialized_adapter(descriptor: ProviderDescriptor) -> Box<dyn ProviderAdapter> {
    Box::new(InitializedAdapter { descriptor })
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
            .adapters()
            .map(|provider| provider.descriptor().key)
            .collect()
    }

    #[test]
    fn catalog_is_explicit_and_keeps_the_five_provider_identities() {
        let keys: Vec<_> = SUPPORTED_PROVIDERS
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
    fn malformed_source_contents_are_not_read_during_discovery() {
        let home = TestHome::new();
        home.write_file(".copilot/config.json", &[0, 159, 146, 150]);

        let selection = default_registry().discover_and_initialize(&home.context());

        assert_eq!(keys(&selection), vec![ProviderKey::Copilot]);
    }

    struct ControlledDefinition {
        descriptor: ProviderDescriptor,
        state: Result<DiscoveryState, DiscoveryError>,
        initialized: Arc<AtomicUsize>,
    }

    impl ProviderDefinition for ControlledDefinition {
        fn descriptor(&self) -> ProviderDescriptor {
            self.descriptor
        }

        fn discover(&self, _context: &DiscoveryContext) -> Result<DiscoveryState, DiscoveryError> {
            self.state
        }

        fn initialize(&self) -> Box<dyn ProviderAdapter> {
            self.initialized.fetch_add(1, Ordering::Relaxed);
            initialized_adapter(self.descriptor)
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
        let definitions: [&dyn ProviderDefinition; 3] = [&present, &absent, &failed];
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
        assert!(first.is_empty());

        home.write_file(".gemini/oauth_creds.json", b"not parsed");
        let second = default_registry().discover_and_initialize(&home.context());
        assert_eq!(keys(&second), vec![ProviderKey::Gemini]);

        fs::remove_file(home.path.join(".gemini/oauth_creds.json"))
            .expect("test marker should be removable");
        let third = default_registry().discover_and_initialize(&home.context());
        assert!(third.is_empty());
    }
}
