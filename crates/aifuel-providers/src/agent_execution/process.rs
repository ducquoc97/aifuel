//! Bounded provider-process I/O and ownership helpers.

use process_wrap::tokio::{KillOnDrop, TokioChildWrapper, TokioCommandWrap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt};

#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;

pub(crate) const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
const CAPTURE_READ_BUFFER_BYTES: usize = 16 * 1024;

pub(crate) fn owned_command(
    program: &str,
    configure: impl FnOnce(&mut tokio::process::Command),
) -> TokioCommandWrap {
    let mut command = TokioCommandWrap::with_new(program, |command| {
        // Managed providers must not recursively start another AI Fuel
        // execution owner. The executable boundary rejects this marker.
        command.env("AIFUEL_MANAGED_RUN", "1");
        configure(command);
    });
    #[cfg(unix)]
    command.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(JobObject);
    command.wrap(KillOnDrop);
    command
}

pub(crate) async fn kill_and_wait(
    child: &mut Box<dyn TokioChildWrapper>,
) -> io::Result<ExitStatus> {
    // Process-group/job wrappers terminate descendants as well as the direct
    // child. Ignore a race where the group has already exited, then always
    // wait so owned descendants are reaped before returning to the caller.
    let kill_error = child.start_kill().err();
    match Box::into_pin(child.wait()).await {
        Ok(status) => Ok(status),
        Err(wait_error) => Err(kill_error.unwrap_or(wait_error)),
    }
}

#[derive(Debug)]
pub(crate) struct CapturedOutput {
    pub(crate) text: String,
    #[allow(dead_code)]
    bytes: usize,
    #[allow(dead_code)]
    truncated: bool,
}

pub(crate) async fn read_bounded<R>(mut reader: R, limit: usize) -> io::Result<CapturedOutput>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = 0usize;
    let mut captured = Vec::with_capacity(limit.min(CAPTURE_READ_BUFFER_BYTES));
    let mut buffer = vec![0u8; CAPTURE_READ_BUFFER_BYTES];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        bytes = bytes.saturating_add(count);
        if captured.len() < limit {
            let remaining = limit - captured.len();
            captured.extend_from_slice(&buffer[..count.min(remaining)]);
        }
    }
    Ok(CapturedOutput {
        text: String::from_utf8_lossy(&captured).into_owned(),
        bytes,
        truncated: bytes > captured.len(),
    })
}

pub(crate) fn program_candidates(program: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        let mut candidates = vec![program.to_owned()];
        if !program.ends_with(".cmd") {
            candidates.push(format!("{program}.cmd"));
        }
        if !program.ends_with(".bat") {
            candidates.push(format!("{program}.bat"));
        }
        candidates
    }
    #[cfg(not(windows))]
    {
        vec![program.to_owned()]
    }
}

pub(super) struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    pub(super) fn new() -> Result<Self, io::Error> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aifuel-run-{}-{stamp}", std::process::id()));
        fs::create_dir(&path)?;
        Ok(Self { path })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}
