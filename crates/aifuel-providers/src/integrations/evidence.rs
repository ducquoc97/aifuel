//! Discovery evidence for Provider Integrations.
//!
//! Per `docs/specs/provider-integrations.md` ("Discovery Changes"), the
//! integration model adds three source kinds to the existing provider-owned
//! file and directory markers:
//!
//! - [`EvidenceSource::EnvVar`]: a declared environment variable is set and
//!   non-empty. The value is never read into reports.
//! - [`EvidenceSource::ManagedEntry`]: a [`CredentialRef`] is present in the
//!   Credential Store, observed through the metadata-only read only.
//! - [`EvidenceSource::ConfiguredEndpoint`]: a `providers.json` entry exists
//!   for the integration, or a provider-owned marker directory such as
//!   `~/.ollama` or `~/.lmstudio` is present.
//!
//! All checks are local, fast, and side-effect-free per CONTEXT.md Provider
//! Discovery: nothing here opens a socket, refreshes a token, writes state,
//! or reads credential content.

use crate::credentials::CredentialStore;
use crate::discovery::{DiscoveryContext, SourceKind};
use aifuel_core::{
    ApiKeySource, AuthBinding, CredentialRef, DiscoveryError, DiscoveryState, IntegrationId,
};
use std::collections::BTreeSet;

/// One local source of presence evidence for a Provider Integration.
///
/// A source answers "is there local evidence this integration is set up?",
/// never "is the endpoint reachable?". Reachability and credential validity
/// are collect-time concerns; discovery reports presence only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceSource {
    /// A provider-owned marker file relative to the home directory, the same
    /// evidence kind the existing catalog providers use.
    File(String),
    /// A provider-owned marker directory relative to the home directory.
    Directory(String),
    /// A declared environment variable that is set and non-empty. Presence
    /// uses the same check as credential resolution, so evidence and
    /// resolution cannot disagree.
    EnvVar(String),
    /// A Credential Reference present in the Credential Store, checked via
    /// [`CredentialStore::metadata`] - presence, kind, and expiry only, never
    /// secret material.
    ManagedEntry(CredentialRef),
    /// A `providers.json` entry exists for this integration, or one of the
    /// provider-owned marker directories (for example `~/.ollama`,
    /// `~/.lmstudio`) is present under the home directory. The config-entry
    /// check fires for config-defined integrations; marker directories
    /// evidence endpoints whose presence is established by a provider-owned
    /// install rather than by AI Fuel config.
    ConfiguredEndpoint { marker_directories: Vec<String> },
}

/// The local facts an evidence check may inspect.
///
/// Constructed by the caller (or [`crate::integrations::IntegrationRegistry`])
/// at the discovery boundary. Every field is borrowed read-only.
#[derive(Debug, Clone, Copy)]
pub struct EvidenceContext<'a> {
    /// Home-relative filesystem checks, shared with catalog discovery.
    pub discovery: &'a DiscoveryContext,
    /// The Credential Store rooted at the AI Fuel config directory. The
    /// metadata-only read is used; a missing `credentials.json` reads as an
    /// empty store.
    pub credentials: &'a CredentialStore,
    /// The Integration Identities that carry a `providers.json` entry.
    pub configured: &'a BTreeSet<IntegrationId>,
}

impl EvidenceSource {
    /// The stable evidence-kind name, for reporting.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::File(_) => "file",
            Self::Directory(_) => "directory",
            Self::EnvVar(_) => "env_var",
            Self::ManagedEntry(_) => "managed_entry",
            Self::ConfiguredEndpoint { .. } => "configured_endpoint",
        }
    }

    /// Inspect this one source for `integration` without side effects.
    pub fn inspect(
        &self,
        integration: &IntegrationId,
        context: &EvidenceContext<'_>,
    ) -> Result<DiscoveryState, DiscoveryError> {
        match self {
            Self::File(relative) => context.discovery.inspect_source(relative, SourceKind::File),
            Self::Directory(relative) => context
                .discovery
                .inspect_source(relative, SourceKind::Directory),
            Self::EnvVar(var) => Ok(if crate::env_override(var).is_some() {
                DiscoveryState::Present
            } else {
                // Discovery presence shares the store's env semantics: a
                // variable that is unset, empty, or non-Unicode is equally
                // unusable as credential material, so evidence and
                // resolution cannot disagree about "present".
                DiscoveryState::Absent
            }),
            // A pool whose base member was removed but that still holds
            // `reference/…` members resolves normally, so presence counts
            // the whole pool, not only the base slot.
            Self::ManagedEntry(reference) => {
                match context.credentials.contains_credential(reference) {
                    Ok(true) => Ok(DiscoveryState::Present),
                    Ok(false) => Ok(DiscoveryState::Absent),
                    // A store that cannot be read (for example a malformed
                    // credentials.json) means presence cannot be determined:
                    // report a Discovery Failure rather than guessing.
                    Err(_) => Err(DiscoveryError::SourceUnavailable),
                }
            }
            Self::ConfiguredEndpoint { marker_directories } => {
                if context.configured.contains(integration) {
                    return Ok(DiscoveryState::Present);
                }
                let markers: Vec<Self> = marker_directories
                    .iter()
                    .map(|relative| Self::Directory(relative.clone()))
                    .collect();
                inspect_any(&markers, integration, context)
            }
        }
    }
}

/// Inspect alternative sources: the first Present source wins. An
/// uninspectable source is recorded and inspection continues, mirroring
/// [`DiscoveryContext::inspect_any`]: when nothing is present, one recorded
/// failure surfaces instead of a bare Absent.
pub fn inspect_any<'a, I>(
    sources: I,
    integration: &IntegrationId,
    context: &EvidenceContext<'_>,
) -> Result<DiscoveryState, DiscoveryError>
where
    I: IntoIterator<Item = &'a EvidenceSource>,
{
    let mut failure = None;
    for source in sources {
        match source.inspect(integration, context) {
            Ok(DiscoveryState::Present) => return Ok(DiscoveryState::Present),
            Ok(DiscoveryState::Absent) => {}
            Err(error) => failure = Some(failure.unwrap_or(error)),
        }
    }

    match failure {
        Some(error) => Err(error),
        None => Ok(DiscoveryState::Absent),
    }
}

/// The evidence an Authentication Binding implies: an env-var source is
/// evidenced by the variable's presence, a store or OAuth source by the
/// Credential Reference's presence, and an env-or-store source by both.
/// Endpoint and CLI marker evidence are attached separately by the
/// descriptor's owner.
pub(crate) fn auth_sources(auth: &AuthBinding) -> Vec<EvidenceSource> {
    match auth {
        AuthBinding::None => Vec::new(),
        AuthBinding::ApiKey { source, .. } => match source {
            ApiKeySource::Env { var } => vec![EvidenceSource::EnvVar(var.clone())],
            ApiKeySource::Store { credential } => {
                vec![EvidenceSource::ManagedEntry(credential.clone())]
            }
            ApiKeySource::EnvOrStore { var, credential } => vec![
                EvidenceSource::EnvVar(var.clone()),
                EvidenceSource::ManagedEntry(credential.clone()),
            ],
        },
        AuthBinding::OAuth { credential, .. } => {
            vec![EvidenceSource::ManagedEntry(credential.clone())]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::builtin::builtin_integrations;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_HOME: AtomicU64 = AtomicU64::new(0);

    struct TestHome {
        path: PathBuf,
    }

    impl TestHome {
        fn new() -> Self {
            let suffix = NEXT_HOME.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aifuel-evidence-test-{}-{}",
                std::process::id(),
                suffix
            ));
            fs::create_dir_all(&path).expect("test home should be creatable");
            Self { path }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn integration(id: &str) -> IntegrationId {
        IntegrationId::new(id)
    }

    fn context<'a>(
        discovery: &'a DiscoveryContext,
        credentials: &'a CredentialStore,
        configured: &'a BTreeSet<IntegrationId>,
    ) -> EvidenceContext<'a> {
        EvidenceContext {
            discovery,
            credentials,
            configured,
        }
    }

    #[test]
    fn env_var_evidence_is_presence_only() {
        let home = TestHome::new();
        let discovery = DiscoveryContext::new(&home.path);
        let credentials = CredentialStore::new(home.path.join("aifuel"));
        let configured = BTreeSet::new();
        let context = context(&discovery, &credentials, &configured);
        let var = "AIFUEL_EVIDENCE_TEST_VAR";
        unsafe { env::remove_var(var) };

        let source = EvidenceSource::EnvVar(var.to_owned());
        assert_eq!(
            source.inspect(&integration("test"), &context),
            Ok(DiscoveryState::Absent)
        );

        // An empty variable is not evidence of a credential.
        unsafe { env::set_var(var, "") };
        assert_eq!(
            source.inspect(&integration("test"), &context),
            Ok(DiscoveryState::Absent)
        );

        unsafe { env::set_var(var, "any-value") };
        assert_eq!(
            source.inspect(&integration("test"), &context),
            Ok(DiscoveryState::Present)
        );
        unsafe { env::remove_var(var) };
    }

    #[test]
    fn managed_entry_reports_store_presence_without_reading_material() {
        let home = TestHome::new();
        let discovery = DiscoveryContext::new(&home.path);
        let credentials = CredentialStore::new(home.path.join("aifuel"));
        let configured = BTreeSet::new();
        let context = context(&discovery, &credentials, &configured);
        let reference = CredentialRef::new("work-anthropic");
        let source = EvidenceSource::ManagedEntry(reference.clone());

        // No credentials.json yet: an absent store reads as absent, never an error.
        assert_eq!(
            source.inspect(&integration("test"), &context),
            Ok(DiscoveryState::Absent)
        );

        credentials
            .set_api_key(&reference, "secret-material")
            .expect("credential should be storable");
        assert_eq!(
            source.inspect(&integration("test"), &context),
            Ok(DiscoveryState::Present)
        );
    }

    #[test]
    fn managed_entry_on_a_corrupt_store_is_a_discovery_failure_not_absent() {
        let home = TestHome::new();
        let discovery = DiscoveryContext::new(&home.path);
        let credentials = CredentialStore::new(home.path.join("aifuel"));
        let configured = BTreeSet::new();
        let context = context(&discovery, &credentials, &configured);
        fs::create_dir_all(home.path.join("aifuel")).expect("store dir should be creatable");
        fs::write(home.path.join("aifuel/credentials.json"), b"{not json")
            .expect("corrupt store should be writable");

        let source = EvidenceSource::ManagedEntry(CredentialRef::new("any"));
        assert_eq!(
            source.inspect(&integration("test"), &context),
            Err(DiscoveryError::SourceUnavailable)
        );
    }

    #[test]
    fn configured_endpoint_accepts_a_config_entry_or_marker_directory() {
        let home = TestHome::new();
        let discovery = DiscoveryContext::new(&home.path);
        let credentials = CredentialStore::new(home.path.join("aifuel"));
        let configured = BTreeSet::from([integration("ollama:local")]);
        let source = EvidenceSource::ConfiguredEndpoint {
            marker_directories: vec![".ollama".to_owned()],
        };

        // A providers.json entry alone evidences the endpoint.
        assert_eq!(
            source.inspect(
                &integration("ollama:local"),
                &context(&discovery, &credentials, &configured)
            ),
            Ok(DiscoveryState::Present)
        );

        // Without a config entry the marker directory decides.
        let unconfigured = BTreeSet::new();
        let context = context(&discovery, &credentials, &unconfigured);
        assert_eq!(
            source.inspect(&integration("ollama:local"), &context),
            Ok(DiscoveryState::Absent)
        );
        fs::create_dir_all(home.path.join(".ollama")).expect("marker should be creatable");
        assert_eq!(
            source.inspect(&integration("ollama:local"), &context),
            Ok(DiscoveryState::Present)
        );
    }

    #[test]
    fn file_and_directory_sources_match_existing_marker_semantics() {
        let home = TestHome::new();
        let discovery = DiscoveryContext::new(&home.path);
        let credentials = CredentialStore::new(home.path.join("aifuel"));
        let configured = BTreeSet::new();
        let context = context(&discovery, &credentials, &configured);
        fs::create_dir_all(home.path.join(".claude")).expect("marker parent should be creatable");
        fs::write(
            home.path.join(".claude/.credentials.json"),
            b"malformed but present",
        )
        .expect("marker should be writable");

        let file = EvidenceSource::File(".claude/.credentials.json".to_owned());
        assert_eq!(
            file.inspect(&integration("claude:cli"), &context),
            Ok(DiscoveryState::Present)
        );

        let directory = EvidenceSource::Directory(".claude/.credentials.json".to_owned());
        assert_eq!(
            directory.inspect(&integration("claude:cli"), &context),
            Err(DiscoveryError::UnexpectedSourceType)
        );
    }

    #[test]
    fn inspect_any_records_one_failure_when_nothing_is_present() {
        let home = TestHome::new();
        let discovery = DiscoveryContext::new(&home.path);
        let credentials = CredentialStore::new(home.path.join("aifuel"));
        let configured = BTreeSet::new();
        let context = context(&discovery, &credentials, &configured);
        let sources = [
            EvidenceSource::Directory("missing-dir".to_owned()),
            EvidenceSource::File("missing-file".to_owned()),
            // A file where a directory is expected fails the whole check when
            // no source is present, so a broken marker is never silent.
            EvidenceSource::Directory("marker-file".to_owned()),
        ];
        fs::write(
            home.path.join("marker-file"),
            b"file where directory is expected",
        )
        .expect("marker should be writable");

        assert_eq!(
            inspect_any(&sources, &integration("test"), &context),
            Err(DiscoveryError::UnexpectedSourceType)
        );
    }

    #[test]
    fn builtin_descriptors_expose_local_evidence_sources() {
        // Every builtin carries at least one local presence source, and the
        // env-key builtins evidence the declared variable by name. None of
        // these checks can contact a provider API: they are filesystem,
        // environment, and credential-store-metadata reads only.
        let descriptors = builtin_integrations();
        for descriptor in &descriptors {
            assert!(
                !descriptor.sources.is_empty(),
                "{} has no discovery evidence",
                descriptor.integration.id
            );
        }

        let openai = descriptors
            .iter()
            .find(|descriptor| descriptor.integration.id.as_str() == "openai:api-key")
            .expect("openai:api-key is a builtin");
        assert_eq!(
            openai.sources,
            vec![
                EvidenceSource::EnvVar("OPENAI_API_KEY".to_owned()),
                // The env-or-store binding also evidences the managed
                // credential's presence in the store.
                EvidenceSource::ManagedEntry(CredentialRef::new("openai:api-key")),
            ]
        );
    }
}
