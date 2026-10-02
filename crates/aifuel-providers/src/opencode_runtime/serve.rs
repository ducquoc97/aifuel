//! The `opencode serve` process endpoint behind one session.
//!
//! Each session spawns one `opencode serve --hostname 127.0.0.1 --port
//! <free>` process rooted at the session's working directory. The serve
//! binds loopback only and inherits no AI Fuel state. When the host
//! environment sets `OPENCODE_SERVER_PASSWORD`, the spawned server
//! requires HTTP basic auth on every route - the documented convention -
//! so the adapter reads the same variables and authenticates each
//! request rather than stripping a user-configured credential.
//!
//! The connector is the session driver's seam: production spawns the
//! process; tests inject a fake endpoint and play the API side.

use super::session::SessionSetup;
use crate::agent_execution::{owned_command, program_candidates};
use aifuel_core::AgentRuntimeError;
use process_wrap::tokio::TokioChildWrapper;
use std::io;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncRead;

/// The documented environment variables guarding `opencode serve` with
/// HTTP basic auth.
const SERVER_PASSWORD_ENV: &str = "OPENCODE_SERVER_PASSWORD";
const SERVER_USERNAME_ENV: &str = "OPENCODE_SERVER_USERNAME";
const DEFAULT_SERVER_USERNAME: &str = "opencode";

/// The transport factory the driver asks for one serve endpoint.
pub(super) type Connector =
    Arc<dyn Fn(&SessionSetup) -> Result<ServeHandle, AgentRuntimeError> + Send + Sync>;

/// Spawn `opencode serve` and return its endpoint as a [`ServeHandle`].
pub(super) fn default_connector() -> Connector {
    Arc::new(|setup| spawn_serve(&setup.cwd, &setup.env))
}

/// Basic-auth material the spawned server requires, read from the
/// documented environment variables the serve process itself honors.
///
/// `Debug` and `Clone` are derived: the struct is `pub(super)` and its
/// password is returned only into the request headers for the matching
/// spawn - it never lands in logs or receipts.
#[derive(Clone)]
pub(super) struct BasicAuth {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for BasicAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BasicAuth")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl BasicAuth {
    /// The credentials the spawned server enforces given the environment it
    /// actually runs with: the instance overlay wins over the inherited
    /// process environment, matching `Command::envs` precedence. `None`
    /// when neither asks for auth.
    pub(super) fn resolve(env: &std::collections::BTreeMap<String, String>) -> Option<Self> {
        let read = |name: &str| {
            env.get(name)
                .cloned()
                .or_else(|| std::env::var(name).ok())
                .filter(|value| !value.is_empty())
        };
        let password = read(SERVER_PASSWORD_ENV)?;
        let username =
            read(SERVER_USERNAME_ENV).unwrap_or_else(|| DEFAULT_SERVER_USERNAME.to_owned());
        Some(Self { username, password })
    }
}

/// The endpoint one `opencode serve` process exposes, plus ownership of
/// the process so teardown can terminate and reap it. `child` is `None`
/// on test transports, where no process exists to reap.
pub(super) struct ServeHandle {
    /// The server's base URL, `http://127.0.0.1:<port>`.
    pub base_url: reqwest::Url,
    /// Basic auth the server requires, from the environment convention.
    pub auth: Option<BasicAuth>,
    pub child: Option<Box<dyn TokioChildWrapper>>,
    /// The serve's stderr, captured for failure diagnostics.
    pub stderr: Option<Box<dyn AsyncRead + Unpin + Send>>,
}

/// Pick an ephemeral loopback port for the serve. The listener is
/// dropped before the serve binds; the brief race is acceptable because
/// a stale bind surfaces as a readiness failure, not a wrong server.
fn free_port() -> Result<u16, AgentRuntimeError> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .map_err(|error| {
            AgentRuntimeError::provider_error(format!(
                "could not reserve a loopback port for the opencode server: {error}"
            ))
        })
}

/// Spawn `opencode serve` on an ephemeral loopback port rooted at
/// `cwd`. Stdout is dropped (the serve logs its listen line to stderr
/// on some versions and stdout on others; stderr is captured for
/// diagnostics, stdout is not a protocol surface here).
pub(super) fn spawn_serve(
    cwd: &Path,
    env: &std::collections::BTreeMap<String, String>,
) -> Result<ServeHandle, AgentRuntimeError> {
    let port = free_port()?;
    for candidate in program_candidates("opencode") {
        let mut command = owned_command(&candidate, |command| {
            command
                .args([
                    "serve",
                    "--hostname",
                    "127.0.0.1",
                    "--port",
                    &port.to_string(),
                ])
                .current_dir(cwd)
                // The instance overlay applies to this child only - the
                // server sees it, the parent environment never mutates.
                .envs(env)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
        });
        match command.spawn() {
            Ok(mut child) => {
                let stderr = child
                    .stderr()
                    .take()
                    .map(|stderr| Box::new(stderr) as Box<dyn AsyncRead + Unpin + Send>);
                let base_url = format!("http://127.0.0.1:{port}/");
                return Ok(ServeHandle {
                    base_url: reqwest::Url::parse(&base_url).map_err(|error| {
                        AgentRuntimeError::provider_error(format!(
                            "the opencode server base URL is malformed: {error}"
                        ))
                    })?,
                    auth: BasicAuth::resolve(env),
                    child: Some(child),
                    stderr,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(AgentRuntimeError::provider_error(format!(
                    "could not start the opencode server: {error}"
                )));
            }
        }
    }
    Err(AgentRuntimeError::provider_error(
        "provider executable \"opencode\" was not found",
    ))
}
