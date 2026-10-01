//! The setup handshake: prove the serve speaks HTTP, open the `/event`
//! stream, wait for its `server.connected` greeting, then create or
//! reattach the OpenCode session. It owns no session state; the caller
//! learns the provider session id or the failure reason through its
//! return.

use super::{REQUEST_TIMEOUT, SETUP_TIMEOUT, ServerMail, authed, event_loop};
use crate::opencode_runtime::protocol;
use crate::opencode_runtime::serve::BasicAuth;
use crate::opencode_runtime::session::{OpenCodeSession, SessionSetup};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::{sleep, timeout};

/// How often readiness polls `GET /session` while the serve binds.
const READY_POLL: Duration = Duration::from_millis(75);

/// Returns the OpenCode session id the serve created or verified.
///
/// Failure order is deliberate: a serve that never answers is a spawn
/// problem, a silent event stream makes streaming claims unprovable,
/// and a rejected session create is the provider's own verdict.
pub(super) async fn handshake(
    session: &Arc<OpenCodeSession>,
    client: &reqwest::Client,
    base_url: &reqwest::Url,
    auth: Option<&BasicAuth>,
    setup: &SessionSetup,
    mail: UnboundedSender<ServerMail>,
    messages: &mut UnboundedReceiver<ServerMail>,
) -> Result<String, String> {
    let deadline = Instant::now() + SETUP_TIMEOUT;
    wait_ready(session, client, base_url, auth, setup, deadline).await?;
    let events_url = protocol::endpoint(base_url, protocol::EVENTS_PATH, &setup.cwd)
        .map_err(|error| error.message)?;
    tokio::spawn(event_loop(
        client.clone(),
        events_url,
        auth.cloned(),
        mail,
        session.reader_cancel.clone(),
    ));
    wait_connected(messages, deadline).await?;
    match &setup.resume_cursor {
        Some(cursor) => verify_session(client, base_url, auth, setup, cursor, deadline)
            .await
            .map(|_| cursor.clone()),
        None => create_session(client, base_url, auth, setup, deadline).await,
    }
}

/// Poll `GET /session` until the serve answers any HTTP response - a
/// 4xx still proves HTTP is up - or the deadline or an early process
/// exit proves it is not.
async fn wait_ready(
    session: &Arc<OpenCodeSession>,
    client: &reqwest::Client,
    base_url: &reqwest::Url,
    auth: Option<&BasicAuth>,
    setup: &SessionSetup,
    deadline: Instant,
) -> Result<(), String> {
    let url = protocol::endpoint(base_url, protocol::SESSIONS_PATH, &setup.cwd)
        .map_err(|error| error.message)?;
    loop {
        if child_exited(session) {
            return Err("the opencode server exited during startup".to_owned());
        }
        match authed(client.get(url.clone()), auth)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
        {
            Ok(_) => return Ok(()),
            Err(error) if Instant::now() >= deadline => {
                return Err(format!(
                    "the opencode server did not begin listening: {error}"
                ));
            }
            Err(_) => sleep(READY_POLL).await,
        }
    }
}

/// The spawned serve died before speaking HTTP.
fn child_exited(session: &Arc<OpenCodeSession>) -> bool {
    session
        .child
        .lock()
        .expect("child mutex")
        .as_mut()
        .and_then(|child| child.try_wait().ok().flatten())
        .is_some()
}

/// The serve writes `server.connected` as the first event on every
/// `/event` connection. Requiring it proves the declared streaming
/// surface is actually live before the session is announced.
async fn wait_connected(
    messages: &mut UnboundedReceiver<ServerMail>,
    deadline: Instant,
) -> Result<(), String> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("the opencode event stream did not report a connection".to_owned());
        }
        match timeout(remaining, messages.recv()).await {
            Err(_) => {
                return Err("the opencode event stream did not report a connection".to_owned());
            }
            Ok(Some(ServerMail::Event(event))) => {
                if event["type"].as_str() == Some(protocol::EV_CONNECTED) {
                    return Ok(());
                }
            }
            Ok(Some(ServerMail::StreamClosed)) | Ok(None) => {
                return Err("the opencode event stream closed during setup".to_owned());
            }
            Ok(Some(ServerMail::StreamFailed(reason))) => return Err(reason),
            // A prompt outcome cannot exist before the session opens;
            // ignore rather than panic on a misrouted mail.
            Ok(Some(ServerMail::PromptDone(_))) => {}
        }
    }
}

/// `POST /session` for a fresh session; the response's `id` is the
/// provider session identity and resume cursor.
async fn create_session(
    client: &reqwest::Client,
    base_url: &reqwest::Url,
    auth: Option<&BasicAuth>,
    setup: &SessionSetup,
    deadline: Instant,
) -> Result<String, String> {
    let url = protocol::endpoint(base_url, protocol::SESSIONS_PATH, &setup.cwd)
        .map_err(|error| error.message)?;
    let response = authed(client.post(url), auth)
        .json(&json!({}))
        .timeout(remaining(deadline)?)
        .send()
        .await
        .map_err(|error| format!("the session create request failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "the opencode server rejected the session create (HTTP {})",
            response.status()
        ));
    }
    let body = response
        .json::<serde_json::Value>()
        .await
        .map_err(|error| format!("the session create response could not be decoded: {error}"))?;
    protocol::session_id_of(&body)
        .ok_or_else(|| "the opencode server did not report a session id".to_owned())
}

/// `GET /session/{id}` reattaches a persisted provider session. A
/// missing or unreadable session fails `start`: the resume cursor must
/// still name a real provider session.
async fn verify_session(
    client: &reqwest::Client,
    base_url: &reqwest::Url,
    auth: Option<&BasicAuth>,
    setup: &SessionSetup,
    cursor: &str,
    deadline: Instant,
) -> Result<(), String> {
    let url = protocol::endpoint(
        base_url,
        &format!("{}/{cursor}", protocol::SESSIONS_PATH),
        &setup.cwd,
    )
    .map_err(|error| error.message)?;
    let response = authed(client.get(url), auth)
        .timeout(remaining(deadline)?)
        .send()
        .await
        .map_err(|error| format!("the session reattach request failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "the provider session {cursor} is no longer available (HTTP {})",
            response.status()
        ));
    }
    let body = response
        .json::<serde_json::Value>()
        .await
        .map_err(|error| format!("the session lookup response could not be decoded: {error}"))?;
    match protocol::session_id_of(&body) {
        Some(id) if id == cursor => Ok(()),
        _ => Err(format!("the provider session {cursor} did not verify")),
    }
}

/// Remaining handshake budget, or the setup timeout failure itself.
fn remaining(deadline: Instant) -> Result<Duration, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err("the opencode setup handshake timed out".to_owned())
    } else {
        Ok(remaining.min(REQUEST_TIMEOUT))
    }
}
