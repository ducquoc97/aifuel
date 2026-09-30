//! The setup handshake: write the `initialize` control request, then
//! wait for the provider's `system`/`init` frame, which carries the
//! claude session id this adapter uses as the resume cursor. It owns
//! no session state; the caller learns success or the failure reason
//! through its return.

use super::{HANDSHAKE_TIMEOUT, stderr_hint, write_line};
use crate::claude_runtime::protocol::{self, Frame};
use crate::claude_runtime::session::{ClaudeSession, DriverCommand, DriverInput, LineRead};
use std::io::Write;
use std::sync::Mutex;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Instant;

/// Write `initialize`, then wait for the provider's `system`/`init`
/// frame. Frames the CLI emits before init are safe to skip here; the
/// session handle does not exist yet, so nothing can observe them.
pub(super) fn handshake(
    session: &ClaudeSession,
    inbox: &mpsc::Receiver<DriverInput>,
    stdin: &mut dyn Write,
    stderr_tail: &Mutex<String>,
) -> Result<(), String> {
    let initialize = protocol::initialize_request(&session.next_wire_id("aifuel-init"));
    write_line(stdin, &initialize)
        .map_err(|error| format!("the control handshake could not reach the provider: {error}"))?;
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("the provider did not report its session id in time".to_owned());
        }
        match inbox.recv_timeout(remaining) {
            Ok(DriverInput::Line(LineRead::Data(line))) => {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                match protocol::parse_frame(&value) {
                    Frame::Init { session_id } => {
                        session
                            .state
                            .lock()
                            .expect("session state mutex")
                            .claude_session_id = session_id;
                        return Ok(());
                    }
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
                    "the provider process ended before reporting its session id{}",
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
                return Err("the provider did not report its session id in time".to_owned());
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err("the provider channel ended during startup".to_owned());
            }
        }
    }
}
