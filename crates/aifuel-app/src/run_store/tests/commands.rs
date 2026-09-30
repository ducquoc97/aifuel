//! Command receipt log tests: record/read round-trip, first-answer wins,
//! and the version-4 to 5 migration that creates the `commands` table.

use super::*;
use aifuel_core::CommandId;

#[test]
fn command_receipt_roundtrips_and_first_answer_wins() {
    let path = store_path("commands");
    let store = RunStore::open(&path).expect("store opens");
    let command = CommandId::new("cmd-1");

    assert_eq!(
        store.command_receipt(&command).expect("receipt reads"),
        None,
        "an unknown command id has no recorded receipt"
    );
    store
        .record_command(&command, r#"{"command_id":"cmd-1","ok":true,"seq":3}"#)
        .expect("receipt records");
    assert_eq!(
        store.command_receipt(&command).expect("receipt reads"),
        Some(r#"{"command_id":"cmd-1","ok":true,"seq":3}"#.to_owned()),
        "the recorded receipt reads back verbatim"
    );

    // A raced retry must not overwrite the first answer: the runtime
    // replays whatever landed first.
    store
        .record_command(&command, r#"{"command_id":"cmd-1","ok":false}"#)
        .expect("second record is ignored");
    assert_eq!(
        store.command_receipt(&command).expect("receipt reads"),
        Some(r#"{"command_id":"cmd-1","ok":true,"seq":3}"#.to_owned()),
        "the first recorded receipt wins"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn version_four_databases_gain_the_commands_table() {
    let path = store_path("migrate-v4");
    // Seed the version marker a version-4 store carried; the migration
    // stamps version 5 and the schema batch creates the table.
    {
        let connection = rusqlite::Connection::open(&path).expect("seed db opens");
        connection
            .execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                INSERT INTO meta (key, value) VALUES ('schema_version', '4');",
            )
            .expect("seed schema applies");
    }

    let store = RunStore::open(&path).expect("store opens");
    let version: String = store
        .connection
        .lock()
        .expect("run store mutex")
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .expect("schema version reads");
    assert_eq!(
        version,
        SCHEMA_VERSION.to_string(),
        "the migration advances the schema version"
    );

    let command = CommandId::new("cmd-migrated");
    store
        .record_command(&command, r#"{"command_id":"cmd-migrated","ok":true}"#)
        .expect("receipt records on a migrated db");
    assert!(
        store
            .command_receipt(&command)
            .expect("receipt reads")
            .is_some()
    );
    let _ = std::fs::remove_file(path);
}
