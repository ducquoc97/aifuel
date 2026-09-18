use aifuel_core::{
    AIFUEL_GATEWAY_REGISTRATION_NAME, AgentMcpRegistrationAdapter, AgentMcpRegistrationError,
};
mod recovery;
mod storage;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::{error, fmt};
use storage::{FileSnapshot, RegistrationLock};

const RECEIPT_SCHEMA_VERSION: u32 = 1;

/// The operation requested from the shared Agent MCP Registration workflow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgentMcpSetupOptions {
    pub dry_run: bool,
    pub remove: bool,
}

/// The result of applying or previewing one host's Agent MCP Registration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentMcpSetupAction {
    Applied,
    Updated,
    AlreadyConfigured,
    WouldApply,
    WouldUpdate,
    Removed,
    WouldRemove,
    AlreadyAbsent,
    StaleReceiptCleared,
    WouldClearStaleReceipt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentMcpSetupResult {
    pub host_id: &'static str,
    pub action: AgentMcpSetupAction,
    pub config_file: PathBuf,
    pub backup_file: Option<PathBuf>,
}

/// Coordinates receipt-backed setup for any Agent MCP Registration adapter.
///
/// The executable supplies the host's effective home, AI Fuel's private state
/// directory, and the running AI Fuel executable path. This keeps the shared
/// workflow independent of provider credentials and Agent Run capabilities.
pub struct AgentMcpSetupFacade<'a> {
    adapter: &'a dyn AgentMcpRegistrationAdapter,
    config_file: PathBuf,
    gateway_executable: PathBuf,
    state_dir: PathBuf,
}

impl<'a> AgentMcpSetupFacade<'a> {
    pub fn new(
        adapter: &'a dyn AgentMcpRegistrationAdapter,
        host_home: &Path,
        gateway_executable: PathBuf,
        state_dir: PathBuf,
    ) -> Self {
        Self {
            adapter,
            config_file: adapter.config_file(host_home),
            gateway_executable,
            state_dir,
        }
    }

    pub fn config_file(&self) -> &Path {
        &self.config_file
    }

    /// Preview or apply setup/removal. Dry-run performs reads only and does not
    /// create lock, backup, config, or receipt files.
    pub fn run(
        &self,
        options: AgentMcpSetupOptions,
    ) -> Result<AgentMcpSetupResult, AgentMcpSetupError> {
        if options.dry_run {
            self.ensure_no_pending()?;
            return self.plan_and_report(options);
        }

        let _lock = self.acquire_lock()?;
        self.reconcile_pending()?;
        self.plan_and_report(options)
    }

    fn plan_and_report(
        &self,
        options: AgentMcpSetupOptions,
    ) -> Result<AgentMcpSetupResult, AgentMcpSetupError> {
        let snapshot = storage::read_snapshot(&self.config_file, "MCP Host configuration")?;
        let receipt_snapshot = self.read_receipt()?;
        let plan = self.plan(options, &snapshot, &receipt_snapshot)?;
        self.execute_plan(options, plan, &snapshot, &receipt_snapshot)
    }

    fn plan(
        &self,
        options: AgentMcpSetupOptions,
        snapshot: &FileSnapshot,
        receipt_snapshot: &ReceiptSnapshot,
    ) -> Result<SetupPlan, AgentMcpSetupError> {
        let current = self
            .adapter
            .current_entry(snapshot.contents.as_deref())
            .map_err(AgentMcpSetupError::from_adapter)?;

        if options.remove {
            return match current {
                None if receipt_snapshot.receipt.is_some() => {
                    self.validate_receipt_identity(receipt_snapshot.receipt.as_ref().unwrap())?;
                    Ok(SetupPlan::ClearStaleReceipt)
                }
                None => Ok(SetupPlan::AlreadyAbsent),
                Some(current_entry) => {
                    let receipt = receipt_snapshot.receipt.as_ref().ok_or_else(|| {
                        AgentMcpSetupError::new(format!(
                            "{} contains an unowned {} registration; AI Fuel will not remove it",
                            self.config_file.display(),
                            AIFUEL_GATEWAY_REGISTRATION_NAME
                        ))
                    })?;
                    self.validate_receipt_identity(receipt)?;
                    if receipt.entry != current_entry {
                        return Err(self.edited_registration_error());
                    }
                    let config = snapshot.contents.as_deref().ok_or_else(|| {
                        AgentMcpSetupError::new(
                            "MCP Host configuration disappeared while planning removal",
                        )
                    })?;
                    let updated = self
                        .adapter
                        .remove_entry(config)
                        .map_err(AgentMcpSetupError::from_adapter)?;
                    if self
                        .adapter
                        .current_entry(Some(&updated))
                        .map_err(AgentMcpSetupError::from_adapter)?
                        .is_some()
                    {
                        return Err(AgentMcpSetupError::new(
                            "registration adapter did not remove the managed entry",
                        ));
                    }
                    Ok(SetupPlan::Remove(updated))
                }
            };
        }

        let expected = self
            .adapter
            .expected_entry(&self.gateway_executable)
            .map_err(AgentMcpSetupError::from_adapter)?;
        match current {
            None => {
                let updated = self
                    .adapter
                    .write_entry(snapshot.contents.as_deref(), &self.gateway_executable)
                    .map_err(AgentMcpSetupError::from_adapter)?;
                self.verify_written_entry(&updated, &expected)?;
                Ok(SetupPlan::Apply {
                    config: updated,
                    entry: expected,
                })
            }
            Some(current_entry) if current_entry == expected => Ok(SetupPlan::AlreadyConfigured),
            Some(current_entry) => {
                let receipt = receipt_snapshot
                    .receipt
                    .as_ref()
                    .ok_or_else(|| self.conflicting_registration_error())?;
                self.validate_receipt_identity(receipt)?;
                if receipt.entry != current_entry {
                    return Err(self.edited_registration_error());
                }
                let updated = self
                    .adapter
                    .write_entry(snapshot.contents.as_deref(), &self.gateway_executable)
                    .map_err(AgentMcpSetupError::from_adapter)?;
                self.verify_written_entry(&updated, &expected)?;
                Ok(SetupPlan::Update {
                    config: updated,
                    entry: expected,
                })
            }
        }
    }

    fn execute_plan(
        &self,
        options: AgentMcpSetupOptions,
        plan: SetupPlan,
        snapshot: &FileSnapshot,
        receipt_snapshot: &ReceiptSnapshot,
    ) -> Result<AgentMcpSetupResult, AgentMcpSetupError> {
        let (action, backup_file) = match plan {
            SetupPlan::AlreadyConfigured => (AgentMcpSetupAction::AlreadyConfigured, None),
            SetupPlan::AlreadyAbsent => (AgentMcpSetupAction::AlreadyAbsent, None),
            SetupPlan::Apply { .. } if options.dry_run => (AgentMcpSetupAction::WouldApply, None),
            SetupPlan::Update { .. } if options.dry_run => (AgentMcpSetupAction::WouldUpdate, None),
            SetupPlan::Remove(updated) if options.dry_run => {
                let _ = updated;
                (AgentMcpSetupAction::WouldRemove, None)
            }
            SetupPlan::ClearStaleReceipt if options.dry_run => {
                (AgentMcpSetupAction::WouldClearStaleReceipt, None)
            }
            SetupPlan::Apply { config, entry } => {
                let backup =
                    storage::write_backup(&self.config_file, &self.backup_dir(), snapshot)?;
                let previous_receipt_entry = receipt_snapshot
                    .receipt
                    .as_ref()
                    .map(|receipt| &receipt.entry);
                let pending = self.write_pending(
                    snapshot,
                    &config,
                    &entry,
                    previous_receipt_entry,
                    backup.as_deref(),
                )?;
                storage::replace_config_with_backup(
                    &self.config_file,
                    snapshot,
                    &config,
                    backup.as_deref(),
                )?;
                self.finalize_pending(&pending, &entry)?;
                (AgentMcpSetupAction::Applied, backup)
            }
            SetupPlan::Update { config, entry } => {
                let backup =
                    storage::write_backup(&self.config_file, &self.backup_dir(), snapshot)?;
                let previous_receipt_entry = receipt_snapshot
                    .receipt
                    .as_ref()
                    .map(|receipt| &receipt.entry);
                let pending = self.write_pending(
                    snapshot,
                    &config,
                    &entry,
                    previous_receipt_entry,
                    backup.as_deref(),
                )?;
                storage::replace_config_with_backup(
                    &self.config_file,
                    snapshot,
                    &config,
                    backup.as_deref(),
                )?;
                self.finalize_pending(&pending, &entry)?;
                (AgentMcpSetupAction::Updated, backup)
            }
            SetupPlan::Remove(updated) => {
                let backup = self.replace_config(snapshot, &updated)?;
                if let Err(error) = self.clear_receipt(receipt_snapshot) {
                    return Err(storage::with_backup(
                        AgentMcpSetupError::new(format!(
                            "MCP Host configuration was updated, but AI Fuel could not clear its ownership receipt: {error}"
                        )),
                        backup.as_deref(),
                    ));
                }
                (AgentMcpSetupAction::Removed, backup)
            }
            SetupPlan::ClearStaleReceipt => {
                self.clear_receipt(receipt_snapshot)?;
                (AgentMcpSetupAction::StaleReceiptCleared, None)
            }
        };

        Ok(AgentMcpSetupResult {
            host_id: self.adapter.host_id(),
            action,
            config_file: self.config_file.clone(),
            backup_file,
        })
    }

    fn verify_written_entry(
        &self,
        config: &[u8],
        expected: &Value,
    ) -> Result<(), AgentMcpSetupError> {
        let actual = self
            .adapter
            .current_entry(Some(config))
            .map_err(AgentMcpSetupError::from_adapter)?;
        if actual.as_ref() != Some(expected) {
            return Err(AgentMcpSetupError::new(
                "Agent MCP Registration adapter produced an unexpected entry",
            ));
        }
        Ok(())
    }

    fn edited_registration_error(&self) -> AgentMcpSetupError {
        AgentMcpSetupError::new(format!(
            "{} registration was edited after AI Fuel created it; preserve the entry and reconcile its ownership receipt before setup or removal",
            AIFUEL_GATEWAY_REGISTRATION_NAME
        ))
    }

    fn conflicting_registration_error(&self) -> AgentMcpSetupError {
        AgentMcpSetupError::new(format!(
            "{} already contains a different or unowned {} registration; AI Fuel will not overwrite it",
            self.config_file.display(),
            AIFUEL_GATEWAY_REGISTRATION_NAME
        ))
    }

    fn receipt_path(&self) -> PathBuf {
        self.state_dir
            .join("receipts")
            .join(format!("{}.json", self.registration_key()))
    }

    fn lock_path(&self) -> PathBuf {
        self.state_dir
            .join("locks")
            .join(format!("{}.lock", self.registration_key()))
    }

    fn backup_dir(&self) -> PathBuf {
        self.state_dir.join("backups").join(self.registration_key())
    }

    fn registration_key(&self) -> String {
        let digest = Sha256::digest(self.config_file.to_string_lossy().as_bytes());
        let mut key = String::with_capacity(self.adapter.host_id().len() + 65);
        key.push_str(self.adapter.host_id());
        key.push('-');
        for byte in digest {
            use std::fmt::Write as _;
            write!(&mut key, "{byte:02x}").expect("writing to String cannot fail");
        }
        key
    }

    fn acquire_lock(&self) -> Result<RegistrationLock, AgentMcpSetupError> {
        let path = self.lock_path();
        let parent = path
            .parent()
            .ok_or_else(|| AgentMcpSetupError::new("registration lock has no parent directory"))?;
        storage::create_private_dir_all(parent)?;
        storage::acquire_lock(&path)
    }

    fn read_receipt(&self) -> Result<ReceiptSnapshot, AgentMcpSetupError> {
        let snapshot =
            storage::read_snapshot(&self.receipt_path(), "Agent MCP Registration receipt")?;
        let receipt = match snapshot.contents.as_deref() {
            None => None,
            Some(bytes) => {
                let receipt: Receipt = serde_json::from_slice(bytes).map_err(|_| {
                    AgentMcpSetupError::new(
                        "Agent MCP Registration receipt is unreadable; configuration was left untouched",
                    )
                })?;
                self.validate_receipt_identity(&receipt)?;
                Some(receipt)
            }
        };
        Ok(ReceiptSnapshot { snapshot, receipt })
    }

    fn validate_receipt_identity(&self, receipt: &Receipt) -> Result<(), AgentMcpSetupError> {
        if receipt.schema_version != RECEIPT_SCHEMA_VERSION
            || receipt.host_id != self.adapter.host_id()
            || receipt.config_file != self.config_file
            || receipt.entry_name != AIFUEL_GATEWAY_REGISTRATION_NAME
        {
            return Err(AgentMcpSetupError::new(
                "Agent MCP Registration receipt does not match this MCP Host configuration; configuration was left untouched",
            ));
        }
        Ok(())
    }

    fn write_receipt(
        &self,
        receipt_snapshot: &ReceiptSnapshot,
        entry: Value,
        backup_file: Option<&Path>,
    ) -> Result<(), AgentMcpSetupError> {
        let receipt = Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            host_id: self.adapter.host_id().to_owned(),
            config_file: self.config_file.clone(),
            entry_name: AIFUEL_GATEWAY_REGISTRATION_NAME.to_owned(),
            entry,
        };
        let bytes = serde_json::to_vec_pretty(&receipt).map_err(|_| {
            AgentMcpSetupError::new("could not encode Agent MCP Registration receipt")
        })?;
        let path = self.receipt_path();
        if let Some(parent) = path.parent() {
            storage::create_private_dir_all(parent)?;
        }
        storage::atomic_write(&path, &bytes, &receipt_snapshot.snapshot, None).map_err(|error| {
            let backup = match backup_file {
                Some(path) => format!(". The original MCP Host configuration backup is at {}", path.display()),
                None => ". There was no pre-existing MCP Host configuration to back up".to_owned(),
            };
            AgentMcpSetupError::new(format!(
                "MCP Host configuration was updated, but AI Fuel could not save its ownership receipt: {error}. Do not remove the registration until ownership is reconciled{backup}"
            ))
        })
    }

    fn clear_receipt(&self, receipt_snapshot: &ReceiptSnapshot) -> Result<(), AgentMcpSetupError> {
        let path = self.receipt_path();
        if receipt_snapshot.snapshot.contents.is_none() {
            return Ok(());
        }
        storage::ensure_snapshot_matches(&path, &receipt_snapshot.snapshot, "ownership receipt")?;
        fs::remove_file(&path).map_err(|error| {
            storage::io_error("could not clear the Agent MCP Registration receipt", error)
        })
    }

    fn replace_config(
        &self,
        snapshot: &FileSnapshot,
        updated: &[u8],
    ) -> Result<Option<PathBuf>, AgentMcpSetupError> {
        storage::replace_config(&self.config_file, &self.backup_dir(), snapshot, updated)
    }
}

enum SetupPlan {
    Apply { config: Vec<u8>, entry: Value },
    Update { config: Vec<u8>, entry: Value },
    Remove(Vec<u8>),
    ClearStaleReceipt,
    AlreadyConfigured,
    AlreadyAbsent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema_version: u32,
    host_id: String,
    config_file: PathBuf,
    entry_name: String,
    entry: Value,
}

struct ReceiptSnapshot {
    snapshot: FileSnapshot,
    receipt: Option<Receipt>,
}

/// A safe message for setup failures. Configuration contents are never included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMcpSetupError(String);

impl AgentMcpSetupError {
    fn new(detail: impl Into<String>) -> Self {
        Self(detail.into())
    }

    fn from_adapter(error: AgentMcpRegistrationError) -> Self {
        Self(error.to_string())
    }
}

impl fmt::Display for AgentMcpSetupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl error::Error for AgentMcpSetupError {}

#[cfg(test)]
mod tests;
