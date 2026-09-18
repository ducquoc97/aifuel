use aifuel_core::{
    AIFUEL_GATEWAY_REGISTRATION_NAME, AgentMcpRegistrationAdapter, AgentMcpRegistrationError,
};
use serde_json::{Map, Value as JsonValue};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};

pub(super) static CODEX_REGISTRATION: CodexMcpRegistration = CodexMcpRegistration;

/// Codex CLI's documented user-level MCP configuration adapter.
pub struct CodexMcpRegistration;

impl AgentMcpRegistrationAdapter for CodexMcpRegistration {
    fn host_id(&self) -> &'static str {
        "codex"
    }

    fn config_file(&self, host_home: &Path) -> PathBuf {
        host_home.join("config.toml")
    }

    fn expected_entry(
        &self,
        gateway_executable: &Path,
    ) -> Result<JsonValue, AgentMcpRegistrationError> {
        canonical_item(&Item::Table(self.registration_table(gateway_executable)?))
    }

    fn current_entry(
        &self,
        config: Option<&[u8]>,
    ) -> Result<Option<JsonValue>, AgentMcpRegistrationError> {
        let document = self.parse_config(config)?;
        let Some(servers) = document.get("mcp_servers") else {
            return Ok(None);
        };
        match servers {
            Item::Table(table) => table
                .get(AIFUEL_GATEWAY_REGISTRATION_NAME)
                .map(canonical_item)
                .transpose(),
            Item::Value(Value::InlineTable(table)) => table
                .get(AIFUEL_GATEWAY_REGISTRATION_NAME)
                .map(canonical_value)
                .transpose(),
            _ => Err(config_error(
                "Codex config.toml mcp_servers must be a TOML table",
            )),
        }
    }

    fn write_entry(
        &self,
        config: Option<&[u8]>,
        gateway_executable: &Path,
    ) -> Result<Vec<u8>, AgentMcpRegistrationError> {
        let mut document = self.parse_config(config)?;
        if document.get("mcp_servers").is_none() {
            document["mcp_servers"] = Item::Table(Table::new());
        }
        let servers = document
            .get_mut("mcp_servers")
            .and_then(Item::as_table_mut)
            .ok_or_else(|| config_error("Codex config.toml mcp_servers must be a TOML table"))?;
        servers.insert(
            AIFUEL_GATEWAY_REGISTRATION_NAME,
            Item::Table(self.registration_table(gateway_executable)?),
        );
        Ok(document.to_string().into_bytes())
    }

    fn remove_entry(&self, config: &[u8]) -> Result<Vec<u8>, AgentMcpRegistrationError> {
        let mut document = self.parse_config(Some(config))?;
        let Some(servers) = document.get_mut("mcp_servers") else {
            return Ok(document.to_string().into_bytes());
        };
        match servers {
            Item::Table(table) => {
                table.remove(AIFUEL_GATEWAY_REGISTRATION_NAME);
            }
            Item::Value(Value::InlineTable(table)) => {
                table.remove(AIFUEL_GATEWAY_REGISTRATION_NAME);
            }
            _ => {
                return Err(config_error(
                    "Codex config.toml mcp_servers must be a TOML table",
                ));
            }
        }
        Ok(document.to_string().into_bytes())
    }
}

impl CodexMcpRegistration {
    fn parse_config(
        &self,
        config: Option<&[u8]>,
    ) -> Result<DocumentMut, AgentMcpRegistrationError> {
        let Some(config) = config else {
            return DocumentMut::from_str("")
                .map_err(|_| config_error("could not create an empty Codex configuration"));
        };
        let text = std::str::from_utf8(config)
            .map_err(|_| config_error("Codex config.toml is not valid UTF-8"))?;
        DocumentMut::from_str(text).map_err(|_| config_error("Codex config.toml is malformed"))
    }

    fn registration_table(
        &self,
        gateway_executable: &Path,
    ) -> Result<Table, AgentMcpRegistrationError> {
        let command = gateway_executable
            .to_str()
            .ok_or_else(|| config_error("AI Fuel executable path is not valid Unicode"))?;
        let mut entry = Table::new();
        entry.insert("command", Value::from(command).into());
        let mut args = Array::new();
        for argument in ["mcp", "gateway", "--agent", self.host_id()] {
            args.push(argument);
        }
        entry.insert("args", Value::Array(args).into());
        let mut env_vars = Array::new();
        env_vars.push("XDG_CONFIG_HOME");
        entry.insert("env_vars", Value::Array(env_vars).into());
        Ok(entry)
    }
}

fn canonical_item(item: &Item) -> Result<JsonValue, AgentMcpRegistrationError> {
    if let Some(value) = item.as_value() {
        return canonical_value(value);
    }
    if let Some(table) = item.as_table() {
        return canonical_table(table);
    }
    if let Some(tables) = item.as_array_of_tables() {
        let values = tables
            .iter()
            .map(canonical_table)
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(typed("array_of_tables", JsonValue::Array(values)));
    }
    Err(config_error(
        "Codex MCP registration has an unsupported TOML value",
    ))
}

fn canonical_table(table: &Table) -> Result<JsonValue, AgentMcpRegistrationError> {
    let mut fields = Map::new();
    for (name, item) in table.iter() {
        fields.insert(name.to_owned(), canonical_item(item)?);
    }
    Ok(typed("table", JsonValue::Object(fields)))
}

fn canonical_inline_table(table: &InlineTable) -> Result<JsonValue, AgentMcpRegistrationError> {
    let mut fields = Map::new();
    for (name, value) in table.iter() {
        fields.insert(name.to_owned(), canonical_value(value)?);
    }
    Ok(typed("table", JsonValue::Object(fields)))
}

fn canonical_value(value: &Value) -> Result<JsonValue, AgentMcpRegistrationError> {
    if let Some(string) = value.as_str() {
        return Ok(typed("string", JsonValue::String(string.to_owned())));
    }
    if let Some(integer) = value.as_integer() {
        return Ok(typed("integer", JsonValue::from(integer)));
    }
    if let Some(float) = value.as_float() {
        return Ok(typed("float", JsonValue::String(float.to_string())));
    }
    if let Some(boolean) = value.as_bool() {
        return Ok(typed("boolean", JsonValue::Bool(boolean)));
    }
    if let Some(datetime) = value.as_datetime() {
        return Ok(typed("datetime", JsonValue::String(datetime.to_string())));
    }
    if let Some(array) = value.as_array() {
        let values = array
            .iter()
            .map(canonical_value)
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(typed("array", JsonValue::Array(values)));
    }
    if let Some(table) = value.as_inline_table() {
        return canonical_inline_table(table);
    }
    Err(config_error(
        "Codex MCP registration has an unsupported TOML value",
    ))
}

fn typed(kind: &str, value: JsonValue) -> JsonValue {
    let mut fields = Map::new();
    fields.insert("type".to_owned(), JsonValue::String(kind.to_owned()));
    fields.insert("value".to_owned(), value);
    JsonValue::Object(fields)
}

fn config_error(detail: &str) -> AgentMcpRegistrationError {
    AgentMcpRegistrationError::new(detail)
}

#[cfg(test)]
#[path = "registration/tests.rs"]
mod tests;
