use std::ffi::c_void;
use std::fs;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicU64, Ordering};
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
    PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

static NEXT_OWNER_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: Windows allocated this buffer for the caller, and it has
            // not been freed or transferred to another owner.
            unsafe { LocalFree(self.0) };
        }
    }
}

pub(super) struct SecurityDescriptor {
    allocation: LocalAllocation,
    sddl: Vec<u16>,
}

impl SecurityDescriptor {
    pub(super) fn as_ptr(&self) -> *mut c_void {
        self.allocation.0
    }

    pub(super) fn sddl(&self) -> &[u16] {
        &self.sddl
    }
}

pub(super) fn prepare_private_directory(directory: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(directory)?;
    let metadata = fs::symlink_metadata(directory)?;
    if !real_directory(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "local approval directory must be a real directory",
        ));
    }
    let directory = fs::canonicalize(directory)?;
    set_private_dacl(&directory, true)?;
    if !private_dacl_matches(&directory, true)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local approval directory DACL is not restricted to the current user",
        ));
    }
    Ok(directory)
}

pub(super) fn canonical_private_directory(directory: &Path) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(directory)
        .map_err(|error| format!("could not inspect local approval directory: {error}"))?;
    if !real_directory(&metadata) {
        return Err("local approval directory must be a real directory".to_owned());
    }
    let directory = fs::canonicalize(directory)
        .map_err(|error| format!("could not resolve local approval directory: {error}"))?;
    if !private_dacl_matches(&directory, true)
        .map_err(|error| format!("could not validate local approval directory ACL: {error}"))?
    {
        return Err("local approval directory is not private to the current user".to_owned());
    }
    Ok(directory)
}

pub(super) fn real_directory(metadata: &fs::Metadata) -> bool {
    metadata.is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
}

pub(super) fn private_security_descriptor(inherit: bool) -> io::Result<SecurityDescriptor> {
    let sid = current_user_sid_string()?;
    let sddl = if inherit {
        format!("D:P(A;OICI;GA;;;{sid})")
    } else {
        format!("D:P(A;;GA;;;{sid})")
    };
    let sddl_wide = wide_string(&sddl);
    let mut descriptor = null_mut();
    // SAFETY: the SDDL is a valid NUL-terminated descriptor built from the
    // current process token SID; Windows allocates the output for LocalFree.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_wide.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
        || descriptor.is_null()
    {
        return Err(io::Error::last_os_error());
    }
    Ok(SecurityDescriptor {
        allocation: LocalAllocation(descriptor),
        sddl: sddl_wide,
    })
}

pub(super) fn set_private_dacl(path: &Path, inherit: bool) -> io::Result<()> {
    let path_wide = wide_path(path);
    let descriptor = private_security_descriptor(inherit)?;
    let mut present = 0;
    let mut dacl = null_mut();
    let mut defaulted = 0;
    // SAFETY: descriptor is a valid self-relative security descriptor returned
    // by ConvertStringSecurityDescriptorToSecurityDescriptorW.
    if unsafe {
        GetSecurityDescriptorDacl(descriptor.as_ptr(), &mut present, &mut dacl, &mut defaulted)
    } == 0
        || present == 0
        || dacl.is_null()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "generated local approval DACL is invalid",
        ));
    }
    // SAFETY: path and DACL pointers remain alive for the call. This replaces
    // the DACL and marks it protected against broader inherited access.
    let error = unsafe {
        SetNamedSecurityInfoW(
            path_wide.as_ptr() as *mut u16,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            dacl,
            null_mut(),
        )
    };
    if error == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(error as i32))
    }
}

pub(super) fn private_dacl_matches(path: &Path, inherit: bool) -> io::Result<bool> {
    let path_wide = wide_path(path);
    let mut descriptor = null_mut();
    // SAFETY: output fields are valid pointers; Windows allocates the returned
    // descriptor and LocalAllocation releases it below.
    let error = unsafe {
        GetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    if descriptor.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned a null local approval security descriptor",
        ));
    }
    let descriptor = LocalAllocation(descriptor);
    let mut actual = null_mut();
    let mut length = 0;
    // SAFETY: descriptor is valid and the output is allocated by Windows.
    if unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor.0,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut actual,
            &mut length,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let actual = LocalAllocation(actual.cast());
    // SAFETY: Windows returned a valid NUL-terminated UTF-16 allocation.
    let actual = unsafe { wide_ptr_string(actual.0.cast()) }?;
    let expected = private_security_descriptor(inherit)?;
    let expected = String::from_utf16(&expected.sddl()[..expected.sddl().len() - 1])
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(actual == expected)
}

pub(super) fn next_owner_id() -> io::Result<String> {
    let mut nonce = [0_u8; 16];
    // SAFETY: the buffer is writable for the declared length and the system
    // preferred RNG accepts a null algorithm handle.
    let status = unsafe {
        BCryptGenRandom(
            null_mut(),
            nonce.as_mut_ptr(),
            nonce.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(io::Error::other(format!(
            "Windows system RNG failed with NTSTATUS 0x{:08x}",
            status as u32
        )));
    }
    let sequence = NEXT_OWNER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nonce = nonce
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("o-{:x}-{sequence:08x}-{nonce}", std::process::id()))
}

pub(super) fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

pub(super) fn wide_string(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

unsafe fn wide_ptr_string(pointer: *const u16) -> io::Result<String> {
    if pointer.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned a null UTF-16 string",
        ));
    }
    let mut length = 0;
    // SAFETY: caller guarantees the pointer references a valid NUL-terminated
    // UTF-16 allocation returned by a Windows API.
    unsafe {
        while *pointer.add(length) != 0 {
            length += 1;
        }
        String::from_utf16(std::slice::from_raw_parts(pointer, length))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

fn current_user_sid_string() -> io::Result<String> {
    let mut token = null_mut();
    // SAFETY: GetCurrentProcess returns a valid pseudo-handle, and token is a
    // writable out-pointer. TOKEN_QUERY is the least privilege needed here.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcessToken returned a valid owned token handle.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut needed = 0_u32;
    // SAFETY: null buffer and zero length request the required TokenUser size.
    unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut needed) };
    if needed < std::mem::size_of::<TOKEN_USER>() as u32 || needed > 64 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "could not determine current user token information size",
        ));
    }
    let word_count = (needed as usize).div_ceil(std::mem::size_of::<u64>());
    let mut buffer = vec![0_u64; word_count];
    // SAFETY: buffer is aligned, writable, and at least `needed` bytes long.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: TokenUser data was written into this aligned buffer and its
    // returned structure is at least size_of::<TOKEN_USER>() bytes long.
    let user = unsafe { std::ptr::read(buffer.as_ptr().cast::<TOKEN_USER>()) };
    if user.User.Sid.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "current process token does not contain a user SID",
        ));
    }
    let mut sid_string = null_mut();
    // SAFETY: the SID pointer refers into the live token-information buffer;
    // Windows allocates the NUL-terminated result for LocalFree.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid_string) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let sid_string = LocalAllocation(sid_string.cast());
    // SAFETY: Windows returned a valid NUL-terminated SID string allocation.
    unsafe { wide_ptr_string(sid_string.0.cast()) }
}
