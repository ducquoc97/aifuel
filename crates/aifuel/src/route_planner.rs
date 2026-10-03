//! The reusable route planner: the ranked candidate chain `run --provider
//! auto` and the inbound `/v1` gateway share. Populated by the extraction
//! of `run_cli/auto.rs`'s routing logic.

use aifuel_core::{
    ApiKeySource, AuthBinding, ExecutionConfig, IntegrationId, ProviderId, ProviderKey,
    StatusReport,
};
use std::collections::BTreeSet;

/// The evidence a chain candidate's position rests on, reported on the
/// `routing` object so the ranking is explainable rather than positional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteBasis {
    /// Ranked by a quota observation - `remaining_percent` records the
    /// measured headroom (absent when collection reported nothing).
    Quota,
    /// A keyed API-key integration whose catalog provider documents a
    /// free tier.
    FreeTier,
    /// A keyed API-key integration with no documented free tier.
    ApiKey,
    /// Declared by a `providers.json` chain step - position comes from the
    /// file, not evidence.
    Chain,
}

impl RouteBasis {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Quota => "quota",
            Self::FreeTier => "free_tier",
            Self::ApiKey => "api_key",
            Self::Chain => "chain",
        }
    }
}

/// One ranked chain entry: the Provider its attempt reports, the
/// Integration Identity it binds, and the basis that placed it.
pub(crate) struct RouteCandidate {
    pub(crate) provider: ProviderId,
    pub(crate) integration: IntegrationId,
    pub(crate) basis: RouteBasis,
    /// The measured remaining-allowance percent a `quota` candidate
    /// ranked on; `None` for unmeasured quota providers and for
    /// credential-ranked candidates.
    pub(crate) remaining_percent: Option<f64>,
    /// A chain step's model override; `None` keeps the run's `--model`.
    pub(crate) model: Option<String>,
}

/// The routing chain for `auto`, ordered by the evidence each candidate
/// rests on:
///
/// 1. Discovered Providers whose quota observation reports positive
///    headroom, in `StatusReport::route_candidates` order.
/// 2. API-key integrations whose credential is present - the declared
///    environment variable set, or a managed credential (or pool member)
///    stored - ranked catalog order, providers with a documented free
///    tier ahead of paid keys.
/// 3. Discovered Providers with no usable headroom (exhausted or
///    unmeasured quota evidence) last; stale evidence may still resolve
///    to a working run.
///
/// `--model` keeps only providers whose cached model catalog advertises
/// the model - API-key candidates carry no catalog advertisement, so a
/// model filter narrows to the quota-ranked catalog providers.
pub(crate) fn provider_candidates(
    model: Option<&str>,
    report: &StatusReport,
    registry: &aifuel_providers::IntegrationRegistry,
    credentials: &aifuel_providers::CredentialStore,
) -> Result<Vec<RouteCandidate>, String> {
    // `route_candidates` already orders headroom-positive providers ahead
    // of the no-headroom tail; the measured remaining percentage re-exposes
    // that boundary so keyed integrations can slot between the two groups.
    let remaining_of = |key: ProviderKey| {
        report
            .providers
            .iter()
            .find(|usage| usage.key == key)
            .map(|usage| usage.effective_remaining())
            .filter(|remaining| *remaining >= 0.0)
    };
    let (measured, depleted): (Vec<ProviderKey>, Vec<ProviderKey>) = report
        .route_candidates()
        .into_iter()
        .partition(|key| remaining_of(*key).is_some_and(|remaining| remaining > 0.0));

    let mut candidates: Vec<RouteCandidate> = Vec::new();
    candidates.extend(
        measured
            .into_iter()
            .filter_map(|key| bind_provider(registry, key, remaining_of(key))),
    );
    let (free_tier, paid): (Vec<RouteCandidate>, Vec<RouteCandidate>) = registry
        .list()
        .filter_map(|descriptor| keyed_api_key(descriptor, credentials))
        .partition(|candidate| candidate.basis == RouteBasis::FreeTier);
    candidates.extend(free_tier);
    candidates.extend(paid);
    candidates.extend(
        depleted
            .into_iter()
            .filter_map(|key| bind_provider(registry, key, remaining_of(key))),
    );

    if let Some(model) = model {
        let advertised = advertised_providers(model)?;
        candidates.retain(|candidate| {
            candidate
                .provider
                .as_str()
                .parse::<ProviderKey>()
                .is_ok_and(|key| advertised.contains(&key))
        });
    }
    Ok(candidates)
}

/// Bind a quota-ranked Provider to its first registered Integration, in
/// deterministic registry order (built-ins precede configured entries).
fn bind_provider(
    registry: &aifuel_providers::IntegrationRegistry,
    key: ProviderKey,
    remaining_percent: Option<f64>,
) -> Option<RouteCandidate> {
    let provider = ProviderId::from(key);
    registry
        .list()
        .find(|descriptor| *descriptor.provider() == provider)
        .map(|descriptor| RouteCandidate {
            provider,
            integration: descriptor.id().clone(),
            basis: RouteBasis::Quota,
            remaining_percent,
            model: None,
        })
}

/// A registered API-key integration whose credential is present is a
/// candidate on credential evidence alone: it has no quota observation,
/// but its key can run. The basis is `free_tier` when the catalog
/// documents a free allowance for the provider, else `api_key`.
///
/// A cookie-delivered binding is never a candidate: it holds a
/// browser-session credential whose integration exists to monitor, and a
/// `*:web` protocol has no execution engine - candidacy would only
/// manufacture a doomed attempt.
fn keyed_api_key(
    descriptor: &aifuel_providers::IntegrationDescriptor,
    credentials: &aifuel_providers::CredentialStore,
) -> Option<RouteCandidate> {
    let ExecutionConfig::Http {
        auth: AuthBinding::ApiKey { source, delivery },
        ..
    } = &descriptor.integration.execution
    else {
        return None;
    };
    if matches!(delivery, aifuel_core::KeyDelivery::Cookie { .. }) {
        return None;
    }
    if !credential_present(source, credentials) {
        return None;
    }
    let basis = if aifuel_providers::free_tier_note(descriptor.provider().as_str()).is_some() {
        RouteBasis::FreeTier
    } else {
        RouteBasis::ApiKey
    };
    Some(RouteCandidate {
        provider: descriptor.integration.provider.clone(),
        integration: descriptor.integration.id.clone(),
        basis,
        remaining_percent: None,
        model: None,
    })
}

/// Whether an API-key source resolves to present material without reading
/// it - the declared environment variable is set, or a managed credential
/// (or any pool member under its Credential Reference) is stored. Mirrors
/// the evidence check `EvidenceSource::EnvVar`/`ManagedEntry` performs for
/// discovery: presence only, never content, and store errors read as
/// absent (the caller surfaces an unreadable store once).
fn credential_present(
    source: &ApiKeySource,
    credentials: &aifuel_providers::CredentialStore,
) -> bool {
    let stored = |credential: &aifuel_core::CredentialRef| {
        credentials.contains_credential(credential).unwrap_or(false)
    };
    match source {
        ApiKeySource::Env { var } => aifuel_providers::env_override(var).is_some(),
        ApiKeySource::Store { credential } => stored(credential),
        ApiKeySource::EnvOrStore { var, credential } => {
            aifuel_providers::env_override(var).is_some() || stored(credential)
        }
    }
}

/// Providers whose cached model catalog advertises `model`: the catalog
/// marks `advertisement` Supported only for a model the provider itself
/// listed, so a missing cache narrows to no provider rather than guessing.
pub(crate) fn advertised_providers(model: &str) -> Result<BTreeSet<ProviderKey>, String> {
    Ok(crate::run_selection::load_picker_models()?
        .into_iter()
        .filter(|entry| {
            entry.model_id == model
                && entry.advertisement == aifuel_core::CapabilityState::Supported
        })
        .map(|entry| entry.provider)
        .collect())
}

/// A bounded phrase list matching the ways providers spell quota and
/// rate-limit exhaustion in error text.
pub(crate) fn provider_quota_wording(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    [
        "rate limit",
        "rate-limit",
        "ratelimit",
        "usage limit",
        "quota",
        "too many requests",
        "insufficient_quota",
        "insufficient quota",
        "insufficient credits",
        "http 429",
        "status 429",
        " 429 ",
        "exceeded your current",
    ]
    .iter()
    .any(|phrase| text.contains(phrase))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store(name: &str) -> (std::path::PathBuf, aifuel_providers::CredentialStore) {
        let dir =
            std::env::temp_dir().join(format!("aifuel-auto-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test dir should be creatable");
        let store = aifuel_providers::CredentialStore::new(&dir);
        (dir, store)
    }

    fn api_key_descriptor(
        provider: &str,
        source: ApiKeySource,
    ) -> aifuel_providers::IntegrationDescriptor {
        aifuel_providers::IntegrationDescriptor::builtin(
            aifuel_core::Integration {
                id: IntegrationId::new(format!("{provider}:api-key")),
                provider: ProviderId::new(provider),
                name: provider.to_owned(),
                execution: ExecutionConfig::Http {
                    endpoint: aifuel_core::EndpointConfig {
                        base_url: "https://endpoint.test".to_owned(),
                        extra_headers: std::collections::BTreeMap::new(),
                        request_timeout_seconds: None,
                    },
                    protocol: aifuel_core::WireApi::OpenAiChat,
                    auth: AuthBinding::ApiKey {
                        source,
                        delivery: aifuel_core::KeyDelivery::Bearer,
                    },
                },
                monitoring: None,
            },
            Vec::new(),
        )
    }

    #[test]
    fn credential_presence_is_metadata_only_over_env_and_store() {
        // Candidacy mirrors the discovery evidence: a set env var or a
        // stored Managed Credential - including a pool member under the
        // bound Credential Reference - counts; nothing reads key material.
        let (dir, store) = test_store("presence");
        let reference = aifuel_core::CredentialRef::new("test:api-key");
        let source = ApiKeySource::EnvOrStore {
            var: "AIFUEL_TEST_CANDIDATE_KEY".to_owned(),
            credential: reference.clone(),
        };

        unsafe { std::env::remove_var("AIFUEL_TEST_CANDIDATE_KEY") };
        assert!(!credential_present(&source, &store));

        unsafe { std::env::set_var("AIFUEL_TEST_CANDIDATE_KEY", "sk-test") };
        assert!(credential_present(&source, &store));
        unsafe { std::env::remove_var("AIFUEL_TEST_CANDIDATE_KEY") };

        store
            .set_api_key(&reference, "sk-stored")
            .expect("store should accept the key");
        assert!(credential_present(&source, &store));

        // A pool member alone - no root credential - still reads as a
        // present credential, matching `contains_credential`.
        let pooled = aifuel_core::CredentialRef::new("test:pooled-key");
        store
            .set_api_key(
                &aifuel_core::CredentialRef::new("test:pooled-key/2"),
                "sk-2",
            )
            .expect("store should accept the pool member");
        assert!(credential_present(
            &ApiKeySource::Store { credential: pooled },
            &store
        ));

        std::fs::remove_dir_all(&dir).expect("test dir should be removable");
    }

    #[test]
    fn keyed_api_key_basis_follows_the_catalog_free_tier() {
        // A documented free tier outranks a paid key because free headroom
        // beats a billed call; an unkeyed integration is no candidate.
        let (dir, store) = test_store("basis");
        unsafe {
            std::env::set_var("AIFUEL_TEST_BASIS_KEY", "sk-test");
        }
        let source = || ApiKeySource::Env {
            var: "AIFUEL_TEST_BASIS_KEY".to_owned(),
        };

        let groq = keyed_api_key(&api_key_descriptor("groq", source()), &store)
            .expect("a keyed integration is a candidate");
        assert_eq!(groq.basis, RouteBasis::FreeTier);
        assert_eq!(groq.provider.as_str(), "groq");
        assert_eq!(groq.integration.as_str(), "groq:api-key");

        let paid = keyed_api_key(&api_key_descriptor("xai", source()), &store)
            .expect("a keyed integration is a candidate");
        assert_eq!(paid.basis, RouteBasis::ApiKey);

        unsafe {
            std::env::remove_var("AIFUEL_TEST_BASIS_KEY");
        }
        assert!(keyed_api_key(&api_key_descriptor("groq", source()), &store).is_none());
        std::fs::remove_dir_all(&dir).expect("test dir should be removable");
    }

    #[test]
    fn cookie_delivered_integrations_are_not_credential_candidates() {
        // A `*:web` session binding holds material but has no execution
        // engine; letting it into `run --provider auto` would produce a
        // doomed attempt, so presence alone must not make it a candidate.
        let (dir, store) = test_store("cookie");
        unsafe {
            std::env::set_var("AIFUEL_TEST_SESSION_KEY", "session-material");
        }
        let mut descriptor = api_key_descriptor(
            "claude-web",
            ApiKeySource::EnvOrStore {
                var: "AIFUEL_TEST_SESSION_KEY".to_owned(),
                credential: aifuel_core::CredentialRef::new("claude-web:web"),
            },
        );
        let ExecutionConfig::Http { auth, .. } = &mut descriptor.integration.execution else {
            panic!("the fixture builds an Http integration");
        };
        *auth = AuthBinding::ApiKey {
            source: match auth {
                AuthBinding::ApiKey { source, .. } => source.clone(),
                _ => unreachable!(),
            },
            delivery: aifuel_core::KeyDelivery::Cookie {
                name: "sessionKey".to_owned(),
            },
        };
        assert!(keyed_api_key(&descriptor, &store).is_none());
        unsafe {
            std::env::remove_var("AIFUEL_TEST_SESSION_KEY");
        }
        std::fs::remove_dir_all(&dir).expect("test dir should be removable");
    }

    #[test]
    fn non_api_key_integrations_are_not_credential_candidates() {
        // CLI integrations keep their quota/discovery candidacy; a binding
        // without an API key never enters the credential tier.
        let (_dir, store) = test_store("non-api-key");
        let cli = aifuel_providers::IntegrationDescriptor::builtin(
            aifuel_core::Integration {
                id: IntegrationId::new("codex"),
                provider: ProviderId::new("codex"),
                name: "codex".to_owned(),
                execution: ExecutionConfig::Cli {
                    adapter: aifuel_core::CliAdapterId::new("codex"),
                },
                monitoring: None,
            },
            Vec::new(),
        );
        assert!(keyed_api_key(&cli, &store).is_none());
        std::fs::remove_dir_all(&_dir).expect("test dir should be removable");
    }

    #[test]
    fn quota_wording_matches_only_exhaustion_phrasing() {
        assert!(provider_quota_wording("the endpoint returned HTTP 429"));
        assert!(provider_quota_wording("You exceeded your current quota"));
        assert!(!provider_quota_wording(
            "the provider exited with exit code 1"
        ));
        assert!(!provider_quota_wording("authentication failed"));
    }
}
