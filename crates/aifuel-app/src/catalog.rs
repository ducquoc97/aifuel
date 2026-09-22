use crate::agent_mcp_setup::storage::{self, FileSnapshot};
use crate::gateway::McpGatewayFacade;
use serde_json::{Map, Value, json};
use std::fs;
use std::path::{Path, PathBuf};

const CATALOG_VALIDATION_HOST: &str = "__aifuel_catalog_validation__";

/// The selection that an MCP Host receives from the central catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpCatalogSelection {
    /// Replace the shared defaults with these server ids.
    Defaults(Vec<String>),
    /// Replace one host's explicit selection with these server ids.
    Agent { id: String, servers: Vec<String> },
    /// Remove one host's explicit selection so it inherits the defaults.
    Inherit { id: String },
}

/// Manages the per-user external MCP catalog through the same safe file
/// replacement primitives used by Agent MCP Registration.
pub struct McpCatalogFacade {
    catalog_file: PathBuf,
    lock_file: PathBuf,
}

impl McpCatalogFacade {
    pub fn new(catalog_file: impl Into<PathBuf>) -> Self {
        let catalog_file = catalog_file.into();
        let parent = catalog_file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            lock_file: parent.join("mcp.json.lock"),
            catalog_file,
        }
    }

    pub fn catalog_file(&self) -> &Path {
        &self.catalog_file
    }

    /// Return the catalog bytes without rewriting or validating them.
    ///
    /// Listing is intentionally useful for inspecting an invalid catalog.
    /// A missing catalog is represented by the valid empty catalog.
    pub fn list(&self) -> Result<Vec<u8>, McpCatalogError> {
        let snapshot = self.read_snapshot()?;
        Ok(snapshot.contents.unwrap_or_else(|| b"{}".to_vec()))
    }

    /// Validate the complete catalog with the gateway's existing parser and
    /// semantic checks.
    pub fn validate(&self) -> Result<(), McpCatalogError> {
        let bytes = self.list()?;
        validate_catalog_bytes(&bytes)
    }

    /// Add one external MCP server definition under an id.
    pub fn add_server(&self, id: &str, definition: Value) -> Result<(), McpCatalogError> {
        validate_id(id, "MCP server")?;
        if !definition.is_object() {
            return Err(McpCatalogError(
                "MCP server definition must be a JSON object".to_owned(),
            ));
        }
        self.mutate(|catalog| {
            let servers = object_member(catalog, "servers")?;
            if servers.contains_key(id) {
                return Err(McpCatalogError(format!(
                    "MCP server id {id:?} is already present"
                )));
            }
            servers.insert(id.to_owned(), definition);
            Ok(())
        })
    }

    /// Read a definition file and add its JSON object under an id.
    pub fn add_server_from_file(
        &self,
        id: &str,
        definition_file: &Path,
    ) -> Result<(), McpCatalogError> {
        let bytes = fs::read(definition_file).map_err(|error| {
            McpCatalogError(format!(
                "could not read MCP server definition {}: {error}",
                definition_file.display()
            ))
        })?;
        let definition = serde_json::from_slice(&bytes).map_err(|error| {
            McpCatalogError(format!(
                "MCP server definition {} is invalid JSON: {error}",
                definition_file.display()
            ))
        })?;
        self.add_server(id, definition)
    }

    /// Remove an external MCP server when no selection still references it.
    pub fn remove_server(&self, id: &str) -> Result<(), McpCatalogError> {
        validate_id(id, "MCP server")?;
        self.mutate(|catalog| {
            let server_exists = catalog
                .get("servers")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    McpCatalogError("MCP catalog servers must be an object".to_owned())
                })?
                .contains_key(id);
            if !server_exists {
                return Err(McpCatalogError(format!(
                    "MCP server id {id:?} is not present"
                )));
            }

            if selection_contains(catalog, "defaults", id) {
                return Err(McpCatalogError(format!(
                    "MCP server id {id:?} is referenced by defaults; select different defaults before removing it"
                )));
            }
            if let Some(agents) = catalog.get("agents").and_then(Value::as_object) {
                for (agent_id, selection) in agents {
                    if selection
                        .get("servers")
                        .and_then(Value::as_array)
                        .is_some_and(|servers| {
                            servers.iter().any(|server| server.as_str() == Some(id))
                        })
                    {
                        return Err(McpCatalogError(format!(
                            "MCP server id {id:?} is referenced by agent {agent_id:?}; select different servers before removing it"
                        )));
                    }
                }
            }

            let servers = catalog
                .get_mut("servers")
                .and_then(Value::as_object_mut)
                .expect("servers object was checked above");
            servers.remove(id);
            Ok(())
        })
    }

    /// Apply a default or per-agent selection change.
    pub fn select(&self, selection: McpCatalogSelection) -> Result<(), McpCatalogError> {
        match &selection {
            McpCatalogSelection::Defaults(_) => {}
            McpCatalogSelection::Agent { id, .. } | McpCatalogSelection::Inherit { id } => {
                validate_id(id, "MCP Host")?;
            }
        }

        self.mutate(|catalog| {
            match selection {
                McpCatalogSelection::Defaults(servers) => {
                    catalog
                        .as_object_mut()
                        .expect("catalog validation requires an object")
                        .insert("defaults".to_owned(), json!(servers));
                }
                McpCatalogSelection::Agent { id, servers } => {
                    let agents = object_member(catalog, "agents")?;
                    agents.insert(id, json!({"servers": servers}));
                }
                McpCatalogSelection::Inherit { id } => {
                    if let Some(agents) = catalog.get_mut("agents").and_then(Value::as_object_mut) {
                        agents.remove(&id);
                    }
                }
            }
            Ok(())
        })
    }

    pub fn select_defaults(&self, servers: &[String]) -> Result<(), McpCatalogError> {
        self.select(McpCatalogSelection::Defaults(servers.to_vec()))
    }

    pub fn select_agent(&self, id: &str, servers: &[String]) -> Result<(), McpCatalogError> {
        self.select(McpCatalogSelection::Agent {
            id: id.to_owned(),
            servers: servers.to_vec(),
        })
    }

    pub fn inherit_agent(&self, id: &str) -> Result<(), McpCatalogError> {
        self.select(McpCatalogSelection::Inherit { id: id.to_owned() })
    }

    fn read_snapshot(&self) -> Result<FileSnapshot, McpCatalogError> {
        storage::read_snapshot(&self.catalog_file, "MCP catalog")
            .map_err(|error| McpCatalogError(error.to_string()))
    }

    fn mutate<F>(&self, operation: F) -> Result<(), McpCatalogError>
    where
        F: FnOnce(&mut Value) -> Result<(), McpCatalogError>,
    {
        let parent = self.lock_file.parent().ok_or_else(|| {
            McpCatalogError("MCP catalog lock has no parent directory".to_owned())
        })?;
        storage::create_private_dir_all(parent)
            .map_err(|error| McpCatalogError(error.to_string()))?;
        let _lock = storage::acquire_lock(&self.lock_file)
            .map_err(|error| McpCatalogError(error.to_string()))?;

        let snapshot = self.read_snapshot()?;
        let current = snapshot.contents.as_deref().unwrap_or(br"{}");
        validate_catalog_bytes(current)?;
        let mut catalog = parse_catalog(current)?;
        operation(&mut catalog)?;
        let updated = serde_json::to_vec_pretty(&catalog)
            .map_err(|error| McpCatalogError(format!("could not encode MCP catalog: {error}")))?;
        validate_catalog_bytes(&updated)?;
        if updated == current {
            return Ok(());
        }

        storage::replace_config_with_backup(&self.catalog_file, &snapshot, &updated, None)
            .map_err(|error| McpCatalogError(error.to_string()))
    }
}

fn parse_catalog(bytes: &[u8]) -> Result<Value, McpCatalogError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        McpCatalogError(format!(
            "invalid MCP gateway JSON near line {} column {}",
            error.line(),
            error.column()
        ))
    })?;
    if !value.is_object() {
        return Err(McpCatalogError(
            "MCP catalog must be a JSON object".to_owned(),
        ));
    }
    Ok(value)
}

fn validate_catalog_bytes(bytes: &[u8]) -> Result<(), McpCatalogError> {
    McpGatewayFacade::from_json(bytes, CATALOG_VALIDATION_HOST, PathBuf::new())
        .map(|_| ())
        .map_err(|error| McpCatalogError(error.to_string()))
}

fn object_member<'a>(
    catalog: &'a mut Value,
    name: &str,
) -> Result<&'a mut Map<String, Value>, McpCatalogError> {
    let root = catalog
        .as_object_mut()
        .ok_or_else(|| McpCatalogError("MCP catalog must be a JSON object".to_owned()))?;
    if !root.contains_key(name) {
        root.insert(name.to_owned(), Value::Object(Map::new()));
    }
    root.get_mut(name)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| McpCatalogError(format!("MCP catalog {name} must be an object")))
}

fn selection_contains(catalog: &Value, location: &str, id: &str) -> bool {
    catalog
        .get(location)
        .and_then(Value::as_array)
        .is_some_and(|selection| selection.iter().any(|value| value.as_str() == Some(id)))
}

fn validate_id(id: &str, kind: &str) -> Result<(), McpCatalogError> {
    if id.trim().is_empty() {
        return Err(McpCatalogError(format!("{kind} id cannot be empty")));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpCatalogError(String);

impl std::fmt::Display for McpCatalogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for McpCatalogError {}
