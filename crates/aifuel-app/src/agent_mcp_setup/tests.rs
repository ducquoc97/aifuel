use super::*;
use serde_json::{Map, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST_DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let suffix = NEXT_TEST_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("aifuel-mcp-setup-{}-{suffix}", std::process::id()));
        fs::create_dir_all(&path).expect("test directory should be creatable");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct JsonAdapter;

impl AgentMcpRegistrationAdapter for JsonAdapter {
    fn host_id(&self) -> &'static str {
        "fixture-host"
    }

    fn config_file(&self, host_home: &Path) -> PathBuf {
        host_home.join("settings.json")
    }

    fn expected_entry(&self, executable: &Path) -> Result<Value, AgentMcpRegistrationError> {
        Ok(json!({
            "command": executable.to_string_lossy(),
            "args": ["mcp", "gateway", "--agent", self.host_id()]
        }))
    }

    fn current_entry(
        &self,
        config: Option<&[u8]>,
    ) -> Result<Option<Value>, AgentMcpRegistrationError> {
        let Some(config) = config else {
            return Ok(None);
        };
        let value: Value = serde_json::from_slice(config)
            .map_err(|_| AgentMcpRegistrationError::new("fixture config is malformed"))?;
        Ok(value
            .get("mcpServers")
            .and_then(Value::as_object)
            .and_then(|servers| servers.get(AIFUEL_GATEWAY_REGISTRATION_NAME))
            .cloned())
    }

    fn write_entry(
        &self,
        config: Option<&[u8]>,
        executable: &Path,
    ) -> Result<Vec<u8>, AgentMcpRegistrationError> {
        let mut value: Value = match config {
            Some(config) => serde_json::from_slice(config)
                .map_err(|_| AgentMcpRegistrationError::new("fixture config is malformed"))?,
            None => json!({}),
        };
        let root = value
            .as_object_mut()
            .ok_or_else(|| AgentMcpRegistrationError::new("fixture root is not an object"))?;
        let servers = root
            .entry("mcpServers")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .ok_or_else(|| {
                AgentMcpRegistrationError::new("fixture MCP servers are not an object")
            })?;
        servers.insert(
            AIFUEL_GATEWAY_REGISTRATION_NAME.to_owned(),
            self.expected_entry(executable)?,
        );
        serde_json::to_vec_pretty(&value)
            .map_err(|_| AgentMcpRegistrationError::new("fixture config could not be encoded"))
    }

    fn remove_entry(&self, config: &[u8]) -> Result<Vec<u8>, AgentMcpRegistrationError> {
        let mut value: Value = serde_json::from_slice(config)
            .map_err(|_| AgentMcpRegistrationError::new("fixture config is malformed"))?;
        if let Some(servers) = value.get_mut("mcpServers").and_then(Value::as_object_mut) {
            servers.remove(AIFUEL_GATEWAY_REGISTRATION_NAME);
        }
        serde_json::to_vec_pretty(&value)
            .map_err(|_| AgentMcpRegistrationError::new("fixture config could not be encoded"))
    }
}

fn workflow<'a, A: AgentMcpRegistrationAdapter>(
    adapter: &'a A,
    directory: &TestDirectory,
) -> AgentMcpSetupFacade<'a> {
    AgentMcpSetupFacade::new(
        adapter,
        &directory.path().join("host"),
        directory.path().join("bin/aifuel"),
        directory.path().join("aifuel-state/registrations"),
    )
}

fn setup_options(dry_run: bool, remove: bool) -> AgentMcpSetupOptions {
    AgentMcpSetupOptions { dry_run, remove }
}

fn backup_files(directory: &Path) -> Vec<PathBuf> {
    let backup_dir = directory.join("aifuel-state/registrations/backups");
    let Ok(host_dirs) = fs::read_dir(backup_dir) else {
        return Vec::new();
    };
    let mut files: Vec<_> = host_dirs
        .flat_map(|entry| {
            fs::read_dir(entry.expect("backup host directory should exist").path())
                .expect("backup directory should exist")
        })
        .map(|entry| entry.expect("backup file should exist").path())
        .collect();
    files.sort();
    files
}

#[test]
fn dry_run_previews_setup_without_creating_state_or_mutating_configuration() {
    let directory = TestDirectory::new();
    let adapter = JsonAdapter;
    let setup = workflow(&adapter, &directory);
    fs::create_dir_all(setup.config_file().parent().unwrap()).unwrap();
    let original = br#"{
  "unrelated": {"keep": true}
}
"#;
    fs::write(setup.config_file(), original).unwrap();

    let result = setup.run(setup_options(true, false)).unwrap();

    assert_eq!(result.action, AgentMcpSetupAction::WouldApply);
    assert_eq!(fs::read(setup.config_file()).unwrap(), original);
    assert!(!directory.path().join("aifuel-state").exists());
    assert!(result.backup_file.is_none());
}

#[test]
fn apply_preserves_unrelated_data_records_a_backup_and_repeats_without_a_write() {
    let directory = TestDirectory::new();
    let adapter = JsonAdapter;
    let setup = workflow(&adapter, &directory);
    fs::create_dir_all(setup.config_file().parent().unwrap()).unwrap();
    let original = br#"{
  "unrelated": {"keep": true},
  "mcpServers": {"other": {"command": "other", "args": []}}
}
"#;
    fs::write(setup.config_file(), original).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(setup.config_file()).unwrap().permissions();
        permissions.set_mode(0o640);
        fs::set_permissions(setup.config_file(), permissions).unwrap();
    }

    let first = setup.run(AgentMcpSetupOptions::default()).unwrap();

    assert_eq!(first.action, AgentMcpSetupAction::Applied);
    let backup = first
        .backup_file
        .expect("existing config should be backed up");
    assert_eq!(fs::read(&backup).unwrap(), original);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(setup.config_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert_eq!(
            fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let receipt = setup.receipt_path();
        assert_eq!(
            fs::metadata(&receipt).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(receipt.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    let updated: Value = serde_json::from_slice(&fs::read(setup.config_file()).unwrap()).unwrap();
    assert_eq!(updated["unrelated"]["keep"], true);
    assert_eq!(updated["mcpServers"]["other"]["command"], "other");
    assert_eq!(
        updated["mcpServers"][AIFUEL_GATEWAY_REGISTRATION_NAME]["args"],
        json!(["mcp", "gateway", "--agent", "fixture-host"])
    );

    let second = setup.run(AgentMcpSetupOptions::default()).unwrap();

    assert_eq!(second.action, AgentMcpSetupAction::AlreadyConfigured);
    assert!(second.backup_file.is_none());
    assert_eq!(backup_files(directory.path()).len(), 1);
}

#[test]
fn identical_unowned_registration_is_neither_adopted_nor_removed() {
    let directory = TestDirectory::new();
    let adapter = JsonAdapter;
    let setup = workflow(&adapter, &directory);
    fs::create_dir_all(setup.config_file().parent().unwrap()).unwrap();
    let expected = adapter
        .expected_entry(&directory.path().join("bin/aifuel"))
        .unwrap();
    let original = serde_json::to_vec_pretty(&json!({
        "mcpServers": {AIFUEL_GATEWAY_REGISTRATION_NAME: expected},
        "unrelated": true
    }))
    .unwrap();
    fs::write(setup.config_file(), &original).unwrap();

    let setup_result = setup.run(AgentMcpSetupOptions::default()).unwrap();
    let remove_result = setup.run(setup_options(false, true));

    assert_eq!(setup_result.action, AgentMcpSetupAction::AlreadyConfigured);
    assert!(remove_result.unwrap_err().to_string().contains("unowned"));
    assert!(!setup.receipt_path().exists());
    assert_eq!(fs::read(setup.config_file()).unwrap(), original);
}

#[test]
fn removal_refuses_to_delete_a_receipt_owned_entry_after_user_edits() {
    let directory = TestDirectory::new();
    let adapter = JsonAdapter;
    let setup = workflow(&adapter, &directory);
    fs::create_dir_all(setup.config_file().parent().unwrap()).unwrap();
    fs::write(setup.config_file(), br#"{"unrelated": {"keep": true}}"#).unwrap();
    setup.run(AgentMcpSetupOptions::default()).unwrap();

    let mut current: Value =
        serde_json::from_slice(&fs::read(setup.config_file()).unwrap()).unwrap();
    current["mcpServers"][AIFUEL_GATEWAY_REGISTRATION_NAME]["args"] =
        json!(["mcp", "gateway", "--agent", "edited"]);
    current["unrelated"]["new_setting"] = json!(42);
    let user_edited = serde_json::to_vec_pretty(&current).unwrap();
    fs::write(setup.config_file(), &user_edited).unwrap();

    let error = setup.run(setup_options(false, true)).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("edited after AI Fuel created it")
    );
    assert_eq!(fs::read(setup.config_file()).unwrap(), user_edited);
}

#[test]
fn removing_a_registration_with_no_entry_only_clears_its_stale_receipt() {
    let directory = TestDirectory::new();
    let adapter = JsonAdapter;
    let setup = workflow(&adapter, &directory);
    fs::create_dir_all(setup.config_file().parent().unwrap()).unwrap();
    fs::write(setup.config_file(), br#"{"unrelated": true}"#).unwrap();
    setup.run(AgentMcpSetupOptions::default()).unwrap();
    let backups_before_remove = backup_files(directory.path());
    let mut current: Value =
        serde_json::from_slice(&fs::read(setup.config_file()).unwrap()).unwrap();
    current["mcpServers"]
        .as_object_mut()
        .unwrap()
        .remove(AIFUEL_GATEWAY_REGISTRATION_NAME);
    let user_edited = serde_json::to_vec_pretty(&current).unwrap();
    fs::write(setup.config_file(), &user_edited).unwrap();

    let preview = setup.run(setup_options(true, true)).unwrap();
    assert_eq!(preview.action, AgentMcpSetupAction::WouldClearStaleReceipt);
    assert!(directory.path().join("aifuel-state/registrations").exists());

    let result = setup.run(setup_options(false, true)).unwrap();

    assert_eq!(result.action, AgentMcpSetupAction::StaleReceiptCleared);
    assert_eq!(fs::read(setup.config_file()).unwrap(), user_edited);
    assert_eq!(backup_files(directory.path()), backups_before_remove);
}

#[test]
fn malformed_host_configuration_fails_without_replacing_it() {
    let directory = TestDirectory::new();
    let adapter = JsonAdapter;
    let setup = workflow(&adapter, &directory);
    fs::create_dir_all(setup.config_file().parent().unwrap()).unwrap();
    fs::write(setup.config_file(), b"{").unwrap();

    let error = setup.run(AgentMcpSetupOptions::default()).unwrap_err();

    assert!(error.to_string().contains("fixture config is malformed"));
    assert_eq!(fs::read(setup.config_file()).unwrap(), b"{");
    assert!(backup_files(directory.path()).is_empty());
}

#[test]
fn concurrent_setup_attempts_serialize_and_converge_on_one_registration() {
    let directory = TestDirectory::new();
    let adapter = JsonAdapter;
    let first = workflow(&adapter, &directory);
    let second = workflow(&adapter, &directory);

    let (first_result, second_result) = std::thread::scope(|scope| {
        let first_thread = scope.spawn(|| first.run(AgentMcpSetupOptions::default()));
        let second_thread = scope.spawn(|| second.run(AgentMcpSetupOptions::default()));
        (
            first_thread.join().unwrap().unwrap(),
            second_thread.join().unwrap().unwrap(),
        )
    });

    let mut actions = [first_result.action, second_result.action];
    actions.sort_by_key(|action| *action as u8);
    assert_eq!(
        actions,
        [
            AgentMcpSetupAction::Applied,
            AgentMcpSetupAction::AlreadyConfigured
        ]
    );
    let config: Value = serde_json::from_slice(&fs::read(first.config_file()).unwrap()).unwrap();
    assert_eq!(
        config["mcpServers"].as_object().unwrap().len(),
        1,
        "cooperating setup writers should create one registration"
    );
    assert!(first.receipt_path().is_file());
    assert_eq!(backup_files(directory.path()).len(), 0);
}

struct ReceiptRaceAdapter {
    receipt_path: PathBuf,
}

impl AgentMcpRegistrationAdapter for ReceiptRaceAdapter {
    fn host_id(&self) -> &'static str {
        JsonAdapter.host_id()
    }

    fn config_file(&self, host_home: &Path) -> PathBuf {
        JsonAdapter.config_file(host_home)
    }

    fn expected_entry(&self, executable: &Path) -> Result<Value, AgentMcpRegistrationError> {
        JsonAdapter.expected_entry(executable)
    }

    fn current_entry(
        &self,
        config: Option<&[u8]>,
    ) -> Result<Option<Value>, AgentMcpRegistrationError> {
        JsonAdapter.current_entry(config)
    }

    fn write_entry(
        &self,
        config: Option<&[u8]>,
        executable: &Path,
    ) -> Result<Vec<u8>, AgentMcpRegistrationError> {
        fs::create_dir_all(&self.receipt_path).unwrap();
        JsonAdapter.write_entry(config, executable)
    }

    fn remove_entry(&self, config: &[u8]) -> Result<Vec<u8>, AgentMcpRegistrationError> {
        JsonAdapter.remove_entry(config)
    }
}

#[test]
fn failed_receipt_write_is_reported_and_does_not_assume_ownership_for_removal() {
    let directory = TestDirectory::new();
    let plain_adapter = JsonAdapter;
    let plain_setup = workflow(&plain_adapter, &directory);
    let adapter = ReceiptRaceAdapter {
        receipt_path: plain_setup.receipt_path(),
    };
    let setup = workflow(&adapter, &directory);

    let error = setup.run(AgentMcpSetupOptions::default()).unwrap_err();

    assert!(error.to_string().contains("configuration was updated"));
    assert!(
        error
            .to_string()
            .contains("could not save its ownership receipt")
    );
    let installed: Value = serde_json::from_slice(&fs::read(setup.config_file()).unwrap()).unwrap();
    assert!(installed["mcpServers"][AIFUEL_GATEWAY_REGISTRATION_NAME].is_object());
    assert!(setup.pending_path().is_file());
    let config_before_remove = fs::read(setup.config_file()).unwrap();
    let removal = setup.run(setup_options(false, true));
    assert!(removal.is_err());
    assert_eq!(fs::read(setup.config_file()).unwrap(), config_before_remove);

    fs::remove_dir(setup.receipt_path()).unwrap();
    let recovered = setup.run(AgentMcpSetupOptions::default()).unwrap();
    assert_eq!(recovered.action, AgentMcpSetupAction::AlreadyConfigured);
    assert!(setup.receipt_path().is_file());
    assert!(!setup.pending_path().exists());

    let recovered_removal = setup.run(setup_options(false, true)).unwrap();
    assert_eq!(recovered_removal.action, AgentMcpSetupAction::Removed);
}
