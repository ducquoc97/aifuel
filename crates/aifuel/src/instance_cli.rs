//! `aifuel instance` commands: manage named Provider Integration instances.
//!
//! Instances live in the `instances` map of `providers.json` (the same user
//! configuration directory as `mcp.json` and `execution.json`). Each names a
//! base integration, an environment overlay, and optionally a Managed
//! Credential binding:
//!
//! - `list` prints one row per instance: id, base integration, and the env
//!   variables the overlay sets - names only.
//! - `show` prints one instance's full spec: literal values are shown (they
//!   are configuration the user wrote), while `{ "credential": "ref" }`
//!   values print the Credential Reference, never the resolved material.
//! - `add` validates the new instance against the live registry with the
//!   same checks `IntegrationRegistry::build` applies, then writes it.
//! - `remove` deletes the entry; a session that recorded the instance id
//!   then fails to resume honestly rather than falling back to the base
//!   integration's environment.
//!
//! Resolved secret material is never touched here: credential references
//! are stored and compared by name, and resolution happens only inside a
//! provider-process spawn.

use aifuel_core::{CredentialRef, IntegrationId};
use aifuel_providers::{
    InstanceDescriptor, InstanceEnvSource, PROVIDERS_FILE_NAME, edit_instances,
};
use std::collections::BTreeMap;

/// Run `aifuel instance <subcommand>`.
pub fn run(args: &[String]) -> Result<u8, String> {
    match args.first().map(String::as_str) {
        None | Some("--help") | Some("-h") => {
            print_help();
            Ok(0)
        }
        Some("list") => list(&args[1..]),
        Some("show") => show(&args[1..]),
        Some("add") => add(&args[1..]),
        Some("remove") => remove(&args[1..]),
        Some(unknown) => Err(format!(
            "unknown instance command {unknown:?}; use aifuel instance --help"
        )),
    }
}

fn providers_path() -> Result<std::path::PathBuf, String> {
    Ok(crate::aifuel_config_dir()?.join(PROVIDERS_FILE_NAME))
}

fn list(args: &[String]) -> Result<u8, String> {
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "--help" | "-h" => {
                println!("Usage: aifuel instance list [--json]");
                println!("Lists named Provider Integration instances; never prints env values.");
                return Ok(0);
            }
            unknown => return Err(format!("unknown argument {unknown:?} for instance list")),
        }
    }

    let registry = crate::integration_registry()?;
    let store = aifuel_providers::CredentialStore::new(crate::aifuel_config_dir()?);
    // Metadata only - this map records which credential references exist,
    // never their material.
    let stored: std::collections::BTreeSet<CredentialRef> = store
        .list()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|(reference, _)| reference)
        .collect();

    if json {
        let instances: Vec<serde_json::Value> = registry
            .instances()
            .map(|instance| instance_json(instance, &registry, &stored))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "instances": instances,
            }))
            .map_err(|error| format!("could not encode JSON: {error}"))?
        );
        return Ok(0);
    }

    let mut listed = false;
    for instance in registry.instances() {
        listed = true;
        let credential = instance
            .credential
            .as_ref()
            .map(|reference| {
                if stored.contains(reference) {
                    format!("credential {} (present)", reference.as_str())
                } else {
                    format!("credential {} (absent)", reference.as_str())
                }
            })
            .unwrap_or_else(|| "inherits base credential".to_owned());
        println!(
            "  {:<24} {:<16} {:<40} env: {}",
            instance.id.as_str(),
            instance.integration.as_str(),
            credential,
            describe_env_names(instance),
        );
    }
    if !listed {
        println!("No Provider Integration instances configured.");
        println!("Add one with: aifuel instance add <id> --integration <id>");
    }
    Ok(0)
}

fn show(args: &[String]) -> Result<u8, String> {
    let mut json = false;
    let mut id = None;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "--help" | "-h" => {
                println!("Usage: aifuel instance show INSTANCE_ID [--json]");
                println!(
                    "Prints the instance spec; credential env values show their reference only."
                );
                return Ok(0);
            }
            flag if flag.starts_with('-') => {
                return Err(format!("unknown argument {flag:?} for instance show"));
            }
            positional => {
                if id.is_some() {
                    return Err("instance show accepts one INSTANCE_ID".to_owned());
                }
                id = Some(positional.to_owned());
            }
        }
    }
    let id = id.ok_or_else(|| "instance show requires an INSTANCE_ID".to_owned())?;
    let registry = crate::integration_registry()?;
    let instance = registry
        .instance(&IntegrationId::new(&id))
        .ok_or_else(|| format!("no Provider Integration instance is registered as '{id}'"))?;
    let store = aifuel_providers::CredentialStore::new(crate::aifuel_config_dir()?);
    let stored: std::collections::BTreeSet<CredentialRef> = store
        .list()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|(reference, _)| reference)
        .collect();

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&instance_json(instance, &registry, &stored))
                .map_err(|error| format!("could not encode JSON: {error}"))?
        );
        return Ok(0);
    }

    let base = registry.get(&instance.integration);
    println!("instance:    {}", instance.id.as_str());
    println!("integration: {}", instance.integration.as_str());
    if let Some(base) = base {
        println!("provider:    {}", base.integration.provider.as_str());
        if base.integration.name != base.integration.id.as_str() {
            println!("name:        {}", base.integration.name);
        }
    }
    match &instance.credential {
        Some(reference) => println!(
            "credential:  {} ({})",
            reference.as_str(),
            if stored.contains(reference) {
                "present"
            } else {
                "absent"
            }
        ),
        None => println!("credential:  inherits the base integration's binding"),
    }
    if instance.env.is_empty() {
        println!("env:         (none)");
    } else {
        println!("env:");
        for (name, source) in &instance.env {
            match source {
                // Literal values are configuration the user wrote into
                // providers.json; credential references print as references
                // - never resolved material.
                InstanceEnvSource::Literal(value) => println!("  {name}={value}"),
                InstanceEnvSource::Credential(reference) => {
                    println!("  {name}={{credential: {}}}", reference.as_str())
                }
            }
        }
    }
    Ok(0)
}

fn add(args: &[String]) -> Result<u8, String> {
    let mut id = None;
    let mut integration = None;
    let mut env = BTreeMap::new();
    let mut credential = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => {
                print_add_help();
                return Ok(0);
            }
            "--integration" => integration = Some(next_value(args, &mut index, "--integration")?),
            "--env" => {
                let spec = next_value(args, &mut index, "--env")?;
                let Some((name, value)) = spec.split_once('=') else {
                    return Err("--env expects NAME=VALUE".to_owned());
                };
                if !aifuel_providers::valid_env_var_name(name) {
                    return Err(format!("{name:?} is not a valid environment variable name"));
                }
                if env
                    .insert(
                        name.to_owned(),
                        InstanceEnvSource::Literal(value.to_owned()),
                    )
                    .is_some()
                {
                    return Err(format!("env var {name} is declared twice"));
                }
            }
            "--env-credential" => {
                let spec = next_value(args, &mut index, "--env-credential")?;
                let Some((name, reference)) = spec.split_once('=') else {
                    return Err("--env-credential expects NAME=CREDENTIAL_REF".to_owned());
                };
                if !aifuel_providers::valid_env_var_name(name) {
                    return Err(format!("{name:?} is not a valid environment variable name"));
                }
                if reference.trim().is_empty() {
                    return Err(
                        "--env-credential requires a non-empty Credential Reference".to_owned()
                    );
                }
                if env
                    .insert(
                        name.to_owned(),
                        InstanceEnvSource::Credential(CredentialRef::new(reference)),
                    )
                    .is_some()
                {
                    return Err(format!("env var {name} is declared twice"));
                }
            }
            "--credential" => credential = Some(next_value(args, &mut index, "--credential")?),
            flag if flag.starts_with('-') => {
                return Err(format!("unknown argument {flag:?} for instance add"));
            }
            positional => {
                if id.is_some() {
                    return Err("instance add accepts one INSTANCE_ID".to_owned());
                }
                id = Some(positional.to_owned());
            }
        }
        index += 1;
    }
    let id = id.ok_or_else(|| "instance add requires an INSTANCE_ID".to_owned())?;
    if id.trim().is_empty() {
        return Err("instance id must not be empty".to_owned());
    }
    let integration =
        integration.ok_or_else(|| "instance add requires --integration <id>".to_owned())?;
    if let Some(credential) = &credential
        && credential.trim().is_empty()
    {
        return Err("--credential requires a non-empty Credential Reference".to_owned());
    }

    // The instance is checked against the live registry the same way a
    // loaded `providers.json` entry is, so the file never gains an entry
    // the registry would reject.
    let descriptor = InstanceDescriptor {
        id: IntegrationId::new(&id),
        integration: IntegrationId::new(&integration),
        env,
        credential: credential.map(CredentialRef::new),
    };
    let registry = crate::integration_registry()?;
    registry
        .check_instance(&descriptor)
        .map_err(|error| error.to_string())?;

    let env_spec: serde_json::Map<String, serde_json::Value> = descriptor
        .env
        .iter()
        .map(|(name, source)| {
            let value = match source {
                InstanceEnvSource::Literal(value) => serde_json::Value::String(value.clone()),
                InstanceEnvSource::Credential(reference) => serde_json::json!({
                    "credential": reference.as_str(),
                }),
            };
            (name.clone(), value)
        })
        .collect();
    let mut entry = serde_json::Map::new();
    entry.insert(
        "integration".to_owned(),
        serde_json::Value::String(integration.clone()),
    );
    if !env_spec.is_empty() {
        entry.insert("env".to_owned(), serde_json::Value::Object(env_spec));
    }
    if let Some(reference) = &descriptor.credential {
        entry.insert(
            "credential".to_owned(),
            serde_json::Value::String(reference.as_str().to_owned()),
        );
    }

    let path = providers_path()?;
    let entry = serde_json::Value::Object(entry);
    let instance_id = id.clone();
    edit_instances(&path, move |instances| {
        instances.insert(instance_id.clone(), entry.clone());
        Ok(())
    })
    .map_err(|error| error.to_string())?;
    // The write passed `check_instance`, so a registry rebuild must succeed;
    // verify anyway so a torn file reports instead of registering silently.
    crate::integration_registry()
        .map_err(|error| format!("providers.json was written but now fails to load: {error}"))?;
    println!(
        "Added instance {id} served by integration {integration}. Use it anywhere an \
         integration id is accepted, e.g. `aifuel run --integration {id}`."
    );
    Ok(0)
}

fn remove(args: &[String]) -> Result<u8, String> {
    let mut id = None;
    for arg in args {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("Usage: aifuel instance remove INSTANCE_ID");
                return Ok(0);
            }
            flag if flag.starts_with('-') => {
                return Err(format!("unknown argument {flag:?} for instance remove"));
            }
            positional => {
                if id.is_some() {
                    return Err("instance remove accepts one INSTANCE_ID".to_owned());
                }
                id = Some(positional.to_owned());
            }
        }
    }
    let id = id.ok_or_else(|| "instance remove requires an INSTANCE_ID".to_owned())?;
    let registry = crate::integration_registry()?;
    if registry.instance(&IntegrationId::new(&id)).is_none() {
        return Err(format!(
            "no Provider Integration instance is registered as '{id}'"
        ));
    }
    let path = providers_path()?;
    let instance_id = id.clone();
    edit_instances(&path, move |instances| {
        instances.remove(&instance_id);
        Ok(())
    })
    .map_err(|error| error.to_string())?;
    println!("Removed instance {id}.");
    Ok(0)
}

/// One instance's reportable spec: literal values are configuration, so
/// they appear; credential references appear as references. Resolved
/// material is never reachable here because `list`/`show` never call
/// `resolve_env`.
fn instance_json(
    instance: &InstanceDescriptor,
    registry: &aifuel_providers::IntegrationRegistry,
    stored: &std::collections::BTreeSet<CredentialRef>,
) -> serde_json::Value {
    let env: serde_json::Map<String, serde_json::Value> = instance
        .env
        .iter()
        .map(|(name, source)| {
            let value = match source {
                InstanceEnvSource::Literal(value) => serde_json::Value::String(value.clone()),
                InstanceEnvSource::Credential(reference) => serde_json::json!({
                    "credential": reference.as_str(),
                    "stored": stored.contains(reference),
                }),
            };
            (name.clone(), value)
        })
        .collect();
    serde_json::json!({
        "id": instance.id.as_str(),
        "integration": instance.integration.as_str(),
        "provider": registry
            .get(&instance.integration)
            .map(|base| base.integration.provider.as_str()),
        "env": env,
        "credential": instance.credential.as_ref().map(|reference| serde_json::json!({
            "reference": reference.as_str(),
            "stored": stored.contains(reference),
        })),
    })
}

/// The env overlay in `list` output: variable names and value source kinds
/// only - `literal` values stay in `show`, resolved material stays nowhere.
fn describe_env_names(instance: &InstanceDescriptor) -> String {
    if instance.env.is_empty() {
        return "-".to_owned();
    }
    instance
        .env
        .iter()
        .map(|(name, source)| match source {
            InstanceEnvSource::Literal(_) => format!("{name}=literal"),
            InstanceEnvSource::Credential(reference) => {
                format!("{name}=credential:{}", reference.as_str())
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn next_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn print_help() {
    println!("Usage: aifuel instance list [--json]");
    println!("       aifuel instance show INSTANCE_ID [--json]");
    println!("       aifuel instance add INSTANCE_ID --integration INTEGRATION_ID \\");
    println!("           [--env NAME=VALUE]... [--env-credential NAME=CREDENTIAL_REF]... \\");
    println!("           [--credential CREDENTIAL_REF]");
    println!("       aifuel instance remove INSTANCE_ID");
    println!();
    println!("Manage named Provider Integration instances in providers.json. An instance");
    println!("id is accepted anywhere an integration id is; its env overlay applies to");
    println!("the provider process at spawn, and credential references resolve lazily");
    println!("from the Credential Store - secret values are never printed.");
}

fn print_add_help() {
    println!("Usage: aifuel instance add INSTANCE_ID --integration INTEGRATION_ID [OPTIONS]");
    println!();
    println!("  --integration ID             the base Provider Integration this instance serves");
    println!("  --env NAME=VALUE             set a literal environment variable on the provider");
    println!("                               process (repeatable)");
    println!("  --env-credential NAME=REF    set an env var from a Managed Credential in the");
    println!("                               Credential Store, resolved at spawn (repeatable)");
    println!("  --credential REF             bind a named Managed Credential to the instance:");
    println!("                               the destination `aifuel auth set-key INSTANCE_ID`");
    println!("                               writes, and the credential slot of an HTTP");
    println!("                               integration's Authentication Binding");
    println!();
    println!("Secret values are never printed. Store one with `aifuel auth set-key`.");
}
