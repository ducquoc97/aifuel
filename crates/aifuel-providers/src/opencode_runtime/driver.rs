//! The per-session protocol driver.
//!
//! One plain thread runs one `current_thread` tokio runtime per
//! session. A spawned task owns the `GET /event` SSE stream and pushes
//! decoded bus events over a channel; each prompt is a spawned
//! `POST /session/{id}/message` task whose response reports the run's
//! terminal facts through the same channel. The driver loop `select!`s
//! between adapter commands and server mail, so every run-scoped
//! emission happens inside the run's causal order.

mod dispatch;
mod events;
mod handshake;
mod teardown;

use super::serve::{BasicAuth, ServeHandle};
use super::session::{DriverCommand, OpenCodeSession, SessionSetup, SetupReport};
use crate::agent_execution::{MAX_CAPTURE_BYTES, read_bounded};
use crate::wire::sse::SseParser;
use aifuel_core::{RunCancellationToken, TokenUsage};
use serde_json::Value;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// The setup handshake deadline: readiness polling, the event-stream
/// connect, and session create or reattach share one bound. The serve
/// process is a full HTTP server with plugin init - cold starts measured
/// at 6-13s on modest hardware - so the bound is generous, unlike the
/// lightweight stdio servers other adapters drive.
const SETUP_TIMEOUT: Duration = Duration::from_secs(45);
/// How long teardown waits on stderr capture after the process dies.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// Control-request bound (`session` create/get, `abort`,
/// `permissions`), so a wedged server cannot park an adapter call.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the event reader wakes to check cancellation.
const READER_TICK: Duration = Duration::from_millis(100);

/// One decoded message from the serve, or the terminal fact of its
/// stream ending, or one prompt's terminal outcome.
pub(super) enum ServerMail {
    /// One `{type, properties}` bus event from `/event`.
    Event(Value),
    /// The SSE stream ended.
    StreamClosed,
    /// A read or decode failure on the SSE stream; the session cannot
    /// continue.
    StreamFailed(String),
    /// One prompt POST returned.
    PromptDone(PromptOutcome),
}

/// The terminal facts one prompt response carries.
pub(super) enum PromptOutcome {
    /// The response decoded as `{info, parts}`; `error_name` marks a
    /// provider-side failure such as `MessageAbortedError`.
    Completed {
        error_name: Option<String>,
        error_message: Option<String>,
        usage: Option<TokenUsage>,
        /// The response's part list, for the end-of-run completeness
        /// flush: any text the SSE stream did not yet surface.
        parts: Vec<Value>,
    },
    /// The request or response failed at the transport layer; a
    /// provider error would arrive through `Completed` instead.
    Failed(String),
}

/// How the driver loop ended, deciding the terminal facts teardown
/// emits.
pub(super) enum Exit {
    /// `stop` requested the shutdown.
    Requested,
    /// The adapter dropped the session's command sender.
    Dropped,
    /// The event stream ended.
    ServerClosed,
    /// Reading or decoding the event stream failed.
    ServerFailed(String),
    /// The setup handshake failed before the session was announced.
    SetupFailed,
}

/// The session driver entry point, run on a dedicated
/// `current_thread` runtime by [`OpenCodeSession::start_driver`].
pub(super) async fn run(
    session: Arc<OpenCodeSession>,
    connector: super::serve::Connector,
    setup: SessionSetup,
    setup_tx: mpsc::Sender<SetupReport>,
    mut commands: UnboundedReceiver<DriverCommand>,
) {
    let serve = match connector(&setup) {
        Ok(serve) => serve,
        Err(error) => {
            let _ = setup_tx.send(Err(error.message));
            return;
        }
    };
    let ServeHandle {
        base_url,
        auth,
        child,
        stderr,
    } = serve;
    *session.child.lock().expect("child mutex") = child;
    let stderr_task = stderr.map(|stderr| {
        tokio::spawn(async move {
            read_bounded(stderr, MAX_CAPTURE_BYTES)
                .await
                .map(|captured| captured.text)
        })
    });
    let client = match http_client() {
        Ok(client) => client,
        Err(message) => {
            let _ = setup_tx.send(Err(message));
            teardown::teardown(&session, stderr_task, Exit::SetupFailed).await;
            return;
        }
    };
    let (mail, mut messages) = tokio::sync::mpsc::unbounded_channel();
    let exit = match handshake::handshake(
        &session,
        &client,
        &base_url,
        auth.as_ref(),
        &setup,
        mail.clone(),
        &mut messages,
    )
    .await
    {
        Ok(provider_session) => {
            {
                session
                    .state
                    .lock()
                    .expect("session state mutex")
                    .provider_session = Some(provider_session.clone());
            }
            let _ = setup_tx.send(Ok(provider_session.clone()));
            main_loop(
                &session,
                &client,
                &base_url,
                auth.as_ref(),
                &setup,
                mail,
                &mut messages,
                &mut commands,
                provider_session,
            )
            .await
        }
        Err(reason) => {
            let _ = setup_tx.send(Err(reason));
            Exit::SetupFailed
        }
    };
    teardown::teardown(&session, stderr_task, exit).await;
}

/// The HTTP client: connection setup bounded, no total request timeout
/// because a prompt response legitimately spans the whole run.
fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("the HTTP client could not be built: {error}"))
}

/// Apply the serve's basic-auth credential when the environment
/// requires one.
pub(super) fn authed(
    request: reqwest::RequestBuilder,
    auth: Option<&BasicAuth>,
) -> reqwest::RequestBuilder {
    match auth {
        Some(auth) => request.basic_auth(&auth.username, Some(&auth.password)),
        None => request,
    }
}

/// The `GET /event` reader: chunked SSE bytes feed the shared parser,
/// complete events are decoded as JSON and mailed to the driver loop.
/// The read poll checks cancellation every tick so session teardown
/// unblocks it promptly.
pub(super) async fn event_loop(
    client: reqwest::Client,
    url: reqwest::Url,
    auth: Option<BasicAuth>,
    mail: UnboundedSender<ServerMail>,
    cancellation: RunCancellationToken,
) {
    let request = authed(client.get(url), auth.as_ref());
    let mut response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            let _ = mail.send(ServerMail::StreamFailed(format!(
                "the event stream could not be opened: {error}"
            )));
            return;
        }
    };
    if !response.status().is_success() {
        let _ = mail.send(ServerMail::StreamFailed(format!(
            "the event stream answered HTTP {}",
            response.status()
        )));
        return;
    }
    let mut parser = SseParser::new();
    let mut ticker = tokio::time::interval(READER_TICK);
    let mut done = false;
    while !done {
        tokio::select! {
            chunk = response.chunk() => match chunk {
                Ok(Some(bytes)) => {
                    if let Err(error) = parser.feed(&bytes) {
                        let _ = mail.send(ServerMail::StreamFailed(error.to_string()));
                        return;
                    }
                }
                Ok(None) => {
                    done = true;
                    if let Err(error) = parser.finish() {
                        let _ = mail.send(ServerMail::StreamFailed(error.to_string()));
                        return;
                    }
                }
                Err(error) => {
                    let _ = mail.send(ServerMail::StreamFailed(format!(
                        "the event stream failed: {error}"
                    )));
                    return;
                }
            },
            _ = ticker.tick() => {
                if cancellation.is_cancelled() {
                    done = true;
                }
            }
        }
        while let Some(event) = parser.next_event() {
            match serde_json::from_str::<Value>(&event.data) {
                Ok(event) => {
                    if mail.send(ServerMail::Event(event)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = mail.send(ServerMail::StreamFailed(format!(
                        "a server event could not be decoded: {error}"
                    )));
                    return;
                }
            }
        }
    }
    let _ = mail.send(ServerMail::StreamClosed);
}

/// One prompt task: `POST /session/{id}/message` blocks server-side
/// until the assistant message ends, then its response carries the
/// run's terminal facts. The task mails the outcome so the driver
/// stays the single event emitter.
pub(super) async fn prompt_task(
    client: reqwest::Client,
    url: reqwest::Url,
    auth: Option<BasicAuth>,
    body: Value,
    mail: UnboundedSender<ServerMail>,
) {
    let outcome = match authed(client.post(url), auth.as_ref())
        .json(&body)
        .send()
        .await
    {
        Err(error) => PromptOutcome::Failed(format!("the prompt request failed: {error}")),
        Ok(response) => {
            let status = response.status();
            match response.json::<Value>().await {
                Err(error) => PromptOutcome::Failed(format!(
                    "the prompt response could not be decoded (HTTP {status}): {error}"
                )),
                Ok(body) if !status.is_success() => {
                    let detail = super::protocol::error_message(&body["error"])
                        .unwrap_or_else(|| format!("HTTP {status}"));
                    PromptOutcome::Failed(format!(
                        "the server rejected the prompt (HTTP {status}): {detail}"
                    ))
                }
                Ok(body) => {
                    let (error_name, error_message, usage) =
                        super::protocol::assistant_verdict(&body);
                    let parts = body["parts"].as_array().cloned().unwrap_or_default();
                    PromptOutcome::Completed {
                        error_name,
                        error_message,
                        usage,
                        parts,
                    }
                }
            }
        }
    };
    let _ = mail.send(ServerMail::PromptDone(outcome));
}

/// The steady-state loop: adapter commands and server mail on one
/// select, commands first so host answers land before more events are
/// consumed.
#[allow(clippy::too_many_arguments)]
async fn main_loop(
    session: &Arc<OpenCodeSession>,
    client: &reqwest::Client,
    base_url: &reqwest::Url,
    auth: Option<&BasicAuth>,
    setup: &SessionSetup,
    mail: UnboundedSender<ServerMail>,
    messages: &mut UnboundedReceiver<ServerMail>,
    commands: &mut UnboundedReceiver<DriverCommand>,
    provider_session: String,
) -> Exit {
    let mut flow = dispatch::SessionFlow::default();
    loop {
        tokio::select! {
            biased;
            command = commands.recv() => match command {
                None => break Exit::Dropped,
                Some(DriverCommand::Shutdown) => break Exit::Requested,
                Some(command) => {
                    dispatch::on_command(
                        session, client, base_url, auth, setup, mail.clone(), &mut flow,
                        &provider_session, command,
                    )
                    .await;
                }
            },
            mail = messages.recv() => match mail {
                None | Some(ServerMail::StreamClosed) => break Exit::ServerClosed,
                Some(ServerMail::StreamFailed(reason)) => break Exit::ServerFailed(reason),
                Some(ServerMail::Event(event)) => {
                    events::on_event(session, &mut flow, &provider_session, &event);
                }
                Some(ServerMail::PromptDone(outcome)) => {
                    events::on_prompt_done(session, &mut flow, outcome);
                }
            },
        }
    }
}
