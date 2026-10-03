//! Named route resolution for the `/v1` gateway: model aliases and
//! ordered combos from `gateway.json` in the config dir.
//!
//! ```json
//! {"aliases": {"cheap": "groq:api-key/llama-3.3-70b"},
//!  "combos": {"heavy": ["codex", "copilot", "devin"]}}
//! ```
//!
//! An alias substitutes one selector string for another - the caller
//! re-resolves the substituted value, so an alias may chain onto another
//! alias, a combo, or any built-in addressing form. A combo is an ordered
//! selector list the caller expands into an attempt chain; it is not a
//! merge - order is the failover rank.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// The config file name inside `aifuel_config_dir()`.
const FILE_NAME: &str = "gateway.json";

/// What a configured name resolves to when an inbound `model` string
/// matches it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Resolution {
    /// Rewrite the selector to another model string and re-resolve.
    Alias(String),
    /// An ordered chain of selectors to attempt in place of `auto`.
    Combo(Vec<String>),
}

/// One parsed `gateway.json` document. Both maps are optional - an
/// absent or empty file configures no names.
#[derive(Debug, Default, Deserialize)]
struct Config {
    #[serde(default)]
    aliases: BTreeMap<String, String>,
    #[serde(default)]
    combos: BTreeMap<String, Vec<String>>,
}

impl Config {
    /// Look `model` up by name. Aliases win over combos on a collision:
    /// an alias is a verbatim rewrite applied before the name means
    /// anything else, so it shadows a combo carrying the same name.
    fn resolve(&self, model: &str) -> Option<Resolution> {
        if let Some(target) = self.aliases.get(model) {
            return Some(Resolution::Alias(target.clone()));
        }
        self.combos
            .get(model)
            .map(|selectors| Resolution::Combo(selectors.clone()))
    }
}

/// Parse one `gateway.json` document. A malformed document is an error,
/// never a silent empty config - a broken route table must fail loudly or
/// requests route somewhere the user never configured.
fn parse(text: &str) -> Result<Config, String> {
    serde_json::from_str(text).map_err(|error| format!("gateway.json is malformed: {error}"))
}

/// Load `gateway.json` from `path`. A missing file is an empty config -
/// named routes are optional - while an unreadable or malformed file is
/// an error.
fn load_from(path: &Path) -> Result<Config, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(error) => Err(format!("could not read {}: {error}", path.display())),
    }
}

/// Resolve `model` against configured aliases and combos. The file is
/// re-read on every call: `gateway.json` is small, and a per-request read
/// keeps edits and revocation instant - no cache means no stale-name
/// window and no reload signal to wire. `Ok(None)` means no configured
/// name matched and the caller falls back to the built-in addressing
/// convention.
pub(crate) fn resolve(model: &str) -> Result<Option<Resolution>, String> {
    let path = crate::aifuel_config_dir()?.join(FILE_NAME);
    Ok(load_from(&path)?.resolve(model))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aifuel-routes-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test dir should be creatable");
        dir
    }

    #[test]
    fn alias_returns_the_verbatim_substitution() {
        // An alias is a rewrite, not a resolved target: the value comes
        // back exactly as written so the caller can re-resolve it -
        // chaining onto another name or any built-in addressing form.
        let config =
            parse(r#"{"aliases": {"cheap": "groq:api-key/llama-3.3-70b", "nick": "cheap"}}"#)
                .expect("the document parses");
        assert_eq!(
            config.resolve("cheap"),
            Some(Resolution::Alias("groq:api-key/llama-3.3-70b".to_owned()))
        );
        // `nick` stays `cheap` - one hop per call keeps cycles the
        // caller's recursion problem, not a silent loop here.
        assert_eq!(
            config.resolve("nick"),
            Some(Resolution::Alias("cheap".to_owned()))
        );
    }

    #[test]
    fn combo_returns_selectors_in_declared_order() {
        // Order is the failover rank: `resolve_attempts` expands the list
        // front to back, so the config must come back unshuffled.
        let config = parse(r#"{"combos": {"heavy": ["codex", "copilot", "devin"], "empty": []}}"#)
            .expect("the document parses");
        assert_eq!(
            config.resolve("heavy"),
            Some(Resolution::Combo(vec![
                "codex".to_owned(),
                "copilot".to_owned(),
                "devin".to_owned()
            ]))
        );
        // An empty list stays verbatim - the caller reports "resolved no
        // candidates", the resolver does not editorialize the config.
        assert_eq!(config.resolve("empty"), Some(Resolution::Combo(Vec::new())));
    }

    #[test]
    fn an_alias_shadows_a_combo_of_the_same_name() {
        // One name can mean only one thing; the rewrite applies first, so
        // a combo colliding with an alias is unreachable by that name.
        let config = parse(r#"{"aliases": {"x": "codex"}, "combos": {"x": ["copilot"]}}"#)
            .expect("the document parses");
        assert_eq!(
            config.resolve("x"),
            Some(Resolution::Alias("codex".to_owned()))
        );
    }

    #[test]
    fn an_unmatched_name_falls_through_to_builtin_addressing() {
        // `None` is the contract for "not a configured name" - the caller
        // treats the string as a selector or catalog model id.
        let config = parse(r#"{"aliases": {"cheap": "codex"}}"#).expect("the document parses");
        assert_eq!(config.resolve("auto"), None);
        assert_eq!(Config::default().resolve("anything"), None);
    }

    #[test]
    fn malformed_json_is_an_error_not_an_empty_config() {
        // A broken route table must fail loudly: silently resolving no
        // names would route requests through addressing the user never
        // configured, and a typo would look like "routes are ignored".
        assert!(parse("{").is_err());
        assert!(parse("not json").is_err());
        assert!(parse(r#"{"aliases": {"x": 5}}"#).is_err());
        assert!(parse(r#"{"combos": {"x": "codex"}}"#).is_err());
    }

    #[test]
    fn a_missing_file_resolves_nothing() {
        // Named routes are optional: no `gateway.json`, no configured
        // names, every model falls through to built-in addressing.
        let dir = test_dir("missing");
        let config =
            load_from(&dir.join(FILE_NAME)).expect("a missing file loads as the empty config");
        assert_eq!(config.resolve("cheap"), None);
        std::fs::remove_dir_all(&dir).expect("test dir should be removable");
    }

    #[test]
    fn load_from_reads_and_parses_a_real_file() {
        // The production path goes through the filesystem, so cover
        // `load_from` end to end - not just `parse` on a string.
        let dir = test_dir("load");
        std::fs::write(
            dir.join(FILE_NAME),
            r#"{"aliases": {"sonnet": "claude/claude-sonnet-4-5"}, "combos": {"fast": ["groq:api-key", "auto"]}}"#,
        )
        .expect("the fixture writes");
        let config = load_from(&dir.join(FILE_NAME)).expect("the file loads");
        assert_eq!(
            config.resolve("sonnet"),
            Some(Resolution::Alias("claude/claude-sonnet-4-5".to_owned()))
        );
        assert_eq!(
            config.resolve("fast"),
            Some(Resolution::Combo(vec![
                "groq:api-key".to_owned(),
                "auto".to_owned()
            ]))
        );
        std::fs::remove_dir_all(&dir).expect("test dir should be removable");
    }
}
