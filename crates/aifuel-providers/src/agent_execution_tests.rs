#![cfg(unix)]

use super::{CliExecutionAdapter, ExecutionCapabilities, parse_public_output};
use aifuel_core::{
    AccessMode, AgentExecutionAdapter, AgentRunError, OutputFormat, ProviderKey,
    RunCancellationToken, RunRequest, RunStatus,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("aifuel-run-cancel-{}-{suffix}", std::process::id()));
        fs::create_dir(&path).expect("test directory should be creatable");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn no_arguments(_request: &RunRequest) -> Result<Vec<String>, AgentRunError> {
    Ok(Vec::new())
}

fn request() -> RunRequest {
    RunRequest {
        provider: ProviderKey::Gemini,
        model: None,
        effort: None,
        account: None,
        prompt: "hello".to_owned(),
        output: OutputFormat::Text,
        working_directory: None,
        access: AccessMode::ReadOnly,
        resume: None,
        timeout: None,
    }
}

fn shell_quote(path: &std::path::Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[test]
fn cancellation_kills_and_reaps_the_provider_process_and_keeps_partial_output() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TestDirectory::new();
    let marker = directory.0.join("started");
    let executable = directory.0.join("gemini");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--help\" ]; then printf '%s\\n' '--prompt --approval-mode --output-format'; exit 0; fi\nprintf 'partial provider output\\n'\nprintf started > {}\nexec sleep 30\n",
            shell_quote(&marker)
        ),
    )
    .expect("fake provider should be writable");
    let mut permissions = fs::metadata(&executable)
        .expect("fake provider should exist")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).expect("fake provider should be executable");

    let program: &'static str =
        Box::leak(executable.to_string_lossy().into_owned().into_boxed_str());
    let adapter = CliExecutionAdapter::new(
        ProviderKey::Gemini,
        program,
        &["--help"],
        &["--prompt", "--approval-mode", "--output-format"],
        no_arguments,
        parse_public_output,
        ExecutionCapabilities::new(true, false, true, true),
    );
    let request = request();
    let cancellation = RunCancellationToken::new();

    let result = thread::scope(|scope| {
        let cancellation_for_run = cancellation.clone();
        let handle = scope.spawn(move || adapter.execute(&request, &cancellation_for_run));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !marker.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let started = marker.exists();
        cancellation.cancel();
        let result = handle.join().expect("execution worker should not panic");
        assert!(started, "provider process should reach its armed state");
        result
    })
    .expect("cancelled execution should return its result");

    assert_eq!(result.status, RunStatus::Cancelled);
    assert!(!result.timed_out);
    assert!(result.output.contains("partial provider output"));
    assert_eq!(result.session_id, None);
}
