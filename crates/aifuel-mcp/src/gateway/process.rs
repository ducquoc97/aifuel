use super::transport::RpcTransport;
use aifuel_app::{McpServerDefinition, SelectedMcpServer, StdioServerDefinition, default_cwd};
use process_wrap::tokio::{KillOnDrop, TokioChildWrapper, TokioCommandWrap};
use rmcp::RoleClient;
use std::env;
use std::ffi::OsString;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;

pub(crate) type LocalTransport = RpcTransport<ChildStdout, ChildStdin, RoleClient>;
pub(crate) type ResolvedEnvironment = Result<Vec<(String, OsString)>, LocalProcessError>;

pub(crate) struct SpawnedLocalServer {
    pub transport: LocalTransport,
    pub process: LocalProcess,
}

pub(crate) struct LocalProcess {
    child: Option<Box<dyn TokioChildWrapper>>,
    stderr_reader: Option<JoinHandle<()>>,
}

impl SpawnedLocalServer {
    pub fn spawn(
        server: &SelectedMcpServer,
        environment: ResolvedEnvironment,
        output_budget: Arc<Semaphore>,
        max_message_bytes: usize,
        write_stall: Duration,
    ) -> Result<Self, LocalProcessError> {
        let McpServerDefinition::Stdio(config) = &server.definition else {
            return Err(LocalProcessError::UnsupportedTransport);
        };
        let environment = environment?;
        let mut command = TokioCommandWrap::with_new(&config.command, |command| {
            command
                .args(&config.args)
                .current_dir(default_cwd(server))
                .env_clear()
                .envs(environment)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        });

        #[cfg(unix)]
        command.wrap(ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(JobObject);
        #[cfg(any(unix, windows))]
        command.wrap(KillOnDrop);
        #[cfg(not(any(unix, windows)))]
        command.wrap(KillOnDrop);

        let mut child = command
            .spawn()
            .map_err(|_| LocalProcessError::CouldNotStart)?;
        let child_stdout = child
            .stdout()
            .take()
            .ok_or(LocalProcessError::InvalidPipes)?;
        let child_stdin = child
            .stdin()
            .take()
            .ok_or(LocalProcessError::InvalidPipes)?;
        let child_stderr = child
            .stderr()
            .take()
            .ok_or(LocalProcessError::InvalidPipes)?;
        let stderr_reader = tokio::spawn(drain_stderr(child_stderr));
        let transport = RpcTransport::new(
            child_stdout,
            child_stdin,
            output_budget,
            None,
            max_message_bytes,
            write_stall,
        );

        Ok(Self {
            transport,
            process: LocalProcess {
                child: Some(child),
                stderr_reader: Some(stderr_reader),
            },
        })
    }
}

pub(crate) fn snapshot_environment(server: &SelectedMcpServer) -> ResolvedEnvironment {
    let McpServerDefinition::Stdio(config) = &server.definition else {
        return Err(LocalProcessError::UnsupportedTransport);
    };
    resolved_environment(config)
}

impl LocalProcess {
    pub async fn shutdown(&mut self, timeout: Duration) {
        if let Some(mut child) = self.child.take() {
            let deadline = tokio::time::Instant::now() + timeout;
            let _ = tokio::time::timeout_at(deadline, Box::into_pin(child.wait())).await;
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(1), Box::into_pin(child.wait())).await;
        }
        if let Some(mut reader) = self.stderr_reader.take()
            && tokio::time::timeout(Duration::from_secs(1), &mut reader)
                .await
                .is_err()
        {
            // The process wrapper kills the owned tree on drop. Stop a reader
            // that could not observe pipe closure during cleanup.
            reader.abort();
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum LocalProcessError {
    CouldNotStart,
    InvalidPipes,
    MissingEnvironmentReference,
    UnsupportedTransport,
}

impl std::fmt::Display for LocalProcessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CouldNotStart => {
                formatter.write_str("configured local MCP server could not start")
            }
            Self::InvalidPipes => {
                formatter.write_str("configured local MCP server pipes are unavailable")
            }
            Self::MissingEnvironmentReference => formatter
                .write_str("configured local MCP server has a missing environment reference"),
            Self::UnsupportedTransport => {
                formatter.write_str("selected server transport is not available in this gateway")
            }
        }
    }
}

impl std::error::Error for LocalProcessError {}

fn resolved_environment(config: &StdioServerDefinition) -> ResolvedEnvironment {
    let mut environment = Vec::new();
    #[cfg(unix)]
    let inherited = ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL"];
    #[cfg(windows)]
    let inherited = [
        "PATH",
        "SystemRoot",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "TEMP",
        "TMP",
    ];
    #[cfg(not(any(unix, windows)))]
    let inherited: [&str; 0] = [];

    for key in inherited {
        if let Some(value) = env::var_os(key) {
            insert_environment(&mut environment, key.to_owned(), value, cfg!(windows));
        }
    }
    for (key, value) in &config.env {
        insert_environment(
            &mut environment,
            key.clone(),
            value.value.clone().into(),
            cfg!(windows),
        );
    }
    for (key, source) in &config.env_from {
        let value = env::var_os(source).ok_or(LocalProcessError::MissingEnvironmentReference)?;
        insert_environment(&mut environment, key.clone(), value, cfg!(windows));
    }
    Ok(environment)
}

fn insert_environment(
    environment: &mut Vec<(String, OsString)>,
    key: String,
    value: OsString,
    case_insensitive: bool,
) {
    let existing = environment.iter_mut().find(|(existing, _)| {
        if case_insensitive {
            existing.eq_ignore_ascii_case(&key)
        } else {
            existing == &key
        }
    });
    if let Some((existing_key, existing_value)) = existing {
        *existing_key = key;
        *existing_value = value;
    } else {
        environment.push((key, value));
    }
}

async fn drain_stderr(mut stderr: ChildStderr) {
    let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
}

#[cfg(test)]
mod tests {
    use super::insert_environment;
    use std::ffi::OsString;

    #[test]
    fn explicit_windows_environment_values_replace_inherited_keys_case_insensitively() {
        let mut environment = vec![("PATH".to_owned(), OsString::from("inherited"))];

        insert_environment(
            &mut environment,
            "Path".to_owned(),
            OsString::from("configured"),
            true,
        );

        assert_eq!(environment.len(), 1);
        assert_eq!(environment[0].0, "Path");
        assert_eq!(environment[0].1, "configured");
    }
}
