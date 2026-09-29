//! `aifuel auth` commands: inspect and manage Managed Credentials.
//!
//! The commands report the effective credential source per Provider
//! Integration - provider-owned (CLI), a named environment variable, a
//! managed Credential Reference, or none - and write API-key records into the
//! Credential Store. Secret material is never printed.

use aifuel_core::{ApiKeySource, AuthBinding, CredentialRef, ExecutionConfig};
use aifuel_providers::{CredentialExpiry, CredentialKind, CredentialMetadata, CredentialStore};

/// Run `aifuel auth <subcommand>`.
pub fn run(args: &[String]) -> Result<u8, String> {
    match args.first().map(String::as_str) {
        None | Some("--help") | Some("-h") => {
            print_help();
            Ok(0)
        }
        Some("list") => list(&args[1..]),
        Some("set-key") => set_key(&args[1..]),
        Some("remove") => remove(&args[1..]),
        Some(unknown) => Err(format!(
            "unknown auth command {unknown:?}; use aifuel auth --help"
        )),
    }
}

fn credential_store() -> Result<CredentialStore, String> {
    let home = crate::user_home_dir()?;
    Ok(CredentialStore::new(
        crate::user_config_dir(&home)?.join("aifuel"),
    ))
}

fn list(args: &[String]) -> Result<u8, String> {
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
    let store = credential_store()?;
    let entries = store.list().map_err(|error| error.to_string())?;
    let stored: std::collections::BTreeMap<&aifuel_core::CredentialRef, &CredentialMetadata> =
        entries
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
                serde_json::json!({
                    "credential": reference.as_str(),
                    "kind": match meta.kind {
                        CredentialKind::ApiKey => "api_key",
                        CredentialKind::OAuth => "oauth",
                    },
                    "expiry": describe_expiry(meta),
                    "account_id": meta.account_id,
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
        let name = (integration.name != integration.id.as_str())
            .then(|| format!(" ({})", integration.name))
            .unwrap_or_default();
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
            println!(
                "  {:<24} {:<8} {}",
                reference.as_str(),
                kind,
                describe_expiry(meta)
            );
        }
    }
    Ok(0)
}

/// The credential source one integration's execution config declares, with
/// live presence for env vars and store references.
fn describe_source(
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
            }
            | AuthBinding::OAuth { credential, .. } => {
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
                let stored_present = stored.contains_key(credential);
                match (env_set, stored_present) {
                    (true, true) => format!(
                        "managed credential {} (present); env {var} also set",
                        credential.as_str()
                    ),
                    (false, true) => {
                        format!("managed credential {} (present)", credential.as_str())
                    }
                    (true, false) => format!("env {var} (set)"),
                    (false, false) => format!(
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

fn describe_expiry(meta: &CredentialMetadata) -> String {
    match meta.expiry {
        CredentialExpiry::None => "no expiry".to_owned(),
        CredentialExpiry::Valid { until } => format!("expires {until}"),
        CredentialExpiry::Expired { at } => format!("expired at {at}"),
    }
}

fn set_key(args: &[String]) -> Result<u8, String> {
    let mut target = None;
    let mut key = None;
    let mut env_var = None;
    let mut stdin = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => {
                println!(
                    "Usage: aifuel auth set-key TARGET [--key KEY | --env-var NAME | --stdin]"
                );
                println!(
                    "TARGET is an Integration id or a Credential Reference. The key is stored in"
                );
                println!("the AI Fuel Credential Store and never printed.");
                return Ok(0);
            }
            "--key" => key = Some(next(args, &mut index, "--key")?),
            "--env-var" => env_var = Some(next(args, &mut index, "--env-var")?),
            "--stdin" => stdin = true,
            flag if flag.starts_with('-') => {
                return Err(format!("unknown argument {flag:?} for auth set-key"));
            }
            positional => {
                if target.is_some() {
                    return Err("auth set-key accepts one TARGET".to_owned());
                }
                target = Some(positional.to_owned());
            }
        }
        index += 1;
    }
    let target = target.ok_or_else(|| "auth set-key requires a TARGET".to_owned())?;
    let source_count = key.is_some() as usize + env_var.is_some() as usize + stdin as usize;
    if source_count != 1 {
        return Err("choose exactly one of --key, --env-var, or --stdin".to_owned());
    }

    let (reference, destination) = resolve_credential_ref(&target)?;
    let material = if let Some(value) = key {
        eprintln!("aifuel: note - --key leaves the value in shell history; prefer --stdin");
        value
    } else if let Some(var) = env_var {
        if !aifuel_providers::valid_env_var_name(&var) {
            return Err(format!(
                "--env-var {var:?} is not a valid environment variable name"
            ));
        }
        aifuel_providers::env_override(&var)
            .ok_or_else(|| format!("environment variable {var} is unset or empty"))?
    } else {
        use std::io::Read;
        let mut buffer = String::new();
        std::io::stdin()
            .read_to_string(&mut buffer)
            .map_err(|error| format!("could not read the key from stdin: {error}"))?;
        buffer.trim().to_owned()
    };
    if material.is_empty() {
        return Err("the API key is empty".to_owned());
    }

    let store = credential_store()?;
    match &destination {
        Some(integration) => store.set_api_key_for(&reference, &material, integration),
        None => store.set_api_key(&reference, &material),
    }
    .map_err(|error| error.to_string())?;
    match &destination {
        Some(integration) => println!(
            "Stored API key as credential {} bound to integration {}.",
            reference.as_str(),
            integration.as_str()
        ),
        None => println!("Stored API key as credential {}.", reference.as_str()),
    }
    Ok(0)
}

/// A target naming an integration resolves to the Credential Reference its
/// Authentication Binding declares plus that integration's id as the
/// credential destination; anything else is treated as a raw Credential
/// Reference. An ambiguous selector is an error, never a raw reference.
fn resolve_credential_ref(
    target: &str,
) -> Result<(CredentialRef, Option<aifuel_core::IntegrationId>), String> {
    use aifuel_providers::ResolveError;
    let registry = crate::integration_registry()?;
    let descriptor = match registry.resolve(target) {
        Ok(descriptor) => descriptor,
        Err(error @ ResolveError::Ambiguous { .. }) => {
            // Ambiguity must surface as an error, never collapse into a raw
            // Credential Reference for a different destination.
            return Err(error.to_string());
        }
        Err(ResolveError::Unknown { .. }) => {
            return Ok((CredentialRef::new(target), None));
        }
    };
    let integration = descriptor.integration.id.clone();
    match &descriptor.integration.execution {
        ExecutionConfig::Http {
            auth:
                AuthBinding::ApiKey {
                    source:
                        ApiKeySource::Store { credential } | ApiKeySource::EnvOrStore { credential, .. },
                    ..
                },
            ..
        } => Ok((credential.clone(), Some(integration))),
        ExecutionConfig::Http {
            auth:
                AuthBinding::ApiKey {
                    source: ApiKeySource::Env { var },
                    ..
                },
            ..
        } => Err(format!(
            "integration {target} reads its API key from environment variable {var}; \
             export {var} instead"
        )),
        ExecutionConfig::Http {
            auth: AuthBinding::None,
            ..
        } => Err(format!("integration {target} declares no credential")),
        ExecutionConfig::Http {
            auth: AuthBinding::OAuth { credential, .. },
            ..
        } => Err(format!(
            "integration {target} uses OAuth credential {}; OAuth login is not available yet",
            credential.as_str()
        )),
        ExecutionConfig::Cli { .. } => Err(format!(
            "integration {target} runs the provider CLI, which owns its credential"
        )),
    }
}

fn remove(args: &[String]) -> Result<u8, String> {
    let mut target = None;
    for arg in args {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("Usage: aifuel auth remove CREDENTIAL_REF");
                println!("Deletes one Managed Credential from the AI Fuel Credential Store.");
                return Ok(0);
            }
            flag if flag.starts_with('-') => {
                return Err(format!("unknown argument {flag:?} for auth remove"));
            }
            positional => {
                if target.is_some() {
                    return Err("auth remove accepts one CREDENTIAL_REF".to_owned());
                }
                target = Some(positional.to_owned());
            }
        }
    }
    let target = target.ok_or_else(|| "auth remove requires a CREDENTIAL_REF".to_owned())?;
    let reference = CredentialRef::new(&target);

    let store = credential_store()?;
    let removed = store
        .remove(&reference)
        .map_err(|error| error.to_string())?;
    if !removed {
        return Err(format!("no managed credential named {target}"));
    }
    println!("Removed credential {target}.");

    let registry = crate::integration_registry()?;
    for descriptor in registry.list() {
        let id = descriptor.integration.id.as_str();
        match &descriptor.integration.execution {
            ExecutionConfig::Http { auth, .. } => match auth {
                AuthBinding::ApiKey {
                    source: ApiKeySource::Store { credential },
                    ..
                }
                | AuthBinding::OAuth { credential, .. } => {
                    if credential == &reference {
                        eprintln!(
                            "aifuel: warning - integration {id} still binds credential {target} \
                             and will fail authentication until a replacement is stored"
                        );
                    }
                }
                AuthBinding::ApiKey {
                    source:
                        ApiKeySource::EnvOrStore {
                            var, credential, ..
                        },
                    ..
                } => {
                    if credential == &reference {
                        if aifuel_providers::env_override(var).is_some() {
                            eprintln!(
                                "aifuel: warning - removing {target} leaves env var {var} active; \
                                 integration {id} keeps authenticating from the environment"
                            );
                        } else {
                            eprintln!(
                                "aifuel: warning - integration {id} still binds credential \
                                 {target} and will fail authentication until a replacement is \
                                 stored or {var} is exported"
                            );
                        }
                    }
                }
                _ => {}
            },
            ExecutionConfig::Cli { .. } => {}
        }
    }
    Ok(0)
}

fn next(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn print_help() {
    println!("Usage: aifuel auth list [--json]");
    println!("       aifuel auth set-key TARGET (--key KEY | --env-var NAME | --stdin)");
    println!("       aifuel auth remove CREDENTIAL_REF");
    println!();
    println!("Inspects and manages AI Fuel Managed Credentials. Secret values are");
    println!("never printed. Built-in API-key integrations read named environment");
    println!("variables; managed references serve configured and OAuth integrations.");
}
