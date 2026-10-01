//! Streamable-HTTP transport for the execution endpoint. Each HTTP session
//! owns its own `RunManager`, so Agent Runs stay owned by their connection
//! exactly as they do over stdio.

use super::{ConnectionState, MAX_FRAME_BYTES, respond};
use aifuel_app::RunManager;
use aifuel_app::selection::GlobalSelectionConfig;
use aifuel_core::ProviderKey;
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// Serve the execution endpoint over streamable HTTP on `POST /mcp`.
/// `make_manager` builds one owner-local run manager per HTTP session so run
/// ownership and shutdown match the per-connection stdio contract.
pub fn serve<M, F>(
    make_manager: M,
    selection: GlobalSelectionConfig,
    catalog: Vec<Value>,
    refresh_catalog: F,
    host: &str,
    port: u16,
) -> Result<(), String>
where
    M: Fn() -> Result<RunManager, String> + Send + Sync + 'static,
    F: Fn(Option<ProviderKey>) -> Result<Vec<Value>, String> + Send + Sync + 'static,
{
    let selection = Arc::new(selection);
    let catalog = Arc::new(catalog);
    let refresh_catalog = Arc::new(refresh_catalog);
    crate::http::serve(
        "aifuel mcp execution",
        host,
        port,
        MAX_FRAME_BYTES as usize,
        move |_| {
            Ok(ExecutionHttpSession {
                manager: make_manager()?,
                connection: Mutex::new(ConnectionState {
                    initialized: false,
                    catalog: (*catalog).clone(),
                }),
                selection: Arc::clone(&selection),
                refresh_catalog: Arc::clone(&refresh_catalog),
            })
        },
    )
}

struct ExecutionHttpSession<F> {
    manager: RunManager,
    connection: Mutex<ConnectionState>,
    selection: Arc<GlobalSelectionConfig>,
    refresh_catalog: Arc<F>,
}

impl<F> crate::http::HttpSession for ExecutionHttpSession<F>
where
    F: Fn(Option<ProviderKey>) -> Result<Vec<Value>, String> + Send + Sync + 'static,
{
    fn handle(&self, message: &Value) -> Option<Value> {
        let mut connection = self.connection.lock().expect("execution session mutex");
        respond(
            &self.manager,
            &self.selection,
            &mut connection,
            &*self.refresh_catalog,
            message,
        )
    }

    fn close(&self) {
        self.manager.shutdown();
    }
}
