//! Bounded, non-interactive native Agent Integration inspection.

use aifuel_core::{
    AgentAuthenticationEvidence, AgentAuthenticationState, AgentCapability,
    AgentCapabilityEvidence, AgentIntegrationInfo, AgentPresenceEvidence, AgentPresenceState,
    AgentVersionEvidence, ProviderKey,
};
use process_wrap::std::{StdChildWrapper, StdCommandWrap};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(windows)]
use process_wrap::std::JobObject;
#[cfg(unix)]
use process_wrap::std::ProcessGroup;

pub(crate) const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const AUTHENTICATION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_VERSION_OUTPUT_BYTES: u64 = 16 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const AUTHENTICATION_NOT_INSPECTED_REASON: &str = "authentication is not inspected during listing to avoid reading local credentials or starting an auth flow; use provider setup guidance for manual login and checks";

pub(crate) fn inspect_agent(
    provider: ProviderKey,
    program: &str,
    version_args: Option<&'static [&'static str]>,
    authentication_args: Option<&'static [&'static str]>,
    declared_capabilities: impl IntoIterator<Item = (AgentCapability, AgentCapabilityEvidence)>,
) -> AgentIntegrationInfo {
    let declared_capabilities = declared_capabilities.into_iter().collect::<Vec<_>>();
    let path = match resolve_program(program) {
        Ok(Some(path)) => path,
        Ok(None) => {
            let reason = format!("native executable {program:?} was not found on PATH");
            return AgentIntegrationInfo::from_inspection(
                provider,
                AgentPresenceEvidence {
                    state: AgentPresenceState::Absent,
                    reason: reason.clone(),
                },
                AgentVersionEvidence {
                    version: None,
                    reason: format!("version is unknown because {reason}"),
                },
                AgentAuthenticationEvidence {
                    state: AgentAuthenticationState::Unknown,
                    reason: AUTHENTICATION_NOT_INSPECTED_REASON.to_owned(),
                },
                declared_capabilities,
            );
        }
        Err(reason) => {
            return AgentIntegrationInfo::from_inspection(
                provider,
                AgentPresenceEvidence {
                    state: AgentPresenceState::Unknown,
                    reason: reason.clone(),
                },
                AgentVersionEvidence {
                    version: None,
                    reason: format!("version is unknown because {reason}"),
                },
                AgentAuthenticationEvidence {
                    state: AgentAuthenticationState::Unknown,
                    reason: AUTHENTICATION_NOT_INSPECTED_REASON.to_owned(),
                },
                declared_capabilities,
            );
        }
    };

    let version = match version_args {
        None => AgentVersionEvidence {
            version: None,
            reason: "provider documentation does not establish a safe non-interactive version command; no process was started".to_owned(),
        },
        Some(args) => probe_version(&path, args),
    };
    let authentication = authentication_args.map_or_else(authentication_not_inspected, |args| {
        probe_authentication(&path, args)
    });
    AgentIntegrationInfo::from_inspection(
        provider,
        AgentPresenceEvidence {
            state: AgentPresenceState::Present,
            reason: format!("native executable {program:?} was found on PATH"),
        },
        version,
        authentication,
        declared_capabilities,
    )
}

fn authentication_not_inspected() -> AgentAuthenticationEvidence {
    AgentAuthenticationEvidence {
        state: AgentAuthenticationState::Unknown,
        reason: AUTHENTICATION_NOT_INSPECTED_REASON.to_owned(),
    }
}

pub(crate) fn resolve_program(program: &str) -> Result<Option<PathBuf>, String> {
    let names = executable_names(OsStr::new(program));
    let program_path = Path::new(program);
    if program_path.components().count() > 1 || program_path.is_absolute() {
        for name in names {
            let candidate = if name == OsStr::new(program) {
                program_path.to_path_buf()
            } else {
                program_path.with_file_name(name)
            };
            match is_executable_file(&candidate) {
                Ok(true) => {
                    return fs::canonicalize(&candidate)
                        .map(Some)
                        .map_err(|error| format!("could not resolve native executable: {error}"));
                }
                Ok(false) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("could not inspect native executable: {error}")),
            }
        }
        return Ok(None);
    }

    let path = std::env::var_os("PATH")
        .ok_or_else(|| "PATH is unavailable, so native presence is unknown".to_owned())?;
    for directory in std::env::split_paths(&path) {
        for name in &names {
            let candidate = directory.join(name);
            match is_executable_file(&candidate) {
                Ok(true) => {
                    return fs::canonicalize(&candidate)
                        .map(Some)
                        .map_err(|error| format!("could not resolve native executable: {error}"));
                }
                Ok(false) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("could not inspect native executable: {error}")),
            }
        }
    }
    Ok(None)
}

fn executable_names(program: &OsStr) -> Vec<OsString> {
    let path = Path::new(program);
    if path.extension().is_some() {
        return vec![program.to_os_string()];
    }
    #[cfg(windows)]
    {
        let extensions =
            std::env::var_os("PATHEXT").unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
        return extensions
            .to_string_lossy()
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| {
                let mut candidate = program.to_os_string();
                candidate.push(extension);
                candidate
            })
            .collect();
    }
    #[cfg(not(windows))]
    {
        vec![program.to_os_string()]
    }
}

fn is_executable_file(path: &Path) -> io::Result<bool> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        Ok(metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        Ok(true)
    }
}

fn probe_version(path: &Path, args: &[&str]) -> AgentVersionEvidence {
    let mut command = Command::new(path);
    command
        .args(args)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut command = StdCommandWrap::from(command);
    #[cfg(unix)]
    command.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(JobObject);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            return AgentVersionEvidence {
                version: None,
                reason: "the documented version command could not be started".to_owned(),
            };
        }
    };
    let Some(stdout) = child.stdout().take() else {
        terminate_and_reap(&mut child);
        return version_probe_unknown("the version command did not expose stdout");
    };
    let Some(stderr) = child.stderr().take() else {
        terminate_and_reap(&mut child);
        return version_probe_unknown("the version command did not expose stderr");
    };
    let stdout_reader = thread::spawn(move || read_limited(stdout));
    let stderr_reader = thread::spawn(move || read_limited(stderr));
    let deadline = Instant::now() + VERSION_PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Ok(None) => break None,
            Err(_) => {
                terminate_and_reap(&mut child);
                break None;
            }
        }
    };
    if status.is_none() {
        terminate_and_reap(&mut child);
        let _ = stdout_reader.join();
        let _ = stderr_reader.join();
        return version_probe_unknown("the documented version command exceeded the 2-second limit");
    }
    let stdout = match stdout_reader.join() {
        Ok(Ok(output)) => output,
        _ => return version_probe_unknown("could not capture bounded version output"),
    };
    let stderr = match stderr_reader.join() {
        Ok(Ok(output)) => output,
        _ => return version_probe_unknown("could not capture bounded version output"),
    };
    if stdout.truncated || stderr.truncated {
        return version_probe_unknown("version output exceeded the 16 KiB capture limit");
    }
    if !status.is_some_and(|status| status.success()) {
        return version_probe_unknown("the documented version command returned a failure status");
    }
    let output = format!("{}\n{}", stdout.text, stderr.text);
    match parse_version_token(&output) {
        Some(version) => AgentVersionEvidence {
            version: Some(version),
            reason: format!("reported by the documented {args:?} version command"),
        },
        None => version_probe_unknown(
            "the version command output did not contain one unambiguous version",
        ),
    }
}

fn probe_authentication(path: &Path, args: &[&str]) -> AgentAuthenticationEvidence {
    let mut command = Command::new(path);
    command
        .args(args)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut command = StdCommandWrap::from(command);
    #[cfg(unix)]
    command.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(JobObject);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            return authentication_probe_unknown(
                "the documented authentication status command could not be started",
            );
        }
    };
    let deadline = Instant::now() + AUTHENTICATION_PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => match status.code() {
                Some(0) => {
                    return AgentAuthenticationEvidence {
                        state: AgentAuthenticationState::Authenticated,
                        reason: "the documented local authentication status command reported signed in (exit status 0); stdout and stderr were discarded; live provider access was not validated".to_owned(),
                    };
                }
                Some(1) => {
                    return AgentAuthenticationEvidence {
                        state: AgentAuthenticationState::Unauthenticated,
                        reason: "the documented local authentication status command reported not signed in (exit status 1); stdout and stderr were discarded".to_owned(),
                    };
                }
                Some(code) => {
                    return authentication_probe_unknown(&format!(
                        "the documented authentication status command returned unexpected exit status {code}"
                    ));
                }
                None => {
                    return authentication_probe_unknown(
                        "the documented authentication status command terminated without an exit status",
                    );
                }
            },
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                terminate_and_reap(&mut child);
                return authentication_probe_unknown(
                    "the documented authentication status command exceeded the 2-second limit",
                );
            }
            Err(_) => {
                terminate_and_reap(&mut child);
                return authentication_probe_unknown(
                    "the documented authentication status command could not be observed",
                );
            }
        }
    }
}

fn authentication_probe_unknown(reason: &str) -> AgentAuthenticationEvidence {
    AgentAuthenticationEvidence {
        state: AgentAuthenticationState::Unknown,
        reason: format!(
            "{reason}; stdout and stderr were discarded; use provider setup guidance for manual login and checks"
        ),
    }
}

struct BoundedText {
    text: String,
    truncated: bool,
}

fn read_limited(reader: impl Read) -> io::Result<BoundedText> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_VERSION_OUTPUT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() as u64 > MAX_VERSION_OUTPUT_BYTES;
    bytes.truncate(MAX_VERSION_OUTPUT_BYTES as usize);
    Ok(BoundedText {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        truncated,
    })
}

fn parse_version_token(output: &str) -> Option<String> {
    let versions = output
        .split_whitespace()
        .map(|token| {
            token.trim_matches(|character: char| {
                !character.is_ascii_alphanumeric()
                    && character != '.'
                    && character != '-'
                    && character != '+'
                    && character != '_'
            })
        })
        .filter(|token| is_version_token(token))
        .collect::<std::collections::BTreeSet<_>>();
    (versions.len() == 1).then(|| {
        versions
            .into_iter()
            .next()
            .expect("one version token exists")
            .to_owned()
    })
}

fn is_version_token(token: &str) -> bool {
    let token = token.strip_prefix('v').unwrap_or(token);
    let numeric_parts = token
        .split(['.', '-', '+', '_'])
        .take_while(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
        .collect::<Vec<_>>();
    numeric_parts.len() >= 2 && numeric_parts.iter().all(|part| !part.is_empty())
}

fn version_probe_unknown(reason: &str) -> AgentVersionEvidence {
    AgentVersionEvidence {
        version: None,
        reason: reason.to_owned(),
    }
}

fn terminate_and_reap(child: &mut Box<dyn StdChildWrapper>) {
    let _ = child.start_kill();
    let _ = child.wait();
}

#[cfg(all(test, unix))]
#[path = "inspection_tests.rs"]
mod tests;
