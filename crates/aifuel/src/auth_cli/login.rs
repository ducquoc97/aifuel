//! `aifuel auth login TARGET [--device]`: run the target's compiled OAuth
//! login flow - the opencode-style authorize/wait lifecycle - and store
//! the minted grant as a Managed Credential in the Credential Store.
//!
//! TARGET resolves three ways:
//!
//! 1. A compiled OAuth profile's provider or integration (`codex`,
//!    `copilot`, `codex:oauth`, `copilot:oauth`): the grant lands under
//!    the integration's Credential Reference and binds to it.
//! 2. A configured integration or instance whose Authentication Binding
//!    is `oauth-ref`: the binding's declared credential reference is
//!    stored, bound to that integration/instance.
//! 3. Anything else is an error naming what `TARGET` actually uses.

use super::credential_store;
use aifuel_core::{ApiKeySource, AuthBinding, CredentialRef, ExecutionConfig, IntegrationId};
use aifuel_providers::oauth::{self, LoginInstructions, OAuthFlow, OAuthFlowSpec};

/// `aifuel auth login TARGET [--device]`.
pub fn run(args: &[String]) -> Result<u8, String> {
    let mut target = None;
    let mut device = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => {
                print_help();
                return Ok(0);
            }
            "--device" => device = true,
            flag if flag.starts_with('-') => {
                return Err(format!("unknown argument {flag:?} for auth login"));
            }
            positional => {
                if target.is_some() {
                    return Err("auth login accepts one TARGET".to_owned());
                }
                target = Some(positional.to_owned());
            }
        }
        index += 1;
    }
    let target = target.ok_or_else(|| "auth login requires a TARGET".to_owned())?;
    let (spec, reference, destination) = login_target(&target)?;
    let flow = pick_flow(spec, device)?;
    let pending = oauth::begin_login(spec, flow)?;
    match pending.instructions() {
        LoginInstructions::EnterCode {
            verification_uri,
            user_code,
        } => {
            println!("To sign in, open {verification_uri}");
            println!("and enter this one-time code: {user_code}");
            println!("(the code expires in about 15 minutes)");
        }
        LoginInstructions::OpenBrowser { authorization_url } => {
            println!("Open this URL in a browser on this machine to sign in:");
            println!("  {authorization_url}");
            if open_browser(&authorization_url) {
                println!("(attempted to open it in your browser)");
            }
        }
    }
    let mut tokens = pending
        .wait()
        .map_err(|error| format!("the OAuth login did not complete: {error}"))?;
    tokens.destination = Some(destination.clone());
    let store = credential_store()?;
    store
        .set_oauth(&reference, tokens)
        .map_err(|error| error.to_string())?;
    println!(
        "Signed in to {}; credential '{reference}' is bound to integration {destination}.",
        spec.provider
    );
    Ok(0)
}

/// Resolve `TARGET` to the compiled profile to run plus the Credential
/// Reference and destination the minted grant is stored under.
fn login_target(
    target: &str,
) -> Result<(&'static OAuthFlowSpec, CredentialRef, IntegrationId), String> {
    // A compiled profile selector first: provider id or integration id.
    if let Some(spec) = oauth::profile_for(target) {
        let integration = IntegrationId::new(spec.integration);
        return Ok((spec, CredentialRef::new(spec.integration), integration));
    }
    let registry = crate::integration_registry()?;
    // Provider Integration instances bind under the instance id; a spec'd
    // credential override wins over the base binding's reference.
    let target_id = IntegrationId::new(target);
    let (binding, instance_credential, destination) =
        if let Some(instance) = registry.instance(&target_id) {
            let base = registry
                .resolve(instance.integration.as_str())
                .map_err(|error| error.to_string())?;
            let binding = match &base.integration.execution {
                ExecutionConfig::Http { auth, .. } => auth.clone(),
                ExecutionConfig::Cli { .. } => AuthBinding::None,
            };
            (binding, instance.credential.clone(), instance.id.clone())
        } else {
            match registry.resolve(target) {
                Ok(descriptor) => {
                    let binding = match &descriptor.integration.execution {
                        ExecutionConfig::Http { auth, .. } => auth.clone(),
                        ExecutionConfig::Cli { .. } => {
                            return Err(format!(
                                "integration {target} runs the provider CLI, which owns its \
                             credential; sign in through that CLI instead"
                            ));
                        }
                    };
                    (binding, None, descriptor.integration.id.clone())
                }
                Err(error) => {
                    return Err(format!(
                        "{error}; `auth login` targets a compiled OAuth profile ({}) or an \
                     integration with auth.kind 'oauth-ref'",
                        oauth::profiles()
                            .map(|spec| spec.profile)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
        };
    let (credential, profile) = match binding {
        AuthBinding::OAuth {
            credential,
            profile,
        } => (credential, profile),
        AuthBinding::ApiKey { source, .. } => {
            let via = match &source {
                ApiKeySource::Env { var } | ApiKeySource::EnvOrStore { var, .. } => {
                    format!("environment variable {var} or ")
                }
                ApiKeySource::Store { .. } => String::new(),
            };
            return Err(format!(
                "integration {target} authenticates with an API key; provide it via \
                 {via}`aifuel auth set-key {target}`"
            ));
        }
        AuthBinding::None => {
            return Err(format!("integration {target} declares no credential"));
        }
    };
    let spec = oauth::profile(&profile).ok_or_else(|| {
        format!(
            "integration {target} binds OAuth profile '{profile}', which this build does \
             not compile"
        )
    })?;
    Ok((spec, instance_credential.unwrap_or(credential), destination))
}

/// The flow to run: the profile's default, or its device-class grant when
/// `--device` asks for a headless-capable flow.
fn pick_flow(spec: &'static OAuthFlowSpec, device: bool) -> Result<&'static OAuthFlow, String> {
    if device {
        return spec
            .flows
            .iter()
            .find(|flow| {
                matches!(
                    flow,
                    OAuthFlow::Device { .. } | OAuthFlow::DeviceAuth { .. }
                )
            })
            .ok_or_else(|| format!("profile '{}' offers no device login flow", spec.profile));
    }
    spec.flows
        .first()
        .ok_or_else(|| format!("profile '{}' compiles no login flows", spec.profile))
}

/// Best-effort browser open for the loopback flow: the URL is already
/// printed, so a missing opener is not a failure.
fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    const OPENERS: &[&[&str]] = &[&["open"]];
    #[cfg(windows)]
    const OPENERS: &[&[&str]] = &[&["explorer"]];
    #[cfg(all(unix, not(target_os = "macos")))]
    const OPENERS: &[&[&str]] = &[&["xdg-open"], &["wslview"], &["sensible-browser"]];
    #[cfg(not(any(unix, windows)))]
    const OPENERS: &[&[&str]] = &[];
    OPENERS.iter().any(|argv| {
        let Some((program, args)) = argv.split_first() else {
            return false;
        };
        std::process::Command::new(program)
            .args(args)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
    })
}

fn print_help() {
    let profiles: Vec<&str> = oauth::profiles().map(|spec| spec.profile).collect();
    println!("Usage: aifuel auth login TARGET [--device]");
    println!();
    println!("Runs TARGET's compiled OAuth login flow and stores the minted grant");
    println!("as a Managed Credential. TARGET is a compiled profile or provider");
    println!("({}), a *:oauth integration id, or an", profiles.join(", "));
    println!("integration/instance configured with auth.kind 'oauth-ref'.");
    println!();
    println!("  --device   use the provider's device-code flow when the profile");
    println!("             offers one (for headless shells without a browser)");
    println!();
    println!("The grant lands in the Credential Store under the integration's");
    println!("Credential Reference; expiry refreshes transparently at use time.");
}
