use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    pub fn new(prefix: &str) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock should be after the unix epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("aifuel-{prefix}-{}-{suffix}", std::process::id()));
        fs::create_dir_all(&path).expect("test directory should be creatable");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn ai_fuel_config_dir(root: &Path) -> PathBuf {
    #[cfg(windows)]
    let path = root.join("aifuel");

    #[cfg(target_os = "macos")]
    let path = root
        .join("Library")
        .join("Application Support")
        .join("aifuel");

    #[cfg(all(unix, not(target_os = "macos")))]
    let path = root.join(".config").join("aifuel");

    path
}

pub fn install_fake_command(directory: &Path, command_name: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let path = directory.join(command_name);
        let help_output = if command_name == "agy" {
            "printf 'exec --prompt --approval-mode --output-format --print --permission-mode --sandbox --plan\\n' >&2"
        } else {
            "printf 'exec --prompt --approval-mode --output-format --print --permission-mode --sandbox --plan\\n'"
        };
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--help\" ]; then\n{help_output}\nexit 0\nfi\nif [ \"$1\" = \"exec\" ] && [ \"$2\" = \"--help\" ]; then\n{help_output}\nexit 0\nfi\nprintf 'fake {command_name} response: %s\\n' \"$*\"\n"
            ),
        )
        .expect("fake provider executable should be writable");
        let mut permissions = fs::metadata(&path)
            .expect("fake provider executable should exist")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)
            .expect("fake provider executable should be executable");
    }

    #[cfg(windows)]
    {
        let help_output = if command_name == "agy" {
            ">&2 echo exec --prompt --approval-mode --output-format --print --permission-mode --sandbox --plan"
        } else {
            "echo exec --prompt --approval-mode --output-format --print --permission-mode --sandbox --plan"
        };
        fs::write(
            directory.join(format!("{command_name}.cmd")),
            format!(
                "@echo off\nif \"%~1\"==\"--help\" ({help_output} & exit /b 0)\nif \"%~1\"==\"exec\" if \"%~2\"==\"--help\" ({help_output} & exit /b 0)\necho fake {command_name} response: %*\n"
            ),
        )
        .expect("fake provider executable should be writable");
    }
}

/// Install a small Codex App Server fixture that speaks JSONL over stdio.
/// Returns the path where it records each request frame.
pub fn install_fake_codex_app_server(directory: &Path) -> PathBuf {
    let log_path = directory.join("codex-app-server.jsonl");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let executable = directory.join("codex");
        fs::write(
            &executable,
            r##"#!/bin/sh
if [ "$1" != "app-server" ] || [ "$2" != "--stdio" ]; then
    printf 'unexpected Codex invocation: %s\n' "$*" >&2
    exit 64
fi
while IFS= read -r line; do
    if [ -n "${AIFUEL_CODEX_FIXTURE_LOG:-}" ]; then
        printf '%s\n' "$line" >> "$AIFUEL_CODEX_FIXTURE_LOG"
    fi
    case "$line" in
        *'"id":0'*)
            printf '%s\n' '{"id":0,"result":{}}'
            ;;
        *'"id":1'*)
            printf '%s\n' '{"id":1,"result":{"thread":{"id":"fixture-thread"}}}'
            ;;
        *'"id":2'*)
            printf '%s\n' '{"id":2,"result":{}}'
            if [ "${AIFUEL_CODEX_FIXTURE_APPROVAL:-}" = "1" ]; then
                printf '%s\n' '{"id":"approval-1","method":"item/commandExecution/requestApproval","params":{"command":"touch fixture","reason":"run a command"}}'
                continue
            fi
            if [ -n "${AIFUEL_CODEX_FIXTURE_DELAY_SECONDS:-}" ]; then
                sleep "$AIFUEL_CODEX_FIXTURE_DELAY_SECONDS"
            fi
            printf '%s\n' '{"method":"item/agentMessage/delta","params":{"delta":"fake codex app-server response"}}'
            printf '%s\n' '{"method":"turn/completed","params":{"turn":{"status":"completed"}}}'
            ;;
        *'"decision":"accept"'*)
            printf '%s\n' '{"method":"item/agentMessage/delta","params":{"delta":"fake codex app-server response"}}'
            printf '%s\n' '{"method":"turn/completed","params":{"turn":{"status":"completed"}}}'
            ;;
    esac
done
"##,
        )
        .expect("fake Codex App Server should be writable");
        let mut permissions = fs::metadata(&executable)
            .expect("fake Codex executable should exist")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(executable, permissions)
            .expect("fake Codex executable should be executable");
    }

    #[cfg(windows)]
    {
        let script = directory.join("codex-app-server.ps1");
        fs::write(
            &script,
            r#"
$ErrorActionPreference = 'Stop'
while ($null -ne ($line = [Console]::In.ReadLine())) {
    if ($env:AIFUEL_CODEX_FIXTURE_LOG) {
        [System.IO.File]::AppendAllText($env:AIFUEL_CODEX_FIXTURE_LOG, $line + "`n")
    }
    $request = ConvertFrom-Json $line
    if ($null -eq $request.id) { continue }
    switch ([string]$request.id) {
        '0' { [Console]::Out.WriteLine('{"id":0,"result":{}}') }
        '1' { [Console]::Out.WriteLine('{"id":1,"result":{"thread":{"id":"fixture-thread"}}}') }
        '2' {
            [Console]::Out.WriteLine('{"id":2,"result":{}}')
            if ($env:AIFUEL_CODEX_FIXTURE_DELAY_SECONDS) {
                Start-Sleep -Seconds ([int]$env:AIFUEL_CODEX_FIXTURE_DELAY_SECONDS)
            }
            [Console]::Out.WriteLine('{"method":"item/agentMessage/delta","params":{"delta":"fake codex app-server response"}}')
            [Console]::Out.WriteLine('{"method":"turn/completed","params":{"turn":{"status":"completed"}}}')
        }
    }
}
"#,
        )
        .expect("fake Codex App Server script should be writable");
        fs::write(
            directory.join("codex.cmd"),
            "@echo off\n%SystemRoot%\\System32\\WindowsPowerShell\\v1.0\\powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File \"%~dp0codex-app-server.ps1\"\n",
        )
        .expect("fake Codex command wrapper should be writable");
    }

    log_path
}

pub fn path_with(directory: &Path) -> std::ffi::OsString {
    let mut paths = vec![directory.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(paths).expect("test PATH should be joinable")
}

pub fn start_gemini_fixture() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fixture server should bind");
    let address = listener
        .local_addr()
        .expect("fixture address should be available");
    let server = thread::spawn(move || {
        for stream in listener.incoming().take(2) {
            let mut stream = stream.expect("fixture connection should open");
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            loop {
                let count = stream
                    .read(&mut buffer)
                    .expect("request should be readable");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&request);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("");
            let body = if path.ends_with("loadCodeAssist") {
                r#"{"currentTier":{"id":"free","name":"Free"},"cloudaicompanionProject":"test-project"}"#
            } else {
                r#"{"buckets":[{"modelId":"gemini-3-flash","remainingFraction":0.5,"resetTime":"2030-01-01T00:00:00Z"}]}"#
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("fixture response should be writable");
        }
    });
    (format!("http://{address}/"), server)
}
