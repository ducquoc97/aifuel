//! Platform helpers: timestamp conversion, column-type errors, and owner
//! process liveness used by stale-run reconciliation.

use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn invalid_text(column: usize) -> rusqlite::Error {
    rusqlite::Error::InvalidColumnType(column, "text".to_owned(), rusqlite::types::Type::Text)
}

pub(super) fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(unix)]
pub(super) fn pid_alive(pid: u32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    if unsafe { kill(pid as i32, 0) } == 0 {
        return true;
    }
    // EPERM means the process exists but belongs to another user.
    std::io::Error::last_os_error().raw_os_error() == Some(1)
}

#[cfg(windows)]
pub(super) fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut code = 0u32;
    let queried = unsafe { GetExitCodeProcess(handle, &mut code) };
    unsafe { CloseHandle(handle) };
    const STILL_ACTIVE: u32 = 259;
    queried != 0 && code == STILL_ACTIVE
}

#[cfg(not(any(unix, windows)))]
pub(super) fn pid_alive(_pid: u32) -> bool {
    // Without a liveness check, conservatively leave foreign rows untouched.
    true
}
