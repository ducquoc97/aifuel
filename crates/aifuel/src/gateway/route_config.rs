//! `GET|PUT /api/gateway/routes`: read and replace the `gateway.json`
//! route table - the aliases and combos `routes::resolve` consults for
//! every inbound `/v1` model string.
//!
//! `GET` answers `{"aliases": {...}, "combos": {...}}` - the file's own
//! schema, with empty objects when no file exists. `PUT` replaces the
//! whole table: the body is the same shape, validated so only tables
//! `resolve` can act on get written, then swapped in atomically so a
//! concurrent `/v1` reader never sees a torn file. `resolve` re-reads
//! the file on every call, so a successful PUT takes effect on the very
//! next request - no reload step and no stale-name window.

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use tiny_http::Request;

/// The file `routes` reads - repeated because `routes::FILE_NAME` is
/// private to that module.
const FILE_NAME: &str = "gateway.json";

/// Route tables are small - a handful of names and selectors - so the
/// dashboard's own mutation bound applies, same as `admin`'s.
const MAX_BODY_BYTES: u64 = 8 * 1024;

/// Unique suffixes for temporary siblings, the `keys` write pattern.
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// One `gateway.json` document - the serde twin of `routes::Config`,
/// which stays private to that module. Field names, types, and the
/// `default` posture must track it exactly: same schema, parsed here
/// for the admin surface. Unknown fields are ignored, matching
/// `routes::Config`; the write then persists only this shape.
#[derive(Debug, Default, Deserialize, Serialize)]
struct RouteTable {
    #[serde(default)]
    aliases: BTreeMap<String, String>,
    #[serde(default)]
    combos: BTreeMap<String, Vec<String>>,
    /// Optional `models` map - integration id to the model ids this
    /// deployment advertises to client pickers. It is user-managed and
    /// carried untouched through routes PUTs: an empty map omits the
    /// key entirely.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    models: BTreeMap<String, Vec<DeclaredModel>>,
}

/// One declared model: the bare `"model-id"` string, or
/// `{"id": ..., "label": ..., "efforts": [...], "default_effort": ...}`
/// when the picker should offer reasoning effort for it.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub(crate) enum DeclaredModel {
    Id(String),
    Full {
        id: String,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        efforts: Vec<String>,
        #[serde(default)]
        default_effort: Option<String>,
    },
}

impl DeclaredModel {
    pub(crate) fn id(&self) -> &str {
        match self {
            DeclaredModel::Id(id) => id,
            DeclaredModel::Full { id, .. } => id,
        }
    }

    pub(crate) fn label(&self) -> Option<&str> {
        match self {
            DeclaredModel::Id(_) => None,
            DeclaredModel::Full { label, .. } => label.as_deref(),
        }
    }

    pub(crate) fn efforts(&self) -> &[String] {
        match self {
            DeclaredModel::Id(_) => &[],
            DeclaredModel::Full { efforts, .. } => efforts,
        }
    }

    pub(crate) fn default_effort(&self) -> Option<&str> {
        match self {
            DeclaredModel::Id(_) => None,
            DeclaredModel::Full { default_effort, .. } => default_effort.as_deref(),
        }
    }
}

/// `GET /api/gateway/routes`: the configured aliases and combos.
pub(crate) fn get(request: Request) {
    match config_path().and_then(|path| load(&path)) {
        Ok(table) => respond_json(request, 200, &table),
        Err(error) => respond_route_error(request, 500, &error),
    }
}

/// `PUT|POST /api/gateway/routes`: replace the configured aliases and
/// combos. Body and validation failures are 400s; the config directory
/// itself failing is a 500.
pub(crate) fn put(mut request: Request) {
    let table = match read_json_body(&mut request) {
        Ok(table) => table,
        Err(error) => return respond_route_error(request, 400, &error),
    };
    if let Err(error) = validate(&table) {
        return respond_route_error(request, 400, &error);
    }
    match config_path().and_then(|path| save(&path, &table)) {
        Ok(()) => respond_json(request, 200, &json!({"ok": true})),
        Err(error) => respond_route_error(request, 500, &error),
    }
}

fn config_path() -> Result<PathBuf, String> {
    Ok(crate::aifuel_config_dir()?.join(FILE_NAME))
}

/// The `models` map declared in `gateway.json`, re-read per call the
/// same way `resolve` re-reads routes. The release-shipped defaults in
/// `builtin_models.json` merge under it - user entries win on the same
/// model id, so a hand declaration can correct or extend what a release
/// ships. A missing or malformed file falls back to the bundled list
/// alone - `models::list` must never break over it.
pub(crate) fn declared_models() -> BTreeMap<String, Vec<DeclaredModel>> {
    let user = config_path()
        .and_then(|path| load(&path))
        .map(|table| table.models)
        .unwrap_or_default();
    merge_declared(builtin_models().clone(), user)
}

/// The model list a release ships, bundled into the binary the way
/// LiteLLM ships its model registry: maintainers update
/// `builtin_models.json` per release and users never have to declare
/// the defaults by hand.
fn builtin_models() -> &'static BTreeMap<String, Vec<DeclaredModel>> {
    static BUILTIN: OnceLock<BTreeMap<String, Vec<DeclaredModel>>> = OnceLock::new();
    BUILTIN.get_or_init(|| {
        serde_json::from_str(include_str!("builtin_models.json"))
            .expect("builtin model declarations must parse")
    })
}

/// User declarations ride over the bundled defaults: an id the user
/// re-declares replaces that entry in place, new ids append.
fn merge_declared(
    mut declared: BTreeMap<String, Vec<DeclaredModel>>,
    user: BTreeMap<String, Vec<DeclaredModel>>,
) -> BTreeMap<String, Vec<DeclaredModel>> {
    for (integration, models) in user {
        let merged = declared.entry(integration).or_default();
        for model in models {
            match merged.iter_mut().find(|existing| existing.id() == model.id()) {
                Some(existing) => *existing = model,
                None => merged.push(model),
            }
        }
    }
    declared
}

/// Read the route table from `path`, mirroring `routes::load_from`: a
/// missing file is the empty table while an unreadable or malformed
/// file is an error - a broken route table must fail loudly, not look
/// like "nothing is configured".
fn load(path: &Path) -> Result<RouteTable, String> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|error| format!("gateway.json is malformed: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(RouteTable::default()),
        Err(error) => Err(format!("could not read {}: {error}", path.display())),
    }
}

/// Reject tables `resolve` could never usefully act on: an empty name
/// can only match an empty inbound `model` string, an empty alias
/// target rewrites to a string nothing resolves, and an empty combo
/// plans zero candidates. A name present in both maps is allowed - the
/// alias shadows the combo, the same precedence `routes::Config::resolve`
/// applies to a hand-edited file.
fn validate(table: &RouteTable) -> Result<(), String> {
    for (name, target) in &table.aliases {
        if name.trim().is_empty() {
            return Err("alias names must be non-empty".to_owned());
        }
        if target.trim().is_empty() {
            return Err(format!("alias {name:?} must name a non-empty target"));
        }
    }
    for (name, selectors) in &table.combos {
        if name.trim().is_empty() {
            return Err("combo names must be non-empty".to_owned());
        }
        if selectors.is_empty() {
            return Err(format!("combo {name:?} needs at least one selector"));
        }
        if selectors.iter().any(|selector| selector.trim().is_empty()) {
            return Err(format!("combo {name:?} has an empty selector"));
        }
    }
    for (integration, models) in &table.models {
        if integration.trim().is_empty() {
            return Err("model declarations need a non-empty integration id".to_owned());
        }
        for model in models {
            if model.id().trim().is_empty() {
                return Err(format!("models declared for {integration:?} need a non-empty id"));
            }
            if model.efforts().iter().any(|effort| effort.trim().is_empty()) {
                return Err(format!(
                    "declared model {:?} has an empty effort",
                    model.id()
                ));
            }
            if model.default_effort().is_some_and(|effort| effort.trim().is_empty()) {
                return Err(format!(
                    "declared model {:?} has an empty default effort",
                    model.id()
                ));
            }
        }
    }
    Ok(())
}

/// Replace the route file atomically: serialize to a unique temporary
/// sibling, then rename over the destination so a `resolve` call racing
/// the write never sees a torn table. The owner-only mode matches the
/// `keys` store convention in the same directory.
fn save(path: &Path, table: &RouteTable) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(table)
        .map_err(|error| format!("could not serialize the route table: {error}"))?;
    let Some(parent) = path.parent() else {
        return Err("the gateway route path has no parent directory".to_owned());
    };
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    let temp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(FILE_NAME),
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed),
    ));
    write_private(&temp, &bytes).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })?;
    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("could not replace {}: {error}", path.display())
    })
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(bytes))
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    fs::write(path, bytes).map_err(|error| format!("could not write {}: {error}", path.display()))
}

/// One JSON answer in the dashboard's envelope - the same conventions
/// `admin` uses, re-declared because its helpers are private to it.
fn respond_json(request: Request, status: u16, value: &impl Serialize) {
    match serde_json::to_vec_pretty(value) {
        Ok(body) => super::respond(
            request,
            status,
            body,
            Some("application/json; charset=utf-8"),
            super::cors_headers(),
        ),
        Err(_) => super::respond(
            request,
            500,
            b"{\"error\": \"could not serialize the response\"}".to_vec(),
            Some("application/json; charset=utf-8"),
            super::cors_headers(),
        ),
    }
}

/// `{"error": "..."}` - the dashboard's error shape, not the OpenAI
/// envelope `/v1` uses; this is a dashboard surface.
fn respond_route_error(request: Request, status: u16, message: &str) {
    respond_json(request, status, &json!({"error": message}));
}

/// Decode a JSON mutation body bounded by `MAX_BODY_BYTES`. Requiring
/// `application/json` is part of the CSRF posture the dashboard relies
/// on - `admin`'s identical copy carries the full rationale.
fn read_json_body(request: &mut Request) -> Result<RouteTable, String> {
    let content_type = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Content-Type"))
        .map(|header| header.value.as_str().to_string())
        .unwrap_or_default();
    if !content_type.starts_with("application/json") {
        // Drain the rejected body: closing a socket while the kernel
        // still holds unread request bytes can RST the connection before
        // the client reads the rejection.
        let mut reader = request.as_reader().take(MAX_BODY_BYTES + 1);
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
        return Err("expected an application/json request body".to_owned());
    }
    let mut body = String::new();
    request
        .as_reader()
        .take(MAX_BODY_BYTES + 1)
        .read_to_string(&mut body)
        .map_err(|error| format!("could not read the request body: {error}"))?;
    if body.len() as u64 > MAX_BODY_BYTES {
        return Err("the request body is too large".to_owned());
    }
    serde_json::from_str(&body).map_err(|error| format!("invalid JSON body: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aifuel-route-config-test-{}-{name}",
            std::process::id()
        ));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("stale test dir removable");
        }
        std::fs::create_dir_all(&dir).expect("test dir creatable");
        dir
    }

    #[test]
    fn a_missing_file_loads_as_the_empty_table() {
        // `gateway.json` is optional: `GET` must answer empty objects,
        // not an error, when nothing is configured yet.
        let dir = test_dir("missing");
        let table = load(&dir.join(FILE_NAME)).expect("a missing file loads");
        let body = serde_json::to_value(&table).expect("serializes");
        assert_eq!(body, json!({"aliases": {}, "combos": {}}));
        std::fs::remove_dir_all(&dir).expect("test dir removable");
    }

    #[test]
    fn a_saved_table_round_trips_in_the_schema_resolve_reads() {
        // The write path must produce the exact `gateway.json` shape
        // `routes` parses - two maps named `aliases` and `combos` - so a
        // PUT takes effect on the very next `/v1` request.
        let dir = test_dir("round-trip");
        let path = dir.join(FILE_NAME);
        let mut table = RouteTable::default();
        table
            .aliases
            .insert("cheap".to_owned(), "groq:api-key/llama-3.3-70b".to_owned());
        table.combos.insert(
            "heavy".to_owned(),
            vec!["codex".to_owned(), "copilot".to_owned()],
        );
        save(&path, &table).expect("save");

        let on_disk: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("file readable"))
                .expect("the file is JSON");
        assert_eq!(on_disk["aliases"]["cheap"], "groq:api-key/llama-3.3-70b");
        assert_eq!(on_disk["combos"]["heavy"], json!(["codex", "copilot"]));
        let keys: BTreeSet<&str> = on_disk
            .as_object()
            .expect("the document is an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            BTreeSet::from(["aliases", "combos"]),
            "the file carries exactly the schema routes::Config deserializes"
        );

        let back = load(&path).expect("the saved file loads back");
        assert_eq!(back.aliases, table.aliases);
        assert_eq!(back.combos, table.combos);
        std::fs::remove_dir_all(&dir).expect("test dir removable");
    }

    #[test]
    fn declared_models_parse_as_ids_or_full_entries_and_round_trip() {
        // The `models` map accepts both spellings and survives a
        // save/load cycle so routes PUTs never drop declarations.
        let table: RouteTable = serde_json::from_str(
            r#"{"models": {"claude": ["sonnet-4-6", {"id": "opus-4-1", "efforts": ["high"], "default_effort": "high", "label": "Opus"}]}}"#,
        )
        .expect("declared models decode");
        let claude = &table.models["claude"];
        assert_eq!(claude[0].id(), "sonnet-4-6");
        assert_eq!(claude[0].efforts(), &[] as &[String]);
        assert_eq!(claude[1].id(), "opus-4-1");
        assert_eq!(claude[1].efforts(), &["high".to_owned()]);
        assert_eq!(claude[1].default_effort(), Some("high"));
        assert_eq!(claude[1].label(), Some("Opus"));
        assert!(serde_json::from_str::<RouteTable>(r#"{"models": {"claude": "x"}}"#).is_err());
        assert!(
            serde_json::from_str::<RouteTable>(r#"{"models": {"claude": [{"label": "x"}]}}"#)
                .is_err(),
            "a full entry without id cannot name a model"
        );

        let dir = test_dir("declared");
        let path = dir.join(FILE_NAME);
        save(&path, &table).expect("save");
        let back = load(&path).expect("load back");
        assert_eq!(back.models["claude"].len(), 2);
        std::fs::remove_dir_all(&dir).expect("test dir removable");
    }

    #[test]
    fn validation_rejects_declared_models_the_picker_cannot_use() {
        let declared = |integration: &str, models: Vec<DeclaredModel>| RouteTable {
            models: BTreeMap::from([(integration.to_owned(), models)]),
            ..RouteTable::default()
        };
        assert!(validate(&declared("claude", vec![DeclaredModel::Id("x".into())])).is_ok());
        assert!(
            validate(&declared("", vec![DeclaredModel::Id("x".into())])).is_err(),
            "a blank integration id can never match an adapter"
        );
        assert!(
            validate(&declared("claude", vec![DeclaredModel::Id("  ".into())])).is_err(),
            "a blank model id emits no selectable entry"
        );
        assert!(
            validate(&declared(
                "claude",
                vec![serde_json::from_str(r#"{"id":"x","efforts":[""]}"#).unwrap()],
            ))
            .is_err(),
            "an empty effort strings looks like an option but selects nothing"
        );
    }

    #[test]
    fn an_empty_models_map_serializes_away() {
        // `models` is optional: an empty map must not start appearing
        // in files that never declared one.
        let body = serde_json::to_value(&RouteTable::default()).expect("serializes");
        assert_eq!(body, json!({"aliases": {}, "combos": {}}));
    }

    #[test]
    fn the_bundled_release_list_parses_in_the_declared_schema() {
        // `builtin_models.json` ships in the binary: a malformed file
        // would panic every models call, so it is parse-checked here.
        let builtin = builtin_models();
        assert!(!builtin.is_empty(), "the release list ships empty");
        for (integration, models) in builtin {
            assert!(
                !integration.trim().is_empty(),
                "a bundled key must name an integration"
            );
            assert!(
                models.iter().all(|model| !model.id().trim().is_empty()),
                "bundled models for {integration} need non-empty ids"
            );
        }
    }

    #[test]
    fn user_declarations_override_bundled_entries_and_append_new_ones() {
        let mut builtin = BTreeMap::new();
        builtin.insert(
            "codex".to_owned(),
            vec![
                DeclaredModel::Id("gpt-a".to_owned()),
                serde_json::from_str(r#"{"id":"gpt-b","efforts":["low"]}"#).unwrap(),
            ],
        );
        let mut user = BTreeMap::new();
        user.insert(
            "codex".to_owned(),
            vec![
                serde_json::from_str::<DeclaredModel>(r#"{"id":"gpt-b","efforts":["max"]}"#)
                    .unwrap(),
                DeclaredModel::Id("gpt-c".to_owned()),
            ],
        );
        user.insert(
            "claude".to_owned(),
            vec![DeclaredModel::Id("sonnet".to_owned())],
        );

        let merged = merge_declared(builtin, user);
        let codex = &merged["codex"];
        assert_eq!(codex.len(), 3, "override replaces in place, not a duplicate");
        assert_eq!(codex[0].id(), "gpt-a");
        assert_eq!(
            codex[1].efforts(),
            &["max".to_owned()],
            "the user's declaration wins over the bundled entry"
        );
        assert_eq!(codex[2].id(), "gpt-c");
        assert_eq!(merged["claude"][0].id(), "sonnet");
    }

    #[test]
    fn a_malformed_file_errors_instead_of_reading_empty() {
        // The same contract `routes::load_from` enforces on `/v1`: a
        // broken route table fails loudly rather than looking like an
        // empty one.
        let dir = test_dir("malformed");
        let path = dir.join(FILE_NAME);
        fs::write(&path, "not json").expect("seed malformed file");
        assert!(load(&path).is_err());
        std::fs::remove_dir_all(&dir).expect("test dir removable");
    }

    #[test]
    fn bodies_follow_the_file_schema() {
        // The PUT body is the `gateway.json` document itself: valid
        // shapes decode, wrong-typed values and non-objects reject, and
        // unknown keys are ignored - `routes::Config` does not deny
        // them, so the write path does not either.
        let table: RouteTable = serde_json::from_str(
            r#"{"aliases": {"cheap": "codex"}, "combos": {"fast": ["auto"]}, "extra": 1}"#,
        )
        .expect("a document with an unknown field still decodes");
        assert_eq!(table.aliases["cheap"], "codex");
        assert!(serde_json::from_str::<RouteTable>("{}").is_ok());
        assert!(serde_json::from_str::<RouteTable>(r#"{"aliases": {"x": 5}}"#).is_err());
        assert!(serde_json::from_str::<RouteTable>(r#"{"combos": {"x": "codex"}}"#).is_err());
        assert!(serde_json::from_str::<RouteTable>("[1]").is_err());
        assert!(serde_json::from_str::<RouteTable>("not json").is_err());
    }

    #[test]
    fn validation_rejects_tables_resolve_cannot_use() {
        let table = |aliases: &[(&str, &str)], combos: &[(&str, &[&str])]| RouteTable {
            aliases: aliases
                .iter()
                .map(|(name, target)| (name.to_string(), target.to_string()))
                .collect(),
            combos: combos
                .iter()
                .map(|(name, selectors)| {
                    (
                        name.to_string(),
                        selectors.iter().map(|s| s.to_string()).collect(),
                    )
                })
                .collect(),
            ..RouteTable::default()
        };
        assert!(validate(&table(&[("cheap", "codex")], &[("fast", &["auto"])])).is_ok());
        assert!(
            validate(&table(&[("", "codex")], &[])).is_err(),
            "an empty name can only match an empty inbound model"
        );
        assert!(
            validate(&table(&[("  ", "codex")], &[])).is_err(),
            "whitespace-only is empty for resolution purposes"
        );
        assert!(
            validate(&table(&[("cheap", "")], &[])).is_err(),
            "an empty target rewrites to a string nothing resolves"
        );
        assert!(validate(&table(&[("cheap", "  ")], &[])).is_err());
        assert!(
            validate(&table(&[], &[("fast", &[])])).is_err(),
            "an empty combo plans zero candidates"
        );
        assert!(
            validate(&table(&[], &[("fast", &["codex", ""])])).is_err(),
            "an empty selector falls through to catalog resolution"
        );
    }

    #[test]
    fn a_name_in_both_maps_is_allowed_the_alias_shadows() {
        // `routes::Config::resolve` lets an alias shadow a combo of the
        // same name rather than rejecting the file; the write path
        // matches that precedence instead of inventing a stricter rule.
        let mut table = RouteTable::default();
        table.aliases.insert("x".to_owned(), "codex".to_owned());
        table
            .combos
            .insert("x".to_owned(), vec!["copilot".to_owned()]);
        assert!(validate(&table).is_ok());
    }

    #[test]
    fn save_replaces_the_table_and_leaves_no_temp_sibling() {
        // PUT is whole-table replacement, not a merge: a second save
        // fully overwrites the first, and the atomic rename leaves no
        // temp file behind.
        let dir = test_dir("replace");
        let path = dir.join(FILE_NAME);
        let mut first = RouteTable::default();
        first.aliases.insert("a".to_owned(), "codex".to_owned());
        save(&path, &first).expect("first save");
        save(&path, &RouteTable::default()).expect("second save");
        let back = load(&path).expect("load");
        assert!(back.aliases.is_empty() && back.combos.is_empty());
        let leftovers = fs::read_dir(&dir)
            .expect("dir readable")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path() != path)
            .count();
        assert_eq!(leftovers, 0, "no temp files survive the rename");
        std::fs::remove_dir_all(&dir).expect("test dir removable");
    }
}
