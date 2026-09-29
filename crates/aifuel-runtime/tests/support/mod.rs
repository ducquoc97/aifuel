//! Test support: a scripted in-crate [`RuntimeAdapter`] double, a scripted
//! [`AgentExecutionAdapter`](aifuel_core::AgentExecutionAdapter) driving the
//! real `CliAdapter`, shared command/channel helpers, and the harness that
//! wires `AgentRuntime` over a temporary run store.

// Each integration-test binary compiles this module independently, so a
// helper or re-export one binary does not use still belongs here.
#![allow(dead_code, unused_imports)]

use aifuel_app::RunStore;
use aifuel_core::{
    AdapterCapabilities, CliAdapterId, ExecutionConfig, Integration, IntegrationId,
    ModelDescriptor, ProviderId,
};
use aifuel_providers::{AdapterDiscovery, CliAdapter, IntegrationDescriptor};
use aifuel_runtime::{AgentRuntime, RuntimeAdapter};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

mod execution;
mod fake;
mod harness;

pub use execution::{ExecScript, ScriptedExecution};
pub use fake::{FakeAdapter, FakeScript};
pub use harness::{
    collect_run, collect_until, create, created_session, is_completed, next_id, receipt_code,
    receipt_seq, receipt_snapshot, run_start, selection, subscribe,
};

/// The Integration Identity the test adapter serves.
pub const FAKE_INTEGRATION: &str = "fake-cli";

/// The upstream provider the test adapter reports.
pub const FAKE_PROVIDER: &str = "fake-provider";

/// A unique temporary directory for one test's store and config root.
pub fn test_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "aifuel-runtime-{label}-{}-{nanos}",
        std::process::id()
    ))
}

/// The descriptor `integrations.list` reports for the fake integration.
pub fn fake_descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor::builtin(
        Integration {
            id: IntegrationId::new(FAKE_INTEGRATION),
            provider: ProviderId::new(FAKE_PROVIDER),
            name: "Fake CLI".to_owned(),
            execution: ExecutionConfig::Cli {
                adapter: CliAdapterId::new(FAKE_INTEGRATION),
            },
            monitoring: None,
        },
        Vec::new(),
    )
}

/// A discovery context rooted at a throwaway directory.
pub fn fake_discovery(dir: &Path) -> AdapterDiscovery {
    AdapterDiscovery {
        discovery: aifuel_providers::DiscoveryContext::new(dir),
        credentials: aifuel_providers::CredentialStore::new(dir),
        configured: BTreeSet::new(),
    }
}

/// Compose a runtime over a fresh store with the given adapters.
pub fn runtime_at(
    dir: &Path,
    adapters: Vec<Arc<dyn RuntimeAdapter>>,
    descriptors: Vec<IntegrationDescriptor>,
) -> (RunStore, AgentRuntime) {
    std::fs::create_dir_all(dir).expect("test dir creates");
    let store = RunStore::open(dir.join("aifuel.db")).expect("run store opens");
    let runtime =
        AgentRuntime::with_adapters(store.clone(), adapters, descriptors, fake_discovery(dir))
            .expect("runtime opens");
    (store, runtime)
}

/// A runtime serving the fake adapter over the given scripts.
pub fn fake_runtime(
    dir: &Path,
    capabilities: AdapterCapabilities,
    models: Vec<ModelDescriptor>,
    scripts: Vec<FakeScript>,
) -> (RunStore, AgentRuntime) {
    let adapter = Arc::new(FakeAdapter::new(capabilities, models).with_scripts(scripts));
    runtime_at(dir, vec![adapter], vec![fake_descriptor()])
}

/// A runtime serving the real `CliAdapter` over a scripted execution
/// adapter - the end-to-end path the facade exists to drive.
pub fn cli_runtime(dir: &Path, scripts: Vec<ExecScript>) -> (RunStore, AgentRuntime) {
    let adapter = Arc::new(CliAdapter::new(Arc::new(ScriptedExecution::new(scripts))));
    runtime_at(dir, vec![adapter], vec![fake_descriptor()])
}
