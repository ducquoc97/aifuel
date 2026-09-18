use super::{AgentMcpSetupError, AgentMcpSetupFacade, storage};
use aifuel_core::AIFUEL_GATEWAY_REGISTRATION_NAME;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

const PENDING_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PendingTransaction {
    schema_version: u32,
    host_id: String,
    config_file: PathBuf,
    entry_name: String,
    base_sha256: Option<String>,
    target_sha256: String,
    backup_file: Option<PathBuf>,
    previous_receipt_entry: Option<Value>,
    entry: Value,
}

pub(super) struct PendingSnapshot {
    snapshot: storage::FileSnapshot,
}

struct LoadedPending {
    snapshot: storage::FileSnapshot,
    transaction: Option<PendingTransaction>,
}

impl AgentMcpSetupFacade<'_> {
    pub(super) fn ensure_no_pending(&self) -> Result<(), AgentMcpSetupError> {
        if self.read_pending()?.transaction.is_some() {
            return Err(AgentMcpSetupError::new(
                "an interrupted Agent MCP Registration needs recovery; dry-run made no changes",
            ));
        }
        Ok(())
    }

    pub(super) fn reconcile_pending(&self) -> Result<(), AgentMcpSetupError> {
        let pending = self.read_pending()?;
        let Some(transaction) = pending.transaction.as_ref() else {
            return Ok(());
        };
        let current = storage::read_snapshot(&self.config_file, "MCP Host configuration")?;
        let current_entry = self
            .adapter
            .current_entry(current.contents.as_deref())
            .map_err(AgentMcpSetupError::from_adapter)?;
        let current_sha256 = sha256(current.contents.as_deref());
        self.verify_pending_backup(transaction)?;

        if current_sha256.as_ref() == Some(&transaction.target_sha256) {
            if current_entry.as_ref() != Some(&transaction.entry) {
                return Err(Self::recovery_conflict_error());
            }
            let receipt = self.read_receipt().map_err(|error| {
                storage::with_backup(
                    AgentMcpSetupError::new(format!(
                        "MCP Host configuration was updated, but AI Fuel could not save its ownership receipt because it could not inspect the current receipt: {error}"
                    )),
                    transaction.backup_file.as_deref(),
                )
            })?;
            let receipt_matches_transaction = receipt.receipt.as_ref().is_none_or(|receipt| {
                receipt.entry == transaction.entry
                    || transaction.previous_receipt_entry.as_ref() == Some(&receipt.entry)
            });
            if !receipt_matches_transaction {
                return Err(Self::recovery_conflict_error());
            }
            if receipt
                .receipt
                .as_ref()
                .is_none_or(|receipt| receipt.entry != transaction.entry)
            {
                self.write_receipt(
                    &receipt,
                    transaction.entry.clone(),
                    transaction.backup_file.as_deref(),
                )?;
            }
            return self.clear_pending_snapshot(&pending.snapshot);
        }

        if current_sha256.as_ref() == transaction.base_sha256.as_ref() {
            return self.clear_pending_snapshot(&pending.snapshot);
        }

        if current_entry.is_none() {
            return self.clear_pending_snapshot(&pending.snapshot);
        }

        let receipt = self.read_receipt()?;
        let entry_is_still_receipt_owned = receipt
            .receipt
            .as_ref()
            .is_some_and(|receipt| current_entry.as_ref() == Some(&receipt.entry));
        if entry_is_still_receipt_owned {
            return self.clear_pending_snapshot(&pending.snapshot);
        }

        Err(Self::recovery_conflict_error())
    }

    pub(super) fn finalize_pending(
        &self,
        expected: &PendingSnapshot,
        expected_entry: &Value,
    ) -> Result<(), AgentMcpSetupError> {
        storage::ensure_snapshot_matches(
            &self.pending_path(),
            &expected.snapshot,
            "recovery journal",
        )?;
        self.reconcile_pending()?;
        if self.read_pending()?.transaction.is_some() {
            return Err(AgentMcpSetupError::new(
                "Agent MCP Registration recovery journal remains after setup",
            ));
        }
        let receipt = self.read_receipt()?;
        if receipt
            .receipt
            .as_ref()
            .is_none_or(|receipt| &receipt.entry != expected_entry)
        {
            return Err(Self::recovery_conflict_error());
        }
        Ok(())
    }

    pub(super) fn write_pending(
        &self,
        base: &storage::FileSnapshot,
        target: &[u8],
        entry: &Value,
        previous_receipt_entry: Option<&Value>,
        backup_file: Option<&Path>,
    ) -> Result<PendingSnapshot, AgentMcpSetupError> {
        let pending = self.read_pending()?;
        if pending.transaction.is_some() {
            return Err(AgentMcpSetupError::new(
                "an earlier Agent MCP Registration transaction still needs recovery",
            ));
        }
        let transaction = PendingTransaction {
            schema_version: PENDING_SCHEMA_VERSION,
            host_id: self.adapter.host_id().to_owned(),
            config_file: self.config_file.clone(),
            entry_name: AIFUEL_GATEWAY_REGISTRATION_NAME.to_owned(),
            base_sha256: sha256(base.contents.as_deref()),
            target_sha256: sha256(Some(target)).expect("target config bytes are present"),
            backup_file: backup_file.map(Path::to_path_buf),
            previous_receipt_entry: previous_receipt_entry.cloned(),
            entry: entry.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&transaction).map_err(|_| {
            AgentMcpSetupError::new("could not encode Agent MCP Registration recovery state")
        })?;
        let path = self.pending_path();
        if let Some(parent) = path.parent() {
            storage::create_private_dir_all(parent)?;
        }
        storage::atomic_write(&path, &bytes, &pending.snapshot, None)
            .map_err(|error| storage::with_backup(error, backup_file))?;
        let loaded = self.read_pending()?;
        if loaded.transaction.as_ref() != Some(&transaction) {
            return Err(AgentMcpSetupError::new(
                "Agent MCP Registration recovery journal could not be verified after writing",
            ));
        }
        Ok(PendingSnapshot {
            snapshot: loaded.snapshot,
        })
    }

    fn clear_pending_snapshot(
        &self,
        snapshot: &storage::FileSnapshot,
    ) -> Result<(), AgentMcpSetupError> {
        let path = self.pending_path();
        storage::ensure_snapshot_matches(&path, snapshot, "recovery journal")?;
        fs::remove_file(&path)
            .map_err(|error| storage::io_error("could not clear the recovery journal", error))
    }

    fn read_pending(&self) -> Result<LoadedPending, AgentMcpSetupError> {
        let snapshot = storage::read_snapshot(&self.pending_path(), "recovery journal")?;
        let transaction = match snapshot.contents.as_deref() {
            None => None,
            Some(bytes) => {
                let transaction: PendingTransaction = serde_json::from_slice(bytes).map_err(|_| {
                    AgentMcpSetupError::new(
                        "Agent MCP Registration recovery journal is unreadable; configuration was left untouched",
                    )
                })?;
                self.validate_pending_identity(&transaction)?;
                Some(transaction)
            }
        };
        Ok(LoadedPending {
            snapshot,
            transaction,
        })
    }

    fn validate_pending_identity(
        &self,
        pending: &PendingTransaction,
    ) -> Result<(), AgentMcpSetupError> {
        if pending.schema_version != PENDING_SCHEMA_VERSION
            || pending.host_id != self.adapter.host_id()
            || pending.config_file != self.config_file
            || pending.entry_name != AIFUEL_GATEWAY_REGISTRATION_NAME
            || pending.target_sha256.len() != 64
            || !pending
                .target_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || pending.base_sha256.as_ref().is_some_and(|digest| {
                digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            return Err(AgentMcpSetupError::new(
                "Agent MCP Registration recovery journal does not match this MCP Host configuration",
            ));
        }
        let expected_backup = pending
            .base_sha256
            .as_ref()
            .map(|_| storage::backup_path(&self.config_file, &self.backup_dir()));
        if pending.backup_file != expected_backup {
            return Err(AgentMcpSetupError::new(
                "Agent MCP Registration recovery journal has an unexpected backup path",
            ));
        }
        Ok(())
    }

    fn verify_pending_backup(
        &self,
        pending: &PendingTransaction,
    ) -> Result<(), AgentMcpSetupError> {
        let (Some(expected_digest), Some(backup_file)) =
            (pending.base_sha256.as_ref(), pending.backup_file.as_ref())
        else {
            if pending.base_sha256.is_none() && pending.backup_file.is_none() {
                return Ok(());
            }
            return Err(Self::recovery_conflict_error());
        };
        let backup = storage::read_snapshot(backup_file, "MCP Host configuration backup")?;
        if sha256(backup.contents.as_deref()).as_ref() != Some(expected_digest) {
            return Err(Self::recovery_conflict_error());
        }
        Ok(())
    }

    pub(super) fn pending_path(&self) -> PathBuf {
        self.state_dir
            .join("pending")
            .join(format!("{}.json", self.registration_key()))
    }

    fn recovery_conflict_error() -> AgentMcpSetupError {
        AgentMcpSetupError::new(
            "MCP Host configuration changed during interrupted setup; AI Fuel preserved it and did not assume ownership",
        )
    }
}

fn sha256(contents: Option<&[u8]>) -> Option<String> {
    contents.map(|contents| {
        let digest = Sha256::digest(contents);
        let mut value = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use std::fmt::Write as _;
            write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
        }
        value
    })
}
