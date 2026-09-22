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

pub fn install_fake_gemini(directory: &Path) {
    install_fake_command(directory, "gemini");
}

#[cfg(unix)]
pub fn install_slow_help_gemini(directory: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let path = directory.join("gemini");
    fs::write(
        &path,
        "#!/bin/sh\nif [ \"$1\" = \"--help\" ]; then\nsleep 2\nexit 0\nfi\nprintf 'unexpected execution\\n'\n",
    )
    .expect("slow fake Gemini executable should be writable");
    let mut permissions = fs::metadata(&path)
        .expect("slow fake Gemini executable should exist")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .expect("slow fake Gemini executable should be executable");
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
