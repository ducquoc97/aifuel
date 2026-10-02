//! `aifuel auth set-session`: store pasted browser-session material as a
//! Session Managed Credential for a `*:web` integration or a raw Credential
//! Reference.
//!
//! Session material is whatever the provider's web app sends as cookies: a
//! bare session token (wrapped as `name=value` at send time) or a full
//! `Cookie` header line copied from dev tools (sent unchanged). The record
//! is a single non-pooled credential - a refreshed paste replaces the stale
//! one - and nothing here reads a browser profile or an OS keyring. The
//! material is never printed.

use aifuel_core::KeyDelivery;
use aifuel_providers::CredentialKind;

/// Run `aifuel auth set-session TARGET (--key SESSION | --stdin)`.
pub(super) fn run(args: &[String]) -> Result<u8, String> {
    let mut target = None;
    let mut key = None;
    let mut stdin = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => {
                println!("Usage: aifuel auth set-session TARGET (--key SESSION | --stdin)");
                println!(
                    "TARGET is a *:web Integration id or a Credential Reference. Paste a bare"
                );
                println!("session token or a full Cookie header line; it is stored masked and");
                println!("never printed. Repeating set-session on an integration replaces its");
                println!("session; nothing reads a browser profile or OS keyring.");
                return Ok(0);
            }
            "--key" => key = Some(super::next(args, &mut index, "--key")?),
            "--stdin" => stdin = true,
            flag if flag.starts_with('-') => {
                return Err(format!("unknown argument {flag:?} for auth set-session"));
            }
            positional => {
                if target.is_some() {
                    return Err("auth set-session accepts one TARGET".to_owned());
                }
                target = Some(positional.to_owned());
            }
        }
        index += 1;
    }
    let target = target.ok_or_else(|| "auth set-session requires a TARGET".to_owned())?;
    if key.is_some() == stdin {
        return Err("choose exactly one of --key or --stdin".to_owned());
    }

    let (reference, destination, delivery) = super::resolve_credential_ref(&target)?;
    if destination.is_some() && !matches!(delivery, Some(KeyDelivery::Cookie { .. })) {
        return Err(format!(
            "integration {target} takes an API key, not a session credential; \
             use 'aifuel auth set-key {target}'"
        ));
    }

    let material = if let Some(value) = key {
        eprintln!("aifuel: note - --key leaves the value in shell history; prefer --stdin");
        value
    } else {
        use std::io::Read;
        let mut buffer = String::new();
        std::io::stdin()
            .read_to_string(&mut buffer)
            .map_err(|error| format!("could not read the session from stdin: {error}"))?;
        buffer.trim().to_owned()
    };
    if material.is_empty() {
        return Err("the session credential is empty".to_owned());
    }

    let store = super::credential_store()?;
    match &destination {
        Some(integration) => {
            store
                .set_session_for(&reference, &material, integration)
                .map_err(|error| error.to_string())?;
            println!(
                "Stored session credential {} bound to integration {}.",
                reference.as_str(),
                integration.as_str()
            );
        }
        None => {
            // A raw Credential Reference overwrites that exact slot, with
            // the same kind-conflict guard `set-key` applies.
            let existing = store.get(&reference).map_err(|error| error.to_string())?;
            if let Some(record) = &existing
                && record.kind() != CredentialKind::Session
            {
                return Err(format!(
                    "credential {reference} already stores {}; remove it first or pick another reference",
                    record.kind()
                ));
            }
            let bound = existing.and_then(|credential| credential.destination().cloned());
            match bound {
                Some(integration) => store.set_session_for(&reference, &material, &integration),
                None => store.set_session(&reference, &material),
            }
            .map_err(|error| error.to_string())?;
            println!("Stored session credential {}.", reference.as_str());
        }
    }
    Ok(0)
}
