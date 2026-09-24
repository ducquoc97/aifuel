use aifuel_app::selection::{ProfileSettings, SelectionSettings, SelectionStore};
use aifuel_core::{AccessMode, ProviderKey};

pub fn run(args: &[String]) -> Result<u8, String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return Ok(0);
    }
    match args.first().map(String::as_str) {
        Some("list") => list(&args[1..]),
        Some("save") => save(&args[1..]),
        Some("remove") => remove(&args[1..]),
        Some(command) => Err(format!("unknown profile command {command:?}; use --help")),
        None => Err("profile requires list, save, or remove".to_owned()),
    }
}

fn list(args: &[String]) -> Result<u8, String> {
    if !args.is_empty() {
        return Err("profile list takes no arguments".to_owned());
    }
    let config = load_config()?;
    println!(
        "{}",
        serde_json::to_string_pretty(&config.profiles)
            .map_err(|error| format!("could not encode profiles: {error}"))?
    );
    Ok(0)
}

fn save(args: &[String]) -> Result<u8, String> {
    let name = args
        .first()
        .ok_or_else(|| "profile save requires NAME".to_owned())?;
    if name.trim().is_empty() || name.starts_with('-') {
        return Err("profile name cannot be empty or start with '-'".to_owned());
    }
    let mut settings = SelectionSettings::default();
    let mut index = 1;
    while index < args.len() {
        let flag = args[index].as_str();
        index += 1;
        let value = args
            .get(index)
            .ok_or_else(|| format!("{flag} requires a value"))?;
        match flag {
            "--provider" => {
                settings.provider = Some(
                    value
                        .parse::<ProviderKey>()
                        .map_err(|error| error.to_string())?,
                );
            }
            "--model" => settings.model = Some(value.clone()),
            "--effort" => settings.effort = Some(value.clone()),
            "--access" => settings.access = Some(AccessMode::parse(value)?),
            "--timeout" => {
                let seconds = value
                    .parse::<u64>()
                    .map_err(|_| "profile timeout must be whole seconds".to_owned())?;
                settings.overall_deadline_seconds = Some(seconds);
            }
            _ => return Err(format!("unknown profile option {flag:?}")),
        }
        index += 1;
    }
    let path = crate::execution_config_path()?;
    let store = SelectionStore::new(path);
    let mut config = store.read().map_err(|error| error.to_string())?;
    config
        .profiles
        .insert(name.clone(), ProfileSettings::from(settings));
    store.write(&config).map_err(|error| error.to_string())?;
    println!("Saved profile {name:?}.");
    Ok(0)
}

fn remove(args: &[String]) -> Result<u8, String> {
    if args.len() != 1 {
        return Err("profile remove requires NAME".to_owned());
    }
    let path = crate::execution_config_path()?;
    let store = SelectionStore::new(path);
    let mut config = store.read().map_err(|error| error.to_string())?;
    if config.profiles.remove(&args[0]).is_none() {
        return Err(format!("profile {:?} was not found", args[0]));
    }
    store.write(&config).map_err(|error| error.to_string())?;
    println!("Removed profile {:?}.", args[0]);
    Ok(0)
}

fn load_config() -> Result<aifuel_app::selection::GlobalSelectionConfig, String> {
    aifuel_app::selection::SelectionStore::load(crate::execution_config_path()?)
        .map_err(|error| error.to_string())
}

fn print_help() {
    println!("Usage: aifuel profile list");
    println!("       aifuel profile save NAME [OPTIONS]");
    println!("       aifuel profile remove NAME");
    println!();
    println!("Options: --provider ID --model ID --effort LEVEL --access MODE --timeout SECONDS");
}
