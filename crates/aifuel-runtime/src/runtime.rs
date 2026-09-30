//! The [`AgentRuntime`] facade itself: construction, startup reconcile,
//! consumer channels, and shutdown.
//!
//! `open` composes the production seam: the run store at the given path,
//! the `providers.json`/credential context from the surrounding config
//! directory, and one [`CliAdapter`] per compiled execution adapter via
//! `cli_fallback_adapters`. `with_adapters` is the same seam over an
//! explicit adapter set, for embedders and tests.

use crate::adapter::RuntimeAdapter;
use crate::pump;
use crate::registry::Registry;
use aifuel_app::{RunStore, warn_store_write};
use aifuel_core::{
    AccessMode, AgentEvent, AgentEventKind, AgentRuntimeError, AgentSessionHandle, ConsumerId,
    ModelSelection, ReceiptCode, RequestId, SessionId, SessionStatus, StartOptions,
};
use aifuel_providers::{
    AdapterDiscovery, CredentialStore, DiscoveryContext, IntegrationDescriptor,
    IntegrationRegistry, PROVIDERS_FILE_NAME, ProvidersConfig, builtin_integrations,
    cli_fallback_adapters,
};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// The mutable facade state shared with event pumps: live sessions,
/// consumer channels, and pending-approval attributions. One lock orders
/// log appends against subscribe replay registration.
pub(crate) struct Inner {
    /// Sessions a live adapter owns right now, keyed by session id.
    pub(crate) live: HashMap<SessionId, LiveSession>,
    /// One event channel per consumer id, created on first use.
    pub(crate) consumers: HashMap<ConsumerId, Consumer>,
    /// The consumer that answered each pending Approval Request, keyed by
    /// `(session, request)`. The pump stamps it onto the adapter-emitted
    /// `approval.resolved` and removes it there.
    pub(crate) answers: HashMap<(SessionId, RequestId), ConsumerId>,
}

/// One session a live adapter owns: its handle, its subscriber set, and
/// the pump draining its event stream.
pub(crate) struct LiveSession {
    pub(crate) adapter: Arc<dyn RuntimeAdapter>,
    pub(crate) handle: AgentSessionHandle,
    pub(crate) subscribers: BTreeSet<ConsumerId>,
    pub(crate) pump: Option<JoinHandle<()>>,
    /// `session.closed` is already in the log, facade- or adapter-authored.
    pub(crate) closed: bool,
}

/// One consumer's live event channel. `receiver` is handed out once by
/// [`AgentRuntime::events`].
pub(crate) struct Consumer {
    pub(crate) sender: Sender<AgentEvent>,
    pub(crate) receiver: Option<Receiver<AgentEvent>>,
}

/// The in-process agent runtime facade.
///
/// `dispatch` is the single seam every `AgentCommand` flows through. The
/// facade is `Send + Sync`; hosts may dispatch from several threads while
/// the Session Event Log serializes event order per session.
pub struct AgentRuntime {
    pub(crate) store: RunStore,
    pub(crate) registry: Registry,
    pub(crate) inner: Arc<Mutex<Inner>>,
    shutdown: AtomicBool,
}

impl AgentRuntime {
    /// Open the runtime over the run store at `path`.
    ///
    /// The AI Fuel configuration directory is the store's parent directory
    /// by convention (`aifuel.db` lives beside `providers.json` and
    /// `credentials.json`), so provider discovery, the Credential Store,
    /// and the Integration Registry resolve the same way the `aifuel`
    /// binary's own surfaces resolve them.
    ///
    /// On open, persisted sessions still carrying an in-flight status -
    /// left by a crash or a killed process - are marked `interrupted` and
    /// the fact is recorded in each session's log. Interrupted sessions
    /// holding a resume cursor then get a provider-side continuation
    /// attempt where their adapter declares `resume`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AgentRuntimeError> {
        let path = path.as_ref();
        let store = RunStore::open(path).map_err(|error| {
            AgentRuntimeError::provider_error(format!("the run store could not be opened: {error}"))
        })?;
        let config_dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| Path::new(".").to_path_buf());
        let discovery = DiscoveryContext::from_environment().map_err(|error| {
            AgentRuntimeError::provider_error(format!(
                "the discovery context could not be resolved: {error}"
            ))
        })?;
        let credentials = CredentialStore::new(config_dir.clone());
        let config =
            ProvidersConfig::load(config_dir.join(PROVIDERS_FILE_NAME)).map_err(|error| {
                AgentRuntimeError::provider_error(format!("providers.json could not load: {error}"))
            })?;
        let registry = IntegrationRegistry::build(builtin_integrations(), config.into_entries())
            .map_err(|error| {
                AgentRuntimeError::provider_error(format!(
                    "the integration registry could not be built: {error}"
                ))
            })?;
        let discovery_context = AdapterDiscovery {
            discovery,
            credentials,
            configured: registry.configured_ids().clone(),
        };
        let adapters = cli_fallback_adapters(&discovery_context)
            .into_iter()
            .map(|adapter| Arc::new(adapter) as Arc<dyn RuntimeAdapter>)
            .collect();
        let descriptors = registry.list().cloned().collect();
        Self::assemble(store, adapters, descriptors, discovery_context)
    }

    /// Open the runtime over an existing store and an explicit adapter set,
    /// for embedders and tests. `descriptors` is the integration registry
    /// snapshot `integrations.list` reports; `discovery` carries the local
    /// evidence context descriptor inspection consults.
    pub fn with_adapters(
        store: RunStore,
        adapters: Vec<Arc<dyn RuntimeAdapter>>,
        descriptors: Vec<IntegrationDescriptor>,
        discovery: AdapterDiscovery,
    ) -> Result<Self, AgentRuntimeError> {
        Self::assemble(store, adapters, descriptors, discovery)
    }

    fn assemble(
        store: RunStore,
        adapters: Vec<Arc<dyn RuntimeAdapter>>,
        descriptors: Vec<IntegrationDescriptor>,
        discovery: AdapterDiscovery,
    ) -> Result<Self, AgentRuntimeError> {
        let runtime = Self {
            store,
            registry: Registry::new(adapters, descriptors, discovery),
            inner: Arc::new(Mutex::new(Inner {
                live: HashMap::new(),
                consumers: HashMap::new(),
                answers: HashMap::new(),
            })),
            shutdown: AtomicBool::new(false),
        };
        // Startup reconcile: sessions still showing an in-flight status were
        // cut off mid-run, so the interruption is recorded as a fact before
        // continuation is attempted.
        runtime
            .store
            .mark_interrupted_on_shutdown()
            .map_err(|error| {
                AgentRuntimeError::new(
                    ReceiptCode::ProviderError,
                    format!("the startup reconcile could not run: {error}"),
                )
            })?;
        runtime.resume_interrupted_sessions();
        Ok(runtime)
    }

    /// Attempt provider-side continuation for each persisted `interrupted`
    /// session holding a resume cursor whose serving adapter declares
    /// `resume`. On success the facade records `session.status: working`
    /// and a pump takes over the adapter's stream; on failure the session
    /// stays `interrupted` - continuation was attempted, never assumed.
    /// Sessions without a cursor or a resume-capable adapter stay
    /// interrupted permanently.
    fn resume_interrupted_sessions(&self) {
        let sessions = match self.store.agent_sessions() {
            Ok(sessions) => sessions,
            Err(error) => {
                warn_store_write(&error);
                return;
            }
        };
        for session in sessions {
            if session.status != SessionStatus::Interrupted {
                continue;
            }
            let Some(cursor) = session.resume_cursor.clone() else {
                continue;
            };
            let (Some(descriptor), Some(adapter)) = (
                self.registry.descriptor_for(&session.integration),
                self.registry.adapter_for(&session.integration),
            ) else {
                continue;
            };
            if !adapter.capabilities().resume {
                continue;
            }
            let handle = match adapter.start(
                &descriptor.integration,
                StartOptions {
                    cwd: session.cwd.clone(),
                    selection: ModelSelection {
                        integration_id: session.integration.clone(),
                        model: session.model.clone().unwrap_or_default(),
                        effort: session.effort,
                    },
                    // The projection does not persist the session's access
                    // mode, so a continuation asks for the least privilege
                    // rather than inventing authorization the host never
                    // re-declared.
                    access: AccessMode::ReadOnly,
                    resume_cursor: Some(cursor),
                },
            ) {
                Ok(handle) => handle,
                Err(error) => {
                    eprintln!(
                        "aifuel: agent session {} could not resume: {error}",
                        session.session_id
                    );
                    continue;
                }
            };
            // The facade records the continuation fact before the pump
            // starts draining, so `working` lands ahead of the adapter's
            // own emissions; the pump drops the adapter's fresh-session
            // `idle` prelude for a resumed session.
            if let Err(error) = self.store.append(
                &session.session_id,
                AgentEventKind::SessionStatus {
                    status: SessionStatus::Working,
                },
            ) {
                warn_store_write(&error);
                let _ = adapter.stop(handle);
                continue;
            }
            match pump::spawn(
                self.store.clone(),
                Arc::downgrade(&self.inner),
                session.session_id.clone(),
                Arc::clone(&adapter),
                handle.clone(),
                true,
            ) {
                Ok(join) => {
                    self.inner.lock().expect("runtime mutex").live.insert(
                        session.session_id.clone(),
                        LiveSession {
                            adapter,
                            handle,
                            subscribers: BTreeSet::new(),
                            pump: Some(join),
                            closed: false,
                        },
                    );
                }
                Err(error) => {
                    eprintln!("aifuel: the resumed session's event pump could not start: {error}");
                    let _ = adapter.stop(handle);
                    // The `working` fact already landed; record that the
                    // continuation fell through so the projection tells the
                    // truth.
                    if let Err(error) = self.store.append(
                        &session.session_id,
                        AgentEventKind::SessionStatus {
                            status: SessionStatus::Interrupted,
                        },
                    ) {
                        warn_store_write(&error);
                    }
                }
            }
        }
    }

    /// The consumer's live event channel: one channel per consumer id
    /// carrying stamped [`AgentEvent`]s for every session that consumer has
    /// subscribed to.
    ///
    /// The channel is created on first use - either here or by a
    /// `session.subscribe` - and the receiver is handed out exactly once;
    /// a second call returns `None`.
    pub fn events(&self, consumer_id: &ConsumerId) -> Option<Receiver<AgentEvent>> {
        let mut inner = self.inner.lock().expect("runtime mutex");
        Self::consumer(&mut inner, consumer_id).receiver.take()
    }

    /// Graceful shutdown: persist each live session's provider resume cursor
    /// where the serving adapter declares `resume`, then mark every
    /// in-flight session `interrupted` with the fact recorded in its log.
    /// Store failures are reported on stderr, matching the run store's
    /// best-effort write convention.
    pub fn shutdown(&self) {
        if self.shutdown.swap(true, Ordering::SeqCst) {
            return;
        }
        let cursors: Vec<(SessionId, String)> = {
            let inner = self.inner.lock().expect("runtime mutex");
            inner
                .live
                .iter()
                .filter(|(_, live)| live.adapter.capabilities().resume)
                .filter_map(|(session_id, live)| {
                    live.adapter
                        .resume_cursor(session_id)
                        .map(|cursor| (session_id.clone(), cursor))
                })
                .collect()
        };
        for (session_id, cursor) in cursors {
            if let Err(error) = self.store.record_resume_cursor(&session_id, Some(&cursor)) {
                warn_store_write(&error);
            }
        }
        if let Err(error) = self.store.mark_interrupted_on_shutdown() {
            warn_store_write(&error);
        }
    }

    /// The consumer's channel entry, creating it on first use.
    pub(crate) fn consumer<'a>(inner: &'a mut Inner, consumer_id: &ConsumerId) -> &'a mut Consumer {
        inner
            .consumers
            .entry(consumer_id.clone())
            .or_insert_with(|| {
                let (sender, receiver) = mpsc::channel();
                Consumer {
                    sender,
                    receiver: Some(receiver),
                }
            })
    }
}

impl Drop for AgentRuntime {
    /// Dropping the runtime is a graceful shutdown: resume cursors are
    /// persisted and in-flight sessions are marked `interrupted`.
    fn drop(&mut self) {
        self.shutdown();
    }
}
