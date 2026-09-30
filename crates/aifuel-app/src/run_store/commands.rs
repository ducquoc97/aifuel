//! The command receipt log: one recorded Receipt per dispatched command
//! id.
//!
//! Every contract command carries a `command_id`; when delivery is
//! ambiguous a consumer retries the same command, and the runtime answers
//! the recorded receipt verbatim instead of executing the command's side
//! effects twice. The `commands` table is that record: `command_id` is the
//! primary key and `receipt_json` is the serialized Receipt the first
//! execution produced.

use super::{RunStore, RunStoreError};
use crate::run_management::now;
use aifuel_core::CommandId;
use rusqlite::{OptionalExtension, params};

impl RunStore {
    /// The serialized Receipt recorded for a previously executed command,
    /// or `None` when the command id has never completed a dispatch.
    pub fn command_receipt(&self, command_id: &CommandId) -> Result<Option<String>, RunStoreError> {
        self.connection
            .lock()
            .expect("run store mutex")
            .query_row(
                "SELECT receipt_json FROM commands WHERE command_id = ?1",
                params![command_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(RunStoreError::from)
    }

    /// Record the serialized Receipt a command's first execution produced.
    /// `INSERT OR IGNORE` keeps the first answer: a raced retry that already
    /// landed must not be overwritten by a later execution's receipt.
    pub fn record_command(
        &self,
        command_id: &CommandId,
        receipt_json: &str,
    ) -> Result<(), RunStoreError> {
        self.connection.lock().expect("run store mutex").execute(
            "INSERT OR IGNORE INTO commands (command_id, receipt_json, created_at)
            VALUES (?1, ?2, ?3)",
            params![command_id.as_str(), receipt_json, now()],
        )?;
        Ok(())
    }
}
