//! Session setup: `initialize`, optional `authenticate`,
//! `initialized`, then `session/new` or `session/load` depending on
//! resume and the agent's advertised capability.

use super::{METHOD_NOT_FOUND, SETUP_TIMEOUT, ServerMail, reject_server_request};
use crate::acp_runtime::interactions;
use crate::acp_runtime::mapping;
use crate::acp_runtime::protocol::send;
use crate::acp_runtime::session::{AcpSession, OpenedSession, SessionSetup};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncWrite;
use tokio::sync::mpsc::UnboundedReceiver;

/// The wire deadline the whole handshake shares.
fn deadline() -> Option<Instant> {
    Some(Instant::now() + SETUP_TIMEOUT)
}

/// Run the ACP handshake and report the opened session facts. On
/// failure the returned message becomes the `start` error.
pub(super) async fn handshake(
    session: &Arc<AcpSession>,
    stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
    messages: &mut UnboundedReceiver<ServerMail>,
    setup: &SessionSetup,
) -> Result<OpenedSession, String> {
    let deadline = deadline();

    // 1. initialize. Client capabilities stay honest: the bounded
    // workspace file services are implemented and advertised; the
    // terminal service is not.
    let message = json!({
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": {"readTextFile": true, "writeTextFile": true},
                "terminal": false,
            },
            "clientInfo": {"name": "aifuel", "version": env!("CARGO_PKG_VERSION")},
        },
    });
    send(stdin, message, deadline)
        .await
        .map_err(|error| error.to_string())?;
    let initialize = match await_response(stdin, messages, &json!(1), deadline).await? {
        Response::Value(response) => response,
        Response::Error(error) => return Err(format!("initialize failed: {error}")),
    };
    let capabilities = initialize
        .get("agentCapabilities")
        .cloned()
        .unwrap_or(Value::Null);
    let prompt_image = capabilities
        .get("promptCapabilities")
        .and_then(|capabilities| capabilities.get("image"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let load_session = capabilities
        .get("loadSession")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // 2. authenticate, only when the agent advertised methods. A single
    // advertised method is used; several require explicit configuration
    // rather than a guess. Agents that advertise none are presumed not
    // to need an interactive login.
    let auth_methods: Vec<&str> = initialize
        .get("authMethods")
        .and_then(Value::as_array)
        .map(|methods| {
            methods
                .iter()
                .filter_map(|method| method.get("id").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    if auth_methods.len() == 1 {
        let method = auth_methods[0].to_owned();
        let message = json!({
            "id": 2,
            "method": "authenticate",
            "params": {"methodId": method},
        });
        send(stdin, message, deadline)
            .await
            .map_err(|error| error.to_string())?;
        match await_response(stdin, messages, &json!(2), deadline).await? {
            Response::Value(_) => {}
            Response::Error(error) => {
                return Err(format!("authenticate({method}) failed: {error}"));
            }
        }
    } else if auth_methods.len() > 1 {
        // Several methods need an explicit policy choice this adapter
        // does not yet model; proceed unauthenticated so an agent that
        // accepts it still works.
    }

    // 3. initialized - some agents gate session calls on it.
    let message = json!({"method": "initialized", "params": {}});
    let _ = send(stdin, message, deadline).await;

    // 4. session/new or session/load. A resume cursor on an agent that
    // cannot load sessions is an honest unsupported, not a silent
    // fresh session.
    let (method, params) = match (&setup.resume_cursor, load_session) {
        (Some(cursor), true) => (
            "session/load",
            json!({"sessionId": cursor, "cwd": setup.cwd, "mcpServers": []}),
        ),
        (Some(cursor), false) => {
            return Err(format!(
                "the agent does not support session/load; resume cursor {cursor:?} cannot be honored"
            ));
        }
        (None, _) => ("session/new", json!({"cwd": setup.cwd, "mcpServers": []})),
    };
    // `mcpServers` is a required field; the adapter does not inject any.
    let message = json!({"id": 3, "method": method, "params": params});
    send(stdin, message, deadline)
        .await
        .map_err(|error| error.to_string())?;
    let session_response = match await_response(stdin, messages, &json!(3), deadline).await? {
        Response::Value(response) => response,
        Response::Error(error) => {
            // A stale or rejected resume cursor is a hard `start`
            // failure, not a silent fresh session.
            return Err(format!("{method} failed: {error}"));
        }
    };
    let session_id = session_response
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{method} returned no sessionId"))?
        .to_owned();
    let mut model_option = session_response
        .get("configOptions")
        .and_then(interactions::model_option);

    // A requested model on an agent that advertises no model selector
    // cannot be honored; fail `start` rather than run a substitute.
    if let Some(model) = &setup.model
        && !model.is_empty()
        && model_option.is_none()
    {
        return Err(format!(
            "the agent advertised no model selector; {model:?} cannot be honored"
        ));
    }

    // 5. Apply the requested model through the advertised selector. A
    // model the selector does not list fails `start` honestly rather
    // than reaching the agent as a guess.
    if let (Some(model), Some(option)) = (setup.model.clone(), model_option.clone())
        && !model.is_empty()
        && option.current != model
    {
        interactions::selectable_model(&option, &model).map_err(|error| error.message)?;
        let message = json!({
            "id": 5,
            "method": "session/set_config_option",
            "params": {
                "sessionId": session_id,
                "configId": option.id,
                "value": model,
            },
        });
        send(stdin, message, deadline)
            .await
            .map_err(|error| error.to_string())?;
        match await_response(stdin, messages, &json!(5), deadline).await? {
            Response::Value(response) => {
                if let Some(options) = response.get("configOptions")
                    && let Some(updated) = interactions::model_option(options)
                {
                    model_option = Some(updated);
                }
            }
            Response::Error(error) => {
                return Err(format!("session/set_config_option failed: {error}"));
            }
        }
    }

    {
        let mut state = session.state.lock().expect("session state mutex");
        state.provider_session = Some(session_id.clone());
        state.prompt_image = prompt_image;
        state.model_option = model_option;
    }
    Ok(OpenedSession { session_id })
}

/// One awaited response: the result payload or the error message.
enum Response {
    Value(Value),
    Error(String),
}

/// Consume frames until the response for `id` arrives. Notifications
/// received during setup are mapped through the usual update path (a
/// session may emit progress during `session/load`); server requests
/// get explicit protocol errors so nothing parks waiting on a client
/// feature this adapter does not implement.
async fn await_response(
    stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
    messages: &mut UnboundedReceiver<ServerMail>,
    id: &Value,
    deadline: Option<Instant>,
) -> Result<Response, String> {
    loop {
        let remaining = match deadline {
            Some(deadline) => deadline.saturating_duration_since(Instant::now()),
            None => SETUP_TIMEOUT,
        };
        if remaining.is_zero() {
            return Err("the agent did not answer in time".to_owned());
        }
        match tokio::time::timeout(remaining, messages.recv()).await {
            Err(_) => return Err("the agent did not answer in time".to_owned()),
            Ok(None) | Ok(Some(ServerMail::Closed)) => {
                return Err("the agent closed its output during setup".to_owned());
            }
            Ok(Some(ServerMail::Failed(reason))) => return Err(reason),
            Ok(Some(ServerMail::Message(message))) => {
                if mapping::is_response_for(&message, id) {
                    return Ok(match mapping::response_result(&message) {
                        Ok(result) => Response::Value(result),
                        Err(error) => Response::Error(error),
                    });
                }
                if mapping::is_server_request(&message) {
                    reject_server_request(
                        stdin,
                        &message,
                        METHOD_NOT_FOUND,
                        "this client does not implement the requested method",
                        deadline,
                    )
                    .await;
                    continue;
                }
                // A notification during setup carries no run to attach
                // to; ignore it rather than emit unattributed facts.
            }
        }
    }
}
