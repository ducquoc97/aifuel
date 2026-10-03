//! The provider-owned credential files the `copilot:oauth` adapter reads:
//! the first-party CLI's `config.json` (JSONC) and the editor plugins'
//! `hosts.json`/`apps.json` account stores. Parsing lives here so the
//! adapter file reads as run orchestration.

use serde_json::Value;
use std::path::Path;

/// The OAuth token inside `~/.copilot/config.json`, the first-party CLI's
/// own store. The file is JSONC - `//` comment lines head it - so
/// full-line comments are stripped before parsing. `copilotTokens` is a
/// map of `<host>:<login>` to token values; the `lastLoggedInUser` entry
/// wins when several are present, then any entry under its host, then the
/// map's first entry.
pub(crate) fn copilot_config_token(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let stripped: String = text
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let value: Value = serde_json::from_str(&stripped).ok()?;
    let tokens = value.get("copilotTokens")?.as_object()?;
    if tokens.is_empty() {
        return None;
    }
    let token_value = |entry: &Value| {
        entry
            .as_str()
            .or_else(|| entry.get("token").and_then(Value::as_str))
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
    };
    // The logged-in user's entry, then anything under its host, then the
    // first entry - the map is unordered so "first" is only a fallback.
    let logged_in = value.get("lastLoggedInUser");
    let host = logged_in
        .and_then(|entry| entry.get("host").and_then(Value::as_str))
        .unwrap_or("");
    let login = logged_in
        .and_then(|entry| entry.get("login").and_then(Value::as_str))
        .unwrap_or("");
    if let Some(entry) = tokens.get(&format!("{host}:{login}"))
        && let token @ Some(_) = token_value(entry)
    {
        return token;
    }
    if !host.is_empty()
        && let Some(entry) = tokens
            .iter()
            .find(|(key, _)| key.starts_with(host))
            .map(|(_, entry)| entry)
        && let token @ Some(_) = token_value(entry)
    {
        return token;
    }
    tokens.values().find_map(token_value)
}

/// The OAuth token inside an editor-plugin store (`hosts.json` or
/// `apps.json`): `{"<github host>": {"oauth_token": "...", ...}, ...}`.
/// Every top-level value names a GitHub host, so the first entry carrying
/// an `oauth_token` (or `token`) string is returned.
pub(crate) fn github_store_token(path: &Path) -> Option<String> {
    let value: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    value.as_object()?.values().find_map(|entry| {
        entry
            .get("oauth_token")
            .or_else(|| entry.get("token"))
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
    })
}
