//! `aifuel auth list`: report the effective credential source per Provider
//! Integration plus every stored Managed Credential's metadata - kind,
//! expiry, destination binding, and per-key pool health. Secret material is
//! never printed.

use aifuel_core::{ApiKeySource, AuthBinding, CredentialRef, ExecutionConfig};
use aifuel_providers::{CredentialExpiry, CredentialKind, CredentialMetadata, KeyHealth};

/// Run `aifuel auth list [--json]`.
pub(super) fn run(args: &[String]) -> Result<u8, String> {
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "--help" | "-h" => {
                println!("Usage: aifuel auth list [--json]");
                println!("Reports the credential source per integration; never prints values.");
                return Ok(0);
            }
            unknown => return Err(format!("unknown argument {unknown:?} for auth list")),
        }
    }

    let registry = crate::integration_registry()?;
    let store = super::credential_store()?;
    let entries = store.list().map_err(|error| error.to_string())?;
    let stored: std::collections::BTreeMap<&CredentialRef, &CredentialMetadata> = entries
        .iter()
        .map(|(reference, meta)| (reference, meta))
        .collect();
    let discovery = aifuel_providers::DiscoveryContext::from_environment()
        .map_err(|error| error.to_string())?;
    let evidence = registry.evidence_context(&discovery, &store);

    if json {
        let integrations: Vec<serde_json::Value> = registry
            .list()
            .map(|descriptor| {
                let source = describe_source(descriptor, &stored);
                serde_json::json!({
                    "integration": descriptor.integration.id.as_str(),
                    "provider": descriptor.integration.provider.as_str(),
                    "credential_source": source,
                    "discovered": describe_discovery(descriptor, &evidence),
                })
            })
            .collect();
        let credentials: Vec<serde_json::Value> = entries
            .iter()
            .map(|(reference, meta)| {
                let (health, cooling_until, invalid_at) = match meta.key_health {
                    Some(KeyHealth::Cooling { until }) => ("cooling", Some(until), None),
                    Some(KeyHealth::Invalid { at }) => ("invalid", None, Some(at)),
                    Some(KeyHealth::Healthy) => ("healthy", None, None),
                    None => ("none", None, None),
                };
                serde_json::json!({
                    "credential": reference.as_str(),
                    "kind": match meta.kind {
                        CredentialKind::ApiKey => "api_key",
                        CredentialKind::OAuth => "oauth",
                    },
                    "expiry": describe_expiry(meta),
                    "account_id": meta.account_id,
                    "health": health,
                    "cooling_until": cooling_until,
                    "invalid_at": invalid_at,
                    "pool": pool_root(reference, meta),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "integrations": integrations,
                "credentials": credentials,
            }))
            .map_err(|error| format!("could not encode JSON: {error}"))?
        );
        return Ok(0);
    }

    println!("Integrations:");
    for descriptor in registry.list() {
        let integration = &descriptor.integration;
        let name = if integration.name != integration.id.as_str() {
            format!(" ({})", integration.name)
        } else {
            String::new()
        };
        println!(
            "  {:<24} {:<12} {}{}",
            integration.id.as_str(),
            describe_discovery(descriptor, &evidence),
            describe_source(descriptor, &stored),
            name
        );
    }
    println!();
    if entries.is_empty() {
        println!("No managed credentials stored.");
    } else {
        println!("Managed credentials:");
        for (reference, meta) in &entries {
            let kind = match meta.kind {
                CredentialKind::ApiKey => "api-key",
                CredentialKind::OAuth => "oauth",
            };
            let mut detail = describe_expiry(meta);
            if meta.kind == CredentialKind::ApiKey {
                detail.push_str(&format!(", {}", describe_health(meta.key_health)));
            }
            if let Some(root) = pool_root(reference, meta) {
                detail.push_str(&format!(", pool of {root}"));
            }
            println!("  {:<24} {:<8} {}", reference.as_str(), kind, detail);
        }
    }
    Ok(0)
}

/// The Key Pool a member record belongs to: the Credential Reference before
/// the last `/` segment, per the `root/…` member convention. Only API-key
/// records can be pool members.
fn pool_root<'a>(reference: &'a CredentialRef, meta: &CredentialMetadata) -> Option<&'a str> {
    if meta.kind != CredentialKind::ApiKey {
        return None;
    }
    reference
        .as_str()
        .rsplit_once('/')
        .map(|(parent, _)| parent)
}

/// The credential source one integration's execution config declares, with
/// live presence for env vars and store references.
pub(crate) fn describe_source(
    descriptor: &aifuel_providers::IntegrationDescriptor,
    stored: &std::collections::BTreeMap<&CredentialRef, &CredentialMetadata>,
) -> String {
    match &descriptor.integration.execution {
        ExecutionConfig::Cli { .. } => "provider CLI credential".to_owned(),
        ExecutionConfig::Http { auth, .. } => match auth {
            AuthBinding::None => "none".to_owned(),
            AuthBinding::ApiKey {
                source: ApiKeySource::Env { var },
                ..
            } => {
                if aifuel_providers::env_override(var).is_some() {
                    format!("env {var} (set)")
                } else {
                    format!("env {var} (absent)")
                }
            }
            AuthBinding::ApiKey {
                source: ApiKeySource::Store { credential },
                ..
            } => describe_pool(credential, pool_members(stored, credential)),
            AuthBinding::OAuth { credential, .. } => {
                if stored.contains_key(credential) {
                    format!("managed credential {} (present)", credential.as_str())
                } else {
                    format!("managed credential {} (absent)", credential.as_str())
                }
            }
            AuthBinding::ApiKey {
                source:
                    ApiKeySource::EnvOrStore {
                        var, credential, ..
                    },
                ..
            } => {
                let env_set = aifuel_providers::env_override(var).is_some();
                let members = pool_members(stored, credential);
                match (env_set, members.is_empty()) {
                    (true, false) => {
                        format!("{}; env {var} also set", describe_pool(credential, members))
                    }
                    (false, false) => describe_pool(credential, members),
                    (true, true) => format!("env {var} (set)"),
                    (false, true) => format!(
                        "env {var} (absent) or managed credential {} (absent)",
                        credential.as_str()
                    ),
                }
            }
        },
    }
}

/// The integration's local discovery evidence state, checked side-effect-free
/// (filesystem markers, environment presence, credential-store metadata).
fn describe_discovery(
    descriptor: &aifuel_providers::IntegrationDescriptor,
    context: &aifuel_providers::EvidenceContext<'_>,
) -> String {
    match descriptor.discover(context) {
        Ok(aifuel_core::DiscoveryState::Present) => "present".to_owned(),
        Ok(aifuel_core::DiscoveryState::Absent) => "absent".to_owned(),
        Err(error) => format!("check failed: {error}"),
    }
}

/// The stored records forming the Key Pool bound under `credential`: the
/// record at the reference itself plus every `credential/…` API-key member,
/// in Credential Reference order.
fn pool_members<'a>(
    stored: &std::collections::BTreeMap<&'a CredentialRef, &'a CredentialMetadata>,
    credential: &CredentialRef,
) -> Vec<(&'a CredentialRef, &'a CredentialMetadata)> {
    stored
        .iter()
        .filter(|(reference, meta)| {
            meta.kind == CredentialKind::ApiKey
                && aifuel_providers::is_pool_member(credential, reference)
        })
        .map(|(reference, meta)| (*reference, *meta))
        .collect()
}

/// The credential-source summary for a store-backed API-key binding: the
/// base record alone reads as a single managed credential; several members -
/// or a lone `credential/…` member after the base was removed - read as a
/// Key Pool with a health rollup.
fn describe_pool(
    credential: &CredentialRef,
    members: Vec<(&CredentialRef, &CredentialMetadata)>,
) -> String {
    let mut cooling = 0usize;
    let mut invalid = 0usize;
    for (_, member) in &members {
        match member.key_health {
            Some(KeyHealth::Cooling { .. }) => cooling += 1,
            Some(KeyHealth::Invalid { .. }) => invalid += 1,
            _ => {}
        }
    }
    match members.as_slice() {
        [] => format!("managed credential {credential} (absent)"),
        [(reference, member)] if *reference == credential => format!(
            "managed credential {credential} ({})",
            describe_health(member.key_health)
        ),
        members => {
            let mut detail = format!("{} healthy", members.len() - cooling - invalid);
            if cooling > 0 {
                detail.push_str(&format!(", {cooling} cooling"));
            }
            if invalid > 0 {
                detail.push_str(&format!(", {invalid} invalid"));
            }
            let keys = if members.len() == 1 { "key" } else { "keys" };
            format!("key pool {credential} ({} {keys}: {detail})", members.len())
        }
    }
}

/// The per-key pool state `auth list` reports: `healthy` for a usable key,
/// else its cooldown deadline or invalid mark. Never carries material.
fn describe_health(health: Option<KeyHealth>) -> String {
    match health {
        None | Some(KeyHealth::Healthy) => "healthy".to_owned(),
        Some(KeyHealth::Cooling { until }) => format!("cooling until {until}"),
        Some(KeyHealth::Invalid { at }) => format!("invalid since {at}"),
    }
}

fn describe_expiry(meta: &CredentialMetadata) -> String {
    match meta.expiry {
        CredentialExpiry::None => "no expiry".to_owned(),
        CredentialExpiry::Valid { until } => format!("expires {until}"),
        CredentialExpiry::Expired { at } => format!("expired at {at}"),
    }
}
