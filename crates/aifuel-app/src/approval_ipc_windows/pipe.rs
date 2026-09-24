use serde::Serialize;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    ERROR_IO_PENDING, ERROR_OPERATION_ABORTED, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE,
    HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT, WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::security;
use crate::approval_ipc_protocol::{ApprovalMessage, ApprovalReply, LocalApprovalDecision};

const MAX_FRAME_BYTES: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) const SERVER_PIPE_MODE: u32 =
    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
pub(super) const SERVER_PIPE_OPEN_MODE: u32 =
    PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE;

pub(super) fn create_server_pipe(pipe_name: &str) -> io::Result<Arc<OwnedHandle>> {
    let pipe_name_wide = security::wide_string(pipe_name);
    let descriptor = security::private_security_descriptor(false)?;
    let security_attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.as_ptr(),
        bInheritHandle: 0,
    };
    // SAFETY: the pipe name and explicit user-only SDDL descriptor remain live
    // for the call; mode/buffer sizes are bounded and the handle is not inherited.
    let raw_pipe = unsafe {
        CreateNamedPipeW(
            pipe_name_wide.as_ptr(),
            SERVER_PIPE_OPEN_MODE,
            SERVER_PIPE_MODE,
            1,
            MAX_FRAME_BYTES as u32,
            MAX_FRAME_BYTES as u32,
            0,
            &security_attributes,
        )
    };
    if raw_pipe == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateNamedPipeW returned a valid owned handle.
    Ok(Arc::new(unsafe { OwnedHandle::from_raw_handle(raw_pipe) }))
}

pub(super) fn spawn_server(
    pipe: Arc<OwnedHandle>,
    owner_id: String,
    stop: Arc<AtomicBool>,
    handler: Arc<
        impl Fn(String, String, LocalApprovalDecision) -> Result<bool, String> + Send + Sync + 'static,
    >,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("aifuel-local-approval".to_owned())
        .spawn(move || {
            let handle = raw_handle(&pipe);
            while !stop.load(Ordering::Acquire) {
                match wait_for_client(handle, &stop) {
                    Ok(true) => handle_connection(handle, &owner_id, &handler, &stop),
                    Ok(false) => break,
                    Err(_) => break,
                }
                if stop.load(Ordering::Acquire) {
                    break;
                }
                // SAFETY: the owned server pipe handle is live and has
                // completed its previous client operation.
                unsafe { DisconnectNamedPipe(handle) };
            }
        })
}

pub(super) fn cancel_server(pipe: &OwnedHandle) {
    // SAFETY: the pipe remains owned by the manager/worker until after join.
    // CancelIoEx wakes any outstanding overlapped connect or frame I/O.
    unsafe { CancelIoEx(raw_handle(pipe), null_mut()) };
}

pub(super) fn exchange(pipe_name: &str, message: &ApprovalMessage) -> io::Result<ApprovalReply> {
    let deadline = Instant::now() + IO_TIMEOUT;
    let pipe_name_wide = security::wide_string(pipe_name);
    // SAFETY: the name came from a private, validated owner record. The server
    // has a current-user-only ACL and rejects remote clients; wait is bounded.
    if unsafe { WaitNamedPipeW(pipe_name_wide.as_ptr(), remaining_millis(deadline)) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: opens an existing pipe; no new object/ACL is created by this call.
    let raw_client = unsafe {
        CreateFileW(
            pipe_name_wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            null_mut(),
        )
    };
    if raw_client == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned a valid owned client handle.
    let client = unsafe { OwnedHandle::from_raw_handle(raw_client) };
    let handle = raw_handle(&client);
    write_frame(handle, message, deadline, None)?;
    let frame = read_frame(handle, deadline, None)?;
    serde_json::from_slice(&frame).map_err(io::Error::other)
}

fn handle_connection(
    handle: HANDLE,
    owner_id: &str,
    handler: &Arc<
        impl Fn(String, String, LocalApprovalDecision) -> Result<bool, String> + Send + Sync + 'static,
    >,
    stop: &AtomicBool,
) {
    let deadline = Instant::now() + IO_TIMEOUT;
    let frame = match read_frame(handle, deadline, Some(stop)) {
        Ok(frame) => frame,
        Err(_) => return,
    };
    let reply = match serde_json::from_slice::<ApprovalMessage>(&frame) {
        Ok(message) if message.owner_id == owner_id => {
            match handler(message.run_id, message.input_id, message.decision) {
                Ok(false) => ApprovalReply {
                    owner_found: false,
                    accepted: false,
                    message: None,
                },
                Ok(true) => ApprovalReply {
                    owner_found: true,
                    accepted: true,
                    message: None,
                },
                Err(error) => ApprovalReply {
                    owner_found: true,
                    accepted: false,
                    message: Some(error),
                },
            }
        }
        Ok(_) => ApprovalReply {
            owner_found: false,
            accepted: false,
            message: None,
        },
        Err(_) => ApprovalReply {
            owner_found: false,
            accepted: false,
            message: Some("invalid local approval request".to_owned()),
        },
    };
    let _ = write_frame(handle, &reply, deadline, Some(stop));
}

fn wait_for_client(handle: HANDLE, stop: &AtomicBool) -> io::Result<bool> {
    let (mut overlapped, event) = new_overlapped()?;
    // SAFETY: the pipe was created with FILE_FLAG_OVERLAPPED and the OVERLAPPED
    // remains live until its completion event is drained.
    let connected = unsafe { ConnectNamedPipe(handle, &mut overlapped) };
    if connected != 0 {
        return Ok(true);
    }
    let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    if error == ERROR_PIPE_CONNECTED {
        return Ok(true);
    }
    if error != ERROR_IO_PENDING {
        if stop.load(Ordering::Acquire) {
            return Ok(false);
        }
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    match wait_overlapped(handle, &mut overlapped, &event, None, Some(stop)) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(false),
        Err(error) => Err(error),
    }
}

fn read_frame(handle: HANDLE, deadline: Instant, stop: Option<&AtomicBool>) -> io::Result<Vec<u8>> {
    let mut frame = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = read_overlapped(handle, &mut buffer, deadline, stop)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "approval frame ended before its newline",
            ));
        }
        if let Some(newline) = buffer[..count].iter().position(|byte| *byte == b'\n') {
            if frame.len() + newline + 1 > MAX_FRAME_BYTES
                || buffer[newline + 1..count]
                    .iter()
                    .any(|byte| !byte.is_ascii_whitespace())
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "approval frame is too large or contains trailing data",
                ));
            }
            frame.extend_from_slice(&buffer[..newline]);
            return Ok(frame);
        }
        frame.extend_from_slice(&buffer[..count]);
        if frame.len() >= MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "approval frame is too large",
            ));
        }
    }
}

fn write_frame<T: Serialize>(
    handle: HANDLE,
    message: &T,
    deadline: Instant,
    stop: Option<&AtomicBool>,
) -> io::Result<()> {
    let mut frame = serde_json::to_vec(message).map_err(io::Error::other)?;
    if frame.len() + 1 > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "approval response exceeds the frame size limit",
        ));
    }
    frame.push(b'\n');
    let mut written = 0;
    while written < frame.len() {
        let count = write_overlapped(handle, &frame[written..], deadline, stop)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "approval frame could not be written",
            ));
        }
        written += count;
    }
    Ok(())
}

fn read_overlapped(
    handle: HANDLE,
    buffer: &mut [u8],
    deadline: Instant,
    stop: Option<&AtomicBool>,
) -> io::Result<usize> {
    let (mut overlapped, event) = new_overlapped()?;
    let mut transferred = 0_u32;
    // SAFETY: handle is an overlapped named pipe, buffer is writable for its
    // declared length, and OVERLAPPED/event stay alive through completion.
    if unsafe {
        ReadFile(
            handle,
            buffer.as_mut_ptr(),
            buffer.len().min(u32::MAX as usize) as u32,
            &mut transferred,
            &mut overlapped,
        )
    } != 0
    {
        return Ok(transferred as usize);
    }
    let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    if error != ERROR_IO_PENDING {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    wait_overlapped(handle, &mut overlapped, &event, Some(deadline), stop)
        .map(|count| count as usize)
}

fn write_overlapped(
    handle: HANDLE,
    buffer: &[u8],
    deadline: Instant,
    stop: Option<&AtomicBool>,
) -> io::Result<usize> {
    let (mut overlapped, event) = new_overlapped()?;
    let mut transferred = 0_u32;
    // SAFETY: handle is an overlapped named pipe, buffer is readable for its
    // declared length, and OVERLAPPED/event stay alive through completion.
    if unsafe {
        WriteFile(
            handle,
            buffer.as_ptr(),
            buffer.len().min(u32::MAX as usize) as u32,
            &mut transferred,
            &mut overlapped,
        )
    } != 0
    {
        return Ok(transferred as usize);
    }
    let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    if error != ERROR_IO_PENDING {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    wait_overlapped(handle, &mut overlapped, &event, Some(deadline), stop)
        .map(|count| count as usize)
}

fn wait_overlapped(
    handle: HANDLE,
    overlapped: &mut OVERLAPPED,
    event: &OwnedHandle,
    deadline: Option<Instant>,
    stop: Option<&AtomicBool>,
) -> io::Result<u32> {
    loop {
        if stop.is_some_and(|stop| stop.load(Ordering::Acquire)) {
            cancel_and_drain(handle, overlapped);
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "local approval server is stopping",
            ));
        }
        let wait_millis = match deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    cancel_and_drain(handle, overlapped);
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "local approval I/O exceeded ten seconds",
                    ));
                }
                let millis = remaining.as_millis().clamp(1, u32::MAX as u128) as u32;
                if stop.is_some() {
                    millis.min(100)
                } else {
                    millis
                }
            }
            None => 100,
        };
        // SAFETY: event is a live manual-reset event associated with this
        // OVERLAPPED request. Waits are finite so shutdown is observed promptly.
        match unsafe { WaitForSingleObject(event.as_raw_handle(), wait_millis) } {
            WAIT_OBJECT_0 => {
                let mut transferred = 0_u32;
                // SAFETY: the event is signaled, so this request completed.
                if unsafe { GetOverlappedResult(handle, overlapped, &mut transferred, 0) } != 0 {
                    return Ok(transferred);
                }
                let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
                if error == ERROR_OPERATION_ABORTED {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "local approval I/O was canceled",
                    ));
                }
                return Err(io::Error::from_raw_os_error(error as i32));
            }
            WAIT_TIMEOUT => {}
            _ => {
                let error = io::Error::last_os_error();
                cancel_and_drain(handle, overlapped);
                return Err(error);
            }
        }
    }
}

fn cancel_and_drain(handle: HANDLE, overlapped: &mut OVERLAPPED) {
    // SAFETY: this exact OVERLAPPED belongs to the outstanding request on this
    // handle. GetOverlappedResult(TRUE) waits until it is safe to drop the value.
    unsafe {
        CancelIoEx(handle, overlapped);
        let mut transferred = 0_u32;
        GetOverlappedResult(handle, overlapped, &mut transferred, 1);
    }
}

fn new_overlapped() -> io::Result<(OVERLAPPED, OwnedHandle)> {
    // SAFETY: CreateEventW requests a manual-reset, nonsignaled event with no
    // inherited handle or name.
    let raw_event = unsafe { CreateEventW(null(), 1, 0, null()) };
    if raw_event.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateEventW returned a valid owned handle.
    let event = unsafe { OwnedHandle::from_raw_handle(raw_event) };
    // SAFETY: OVERLAPPED is a C structure with reserved fields zero-initialized.
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.hEvent = event.as_raw_handle();
    Ok((overlapped, event))
}

fn raw_handle(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle()
}

fn remaining_millis(deadline: Instant) -> u32 {
    deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .clamp(1, u32::MAX as u128) as u32
}
