//! `aifuel auth` commands: inspect and manage Managed Credentials.
//!
//! The commands report the effective credential source per Provider
//! Integration - provider-owned (CLI), a named environment variable, a
//! managed Credential Reference, or none - and write API-key and session
//! records into the Credential Store. Secret material is never printed.
//!
//! API-key Managed Credentials form Key Pools: `set-key` on an integration
//! appends a member (`<ref>` first, then `<ref>/2`, `<ref>/3`, ...), and the
//! HTTP execution path rotates through the pool when a key is rate-limited.
//! `auth list` shows pool membership and per-key health; `auth remove`
//! deletes one member.
//!
//! `set-session` stores pasted browser-session material for `*:web`
//! integrations: a single record, delivered as a `Cookie` header. Nothing
//! here reads a browser profile or an OS keyring - entry is paste/stdin
//! only, and the material is never echoed.

use aifuel_core::{ApiKeySource, AuthBinding, CredentialRef, ExecutionConfig, KeyDelivery};
use aifuel_providers::{CredentialKind, CredentialStore};

mod list;
mod set_session;

// The Connect panel reuses `auth list`'s per-integration credential-state
// wording so the dashboard and CLI describe one source of truth.
pub(crate) use list::describe_source;

/// Run `aifuel auth <subcommand>`.
pub fn run(args: &[String]) -> Result<u8, String> {
    match args.first().map(String::as_str) {
        None | Some("--help") | Some("-h") => {
            print_help();
            Ok(0)
        }
        Some("list") => list::run(&args[1..]),
        Some("set-key") => set_key(&args[1..]),
        Some("set-session") => set_session::run(&args[1..]),
        Some("remove") => remove(&args[1..]),
        Some(unknown) => Err(format!(
            "unknown auth command {unknown:?}; use aifuel auth --help"
        )),
    }
}

pub(crate) fn credential_store() -> Result<CredentialStore, String> {
    let home = crate::user_home_dir()?;
    Ok(CredentialStore::new(
        crate::user_config_dir(&home)?.join("aifuel"),
    ))
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
                println!("the AI Fuel Credential Store and never printed. Repeating set-key on an");
                println!("integration appends to its key pool; a Credential Reference target");
                println!("overwrites that exact member.");
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

    let (reference, destination, delivery) = resolve_credential_ref(&target)?;
    if matches!(delivery, Some(KeyDelivery::Cookie { .. })) {
        return Err(format!(
            "integration {target} takes a session credential, not an API key; \
             use 'aifuel auth set-session {target}'"
        ));
    }
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
        Some(integration) => {
            // An integration target appends to the pool bound under its
            // Credential Reference: `openai:api-key`, then
            // `openai:api-key/2`, and so on.
            let (member, created) = store
                .add_pool_api_key(&reference, &material, integration)
                .map_err(|error| error.to_string())?;
            if !created {
                println!(
                    "Credential {} already stores that key; its failure state was cleared.",
                    member.as_str()
                );
            } else if member == reference {
                println!(
                    "Stored API key as credential {} bound to integration {}.",
                    reference.as_str(),
                    integration.as_str()
                );
            } else {
                println!(
                    "Added API key to the {} pool as credential {} bound to integration {}.",
                    reference.as_str(),
                    member.as_str(),
                    integration.as_str()
                );
            }
        }
        None => {
            // A raw Credential Reference overwrites that exact slot. An
            // existing record's destination binding is preserved so a
            // re-stored member stays bound to its integration, but a record
            // of a different kind is refused: silently converting a stored
            // session or OAuth grant into an API key would strand whatever
            // binds it.
            let existing = store.get(&reference).map_err(|error| error.to_string())?;
            if let Some(record) = &existing
                && record.kind() != CredentialKind::ApiKey
            {
                return Err(format!(
                    "credential {reference} already stores {}; remove it first or pick another reference",
                    record.kind()
                ));
            }
            let bound = existing.and_then(|credential| credential.destination().cloned());
            match bound {
                Some(integration) => store.set_api_key_for(&reference, &material, &integration),
                None => store.set_api_key(&reference, &material),
            }
            .map_err(|error| error.to_string())?;
            println!("Stored API key as credential {}.", reference.as_str());
        }
    }
    Ok(0)
}

/// A target naming an integration resolves to the Credential Reference its
/// Authentication Binding declares plus that integration's id as the
/// credential destination; anything else is treated as a raw Credential
/// Reference. An ambiguous selector is an error, never a raw reference.
///
/// The triple's third member is the binding's [`KeyDelivery`] for an
/// integration target (`None` for a raw reference): `set-key` refuses a
/// cookie-delivered binding and `set-session` refuses a key-delivered one,
/// so a pasted session can never land in the API-key pool it would be
/// resolved as.
/// Shared with the dashboard Connect panel's key submission.
#[allow(clippy::type_complexity)]
pub(crate) fn resolve_credential_ref(
    target: &str,
) -> Result<
    (
        CredentialRef,
        Option<aifuel_core::IntegrationId>,
        Option<KeyDelivery>,
    ),
    String,
> {
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
            return Ok((CredentialRef::new(target), None, None));
        }
    };
    let integration = descriptor.integration.id.clone();
    match &descriptor.integration.execution {
        ExecutionConfig::Http {
            auth:
                AuthBinding::ApiKey {
                    source:
                        ApiKeySource::Store { credential } | ApiKeySource::EnvOrStore { credential, .. },
                    delivery,
                },
            ..
        } => Ok((
            credential.clone(),
            Some(integration),
            Some(delivery.clone()),
        )),
        ExecutionConfig::Http {
            auth:
                AuthBinding::ApiKey {
                    source: ApiKeySource::Env { var },
                    delivery,
                },
            ..
        } => {
            let kind = if matches!(delivery, KeyDelivery::Cookie { .. }) {
                "session credential"
            } else {
                "API key"
            };
            Err(format!(
                "integration {target} reads its {kind} from environment variable {var}; \
                 export {var} instead"
            ))
        }
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

    let (notes, warnings) = removal_warnings(&reference)?;
    for note in notes {
        println!("{note}");
    }
    for warning in warnings {
        eprintln!("aifuel: warning - {warning}");
    }
    Ok(0)
}

/// What `aifuel auth remove` reports after deleting `reference`, split by
/// severity: informational notes report what kept working (surviving Key
/// Pool members), warnings report what will break - integrations still
/// binding the now-empty pool fail authentication until a replacement is
/// stored, or keep authenticating when their declared env var remains set.
/// Shared with the dashboard Connect panel's credential removal.
pub(crate) fn removal_warnings(
    reference: &CredentialRef,
) -> Result<(Vec<String>, Vec<String>), String> {
    let target = reference.as_str();
    let registry = crate::integration_registry()?;
    let store = credential_store()?;
    let entries = store.list().map_err(|error| error.to_string())?;
    let mut notes = Vec::new();
    let mut warnings = Vec::new();
    for descriptor in registry.list() {
        let id = descriptor.integration.id.as_str();
        // Removing one pool member strands the binding only when no member
        // remains; surviving members keep the integration authenticating.
        let remaining = |credential: &CredentialRef| -> usize {
            entries
                .iter()
                .filter(|(r, meta)| {
                    meta.kind == CredentialKind::ApiKey
                        && aifuel_providers::is_pool_member(credential, r)
                })
                .count()
        };
        match &descriptor.integration.execution {
            ExecutionConfig::Http { auth, .. } => match auth {
                // A cookie-delivered binding holds a session credential:
                // one record, no pool, so removing it strands the binding
                // outright.
                AuthBinding::ApiKey {
                    source: ApiKeySource::Store { credential },
                    delivery: KeyDelivery::Cookie { .. },
                } if credential == reference => {
                    warnings.push(format!(
                        "integration {id} binds session credential {credential} and will fail \
                         authentication until a replacement is stored"
                    ));
                }
                AuthBinding::ApiKey {
                    source:
                        ApiKeySource::EnvOrStore {
                            var, credential, ..
                        },
                    delivery: KeyDelivery::Cookie { .. },
                } if credential == reference => {
                    if aifuel_providers::env_override(var).is_some() {
                        warnings.push(format!(
                            "removing {target} leaves env var {var} active; integration {id} \
                             keeps authenticating from the environment"
                        ));
                    } else {
                        warnings.push(format!(
                            "integration {id} binds session credential {credential} and will \
                             fail authentication until a replacement is stored or {var} is \
                             exported"
                        ));
                    }
                }
                AuthBinding::ApiKey {
                    source: ApiKeySource::Store { credential },
                    ..
                } if aifuel_providers::is_pool_member(credential, reference) => {
                    let remaining = remaining(credential);
                    if remaining > 0 {
                        notes.push(format!(
                            "The key pool of credential {credential} bound to integration {id} \
                             retains {remaining} key(s)."
                        ));
                    } else {
                        warnings.push(format!(
                            "integration {id} binds credential {credential}, whose pool now \
                             holds no keys; it will fail authentication until a replacement is \
                             stored"
                        ));
                    }
                }
                AuthBinding::OAuth { credential, .. } => {
                    if credential == reference {
                        warnings.push(format!(
                            "integration {id} still binds credential {target} and will fail \
                             authentication until a replacement is stored"
                        ));
                    }
                }
                AuthBinding::ApiKey {
                    source:
                        ApiKeySource::EnvOrStore {
                            var, credential, ..
                        },
                    ..
                } if aifuel_providers::is_pool_member(credential, reference) => {
                    let remaining = remaining(credential);
                    if remaining > 0 {
                        notes.push(format!(
                            "The key pool of credential {credential} bound to integration {id} \
                             retains {remaining} key(s)."
                        ));
                    } else if aifuel_providers::env_override(var).is_some() {
                        warnings.push(format!(
                            "removing {target} leaves env var {var} active; integration {id} \
                             keeps authenticating from the environment"
                        ));
                    } else {
                        warnings.push(format!(
                            "integration {id} binds credential {credential}, whose pool now \
                             holds no keys; it will fail authentication until a replacement is \
                             stored or {var} is exported"
                        ));
                    }
                }
                _ => {}
            },
            ExecutionConfig::Cli { .. } => {}
        }
    }
    Ok((notes, warnings))
}

pub(super) fn next(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn print_help() {
    println!("Usage: aifuel auth list [--json]");
    println!("       aifuel auth set-key TARGET (--key KEY | --env-var NAME | --stdin)");
    println!("       aifuel auth set-session TARGET (--key SESSION | --stdin)");
    println!("       aifuel auth remove CREDENTIAL_REF");
    println!();
    println!("Inspects and manages AI Fuel Managed Credentials. Secret values are");
    println!("never printed. Built-in API-key integrations read named environment");
    println!("variables; managed references serve configured and OAuth integrations.");
    println!();
    println!("API-key credentials form Key Pools: repeating set-key on an integration");
    println!("appends a member (TARGET, TARGET/2, TARGET/3, ...), auth list shows each");
    println!("member's health, and auth remove deletes one member. A rate-limited key");
    println!("cools down while the run rotates to the next healthy member.");
    println!();
    println!("set-session stores pasted browser-session material for *:web");
    println!("integrations - a bare session token or a copied Cookie header. Entry is");
    println!("paste/stdin only; nothing reads a browser profile or OS keyring.");
}
