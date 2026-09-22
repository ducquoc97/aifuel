use aifuel_app::{McpCatalogFacade, McpCatalogSelection};
use std::path::Path;

/// Run the external MCP catalog management command.
pub fn run(args: &[String]) -> Result<u8, String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return Ok(0);
    }
    let command = args
        .first()
        .map(String::as_str)
        .ok_or_else(|| "mcp servers requires list, validate, add, remove, or select".to_owned())?;
    let catalog = catalog_facade()?;
    match command {
        "list" => run_list(&catalog, &args[1..]),
        "validate" => run_validate(&catalog, &args[1..]),
        "add" => run_add(&catalog, &args[1..]),
        "remove" => run_remove(&catalog, &args[1..]),
        "select" => run_select(&catalog, &args[1..]),
        unknown => Err(format!(
            "unknown MCP servers command {unknown:?}; use --help"
        )),
    }
}

fn catalog_facade() -> Result<McpCatalogFacade, String> {
    let user_home = super::user_home_dir()?;
    let config_file = super::user_config_dir(&user_home)?
        .join("aifuel")
        .join("mcp.json");
    Ok(McpCatalogFacade::new(config_file))
}

fn run_list(catalog: &McpCatalogFacade, args: &[String]) -> Result<u8, String> {
    if !args.is_empty() {
        return Err(format!(
            "unexpected argument {:?} for mcp servers list",
            args[0]
        ));
    }
    let bytes = catalog.list().map_err(|error| error.to_string())?;
    let text = String::from_utf8(bytes).map_err(|_| {
        "MCP catalog is not valid UTF-8; configuration was left untouched".to_owned()
    })?;
    println!("{text}");
    Ok(0)
}

fn run_validate(catalog: &McpCatalogFacade, args: &[String]) -> Result<u8, String> {
    if !args.is_empty() {
        return Err(format!(
            "unexpected argument {:?} for mcp servers validate",
            args[0]
        ));
    }
    catalog.validate().map_err(|error| error.to_string())?;
    println!("MCP catalog is valid.");
    Ok(0)
}

fn run_add(catalog: &McpCatalogFacade, args: &[String]) -> Result<u8, String> {
    let id = args
        .first()
        .ok_or_else(|| "mcp servers add requires SERVER_ID".to_owned())?;
    if id.starts_with('-') {
        return Err("mcp servers add requires SERVER_ID before options".to_owned());
    }
    let mut definition = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--definition" => {
                if definition.is_some() {
                    return Err("mcp servers add accepts one --definition value".to_owned());
                }
                index += 1;
                definition = Some(
                    args.get(index)
                        .ok_or_else(|| "--definition requires a file path".to_owned())?
                        .clone(),
                );
            }
            unknown => {
                return Err(format!("unknown argument {unknown:?} for mcp servers add"));
            }
        }
        index += 1;
    }
    let definition = definition
        .ok_or_else(|| "mcp servers add requires --definition DEFINITION_FILE".to_owned())?;
    let definition_path = Path::new(&definition);
    catalog
        .add_server_from_file(id, definition_path)
        .map_err(|error| error.to_string())?;
    println!("Added MCP server {id:?}.");
    Ok(0)
}

fn run_remove(catalog: &McpCatalogFacade, args: &[String]) -> Result<u8, String> {
    if args.len() != 1 || args[0].starts_with('-') {
        return Err("mcp servers remove requires SERVER_ID".to_owned());
    }
    catalog
        .remove_server(&args[0])
        .map_err(|error| error.to_string())?;
    println!("Removed MCP server {:?}.", args[0]);
    Ok(0)
}

fn run_select(catalog: &McpCatalogFacade, args: &[String]) -> Result<u8, String> {
    let mut agent = None;
    let mut defaults = false;
    let mut inherit = false;
    let mut servers = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--agent" => {
                if agent.is_some() {
                    return Err("mcp servers select accepts one --agent value".to_owned());
                }
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--agent requires MCP_HOST_ID".to_owned())?;
                if value.starts_with('-') {
                    return Err("--agent requires MCP_HOST_ID".to_owned());
                }
                agent = Some(value.clone());
            }
            "--defaults" => {
                if defaults {
                    return Err("mcp servers select accepts one --defaults flag".to_owned());
                }
                defaults = true;
            }
            "--inherit" => {
                if inherit {
                    return Err("mcp servers select accepts one --inherit flag".to_owned());
                }
                inherit = true;
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown argument {value:?} for mcp servers select"));
            }
            server => servers.push(server.to_owned()),
        }
        index += 1;
    }

    if defaults && agent.is_some() {
        return Err("mcp servers select accepts either --defaults or --agent, not both".to_owned());
    }
    if inherit && defaults {
        return Err("--inherit requires --agent MCP_HOST_ID".to_owned());
    }
    let selection = if defaults {
        McpCatalogSelection::Defaults(servers)
    } else {
        let id = agent.ok_or_else(|| {
            "mcp servers select requires --defaults or --agent MCP_HOST_ID".to_owned()
        })?;
        if inherit {
            if !servers.is_empty() {
                return Err("--inherit cannot be combined with SERVER_ID values".to_owned());
            }
            McpCatalogSelection::Inherit { id }
        } else {
            McpCatalogSelection::Agent { id, servers }
        }
    };

    catalog
        .select(selection)
        .map_err(|error| error.to_string())?;
    if inherit {
        println!("MCP Host now inherits the catalog defaults.");
    } else if defaults {
        println!("Updated MCP catalog defaults.");
    } else {
        println!("Updated MCP Host selection.");
    }
    Ok(0)
}

fn print_help() {
    println!("Usage: aifuel mcp servers <COMMAND>");
    println!();
    println!("Commands:");
    println!("  list");
    println!("  validate");
    println!("  add SERVER_ID --definition FILE");
    println!("  remove SERVER_ID");
    println!("  select --defaults [SERVER_ID ...]");
    println!("  select --agent MCP_HOST_ID [SERVER_ID ...]");
    println!("  select --agent MCP_HOST_ID --inherit");
}
