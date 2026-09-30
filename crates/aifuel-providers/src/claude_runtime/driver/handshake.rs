//! The setup handshake: write the `initialize` control request, then
//! wait for its `control_response` answer - the deterministic startup
//! signal. On current CLI versions `system`/`init` only follows the
//! first user message, so the provider session id is captured from
//! whichever frame reports it while the answer is in flight. The
//! caller learns success or the failure reason through the return.

use super::{HANDSHAKE_TIMEOUT, stderr_hint, write_line};
use crate::claude_runtime::protocol::{self, Frame};
use crate::claude_runtime::session::{ClaudeSession, DriverCommand, DriverInput, LineRead};
use std::io::Write;
use std::sync::Mutex;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Instant;

/// Write `initialize`, then wait for the `control_response` that
/// answers its request id. SessionStart hook frames may already carry
/// the provider session id, and `system`/`init` can lag until the
/// first turn, so neither is the startup signal; the timeout remains
/// as the wedged-spawn guard. Frames consumed while waiting are safe
/// to drop here: the session handle does not exist yet, so nothing
/// can observe them, and the session-id capture is all they owed.
pub(super) fn handshake(
    session: &ClaudeSession,
    inbox: &mpsc::Receiver<DriverInput>,
    stdin: &mut dyn Write,
    stderr_tail: &Mutex<String>,
) -> Result<(), String> {
    let request_id = session.next_wire_id("aifuel-init");
    let initialize = protocol::initialize_request(&request_id);
    write_line(stdin, &initialize)
        .map_err(|error| format!("the control handshake could not reach the provider: {error}"))?;
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("the provider did not answer the initialize request in time".to_owned());
        }
        match inbox.recv_timeout(remaining) {
            Ok(DriverInput::Line(LineRead::Data(line))) => {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                // Hook frames, init, and results all carry the provider
                // session id at top level; whichever reports first
                // becomes the resume cursor.
                if let Some(session_id) = protocol::frame_session_id(&value) {
                    session
                        .state
                        .lock()
                        .expect("session state mutex")
                        .claude_session_id = Some(session_id);
                }
                match protocol::parse_frame(&value) {
                    Frame::ControlResponse {
                        request_id: answered,
                        ok,
                        error,
                    } if answered == request_id => {
                        if ok {
                            return Ok(());
                        }
                        return Err(error.unwrap_or_else(|| {
                            "the provider refused the initialize request".to_owned()
                        }));
                    }
                    // Versions that emit `system`/`init` at startup
                    // have already proven the provider is up; waiting
                    // out the answer is then optional.
                    Frame::Init { .. } => return Ok(()),
                    Frame::Error(message) => return Err(message),
                    Frame::Result(result) if result.is_error => {
                        return Err(result
                            .message
                            .unwrap_or_else(|| "the provider session failed to start".to_owned()));
                    }
                    _ => {}
                }
            }
            Ok(DriverInput::Line(LineRead::Eof | LineRead::Error(_))) => {
                return Err(format!(
                    "the provider process ended before answering the initialize request{}",
                    stderr_hint(stderr_tail)
                ));
            }
            // Commands cannot arrive before the session is announced;
            // a shutdown racing setup still tears the driver down.
            Ok(DriverInput::Command(DriverCommand::Shutdown)) => {
                return Err("the session was closed during startup".to_owned());
            }
            Ok(DriverInput::Command(_)) => {}
            Err(RecvTimeoutError::Timeout) => {
                return Err("the provider did not answer the initialize request in time".to_owned());
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err("the provider channel ended during startup".to_owned());
            }
        }
    }
}
