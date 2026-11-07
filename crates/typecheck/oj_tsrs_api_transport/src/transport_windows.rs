//! Windows half of the IPC pipe transport (`tsc/internal/ipc/transport_windows.go`).

use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_REJECT_REMOTE_CLIENTS,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// A Win32 allocation returned by an SDDL conversion function.
pub struct LocalAllocation(pub *mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: the conversion functions allocate these pointers with LocalAlloc; this is their matching free.
        unsafe { LocalFree(self.0) };
    }
}

fn current_user_sid() -> io::Result<String> {
    let mut raw_token = ptr::null_mut();
    // SAFETY: the process pseudo-handle is valid; `raw_token` is a writable out-parameter.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful OpenProcessToken returned a newly owned, non-null handle.
    let token = unsafe { OwnedHandle::from_raw_handle(raw_token) };

    let mut bytes = 0;
    // SAFETY: a null buffer and zero length ask Windows for the required TokenUser size.
    let first_call = unsafe {
        GetTokenInformation(token.as_raw_handle(), TokenUser, ptr::null_mut(), 0, &mut bytes)
    };
    if first_call != 0 || bytes < size_of::<TOKEN_USER>() as u32 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid token user size"));
    }
    let first_error = io::Error::last_os_error();
    if first_error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
        return Err(first_error);
    }

    // GetTokenInformation writes a TOKEN_USER plus variable-length SID. usize storage supplies
    // TOKEN_USER's pointer alignment, which a Vec<u8> would not formally guarantee.
    let mut token_user = vec![0_usize; (bytes as usize).div_ceil(size_of::<usize>())];
    // SAFETY: the aligned allocation has at least `bytes` writable bytes and lives through SID conversion.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            token_user.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if bytes < size_of::<TOKEN_USER>() as u32 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid token user"));
    }
    // SAFETY: the successful call filled an aligned TOKEN_USER at the start of this allocation.
    let sid = unsafe { (*token_user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut sid_wide = ptr::null_mut();
    // SAFETY: `sid` points into the live TokenUser allocation; Windows writes a LocalAlloc result.
    if unsafe { ConvertSidToStringSidW(sid, &mut sid_wide) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _sid_allocation = LocalAllocation(sid_wide.cast());
    let mut sid_units = Vec::new();
    let mut cursor = sid_wide;
    // SAFETY: ConvertSidToStringSidW returns a NUL-terminated UTF-16 string. The allocation is
    // held by `_sid_allocation` until after the returned string has been constructed.
    unsafe {
        while *cursor != 0 {
            sid_units.push(*cursor);
            cursor = cursor.add(1);
        }
    }
    String::from_utf16(&sid_units)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid current user SID"))
}

/// Construct the upstream `D:P(A;;GA;;;current-user-SID)` descriptor. The protected DACL has
/// exactly one access-control entry, so an inherited grant cannot expose the API pipe.
pub fn private_security_descriptor() -> io::Result<LocalAllocation> {
    let mut sddl: Vec<u16> =
        format!("D:P(A;;GA;;;{})", current_user_sid()?).encode_utf16().collect();
    sddl.push(0);

    let mut descriptor = ptr::null_mut();
    // SAFETY: SDDL is NUL-terminated and both output pointers are valid. Windows allocates the
    // returned self-relative descriptor with LocalAlloc.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalAllocation(descriptor))
}

/// A listener owns its first pipe instance from bind until the single accept. This makes the
/// name available before the client tries to connect, matching the Go listener contract.
pub struct PipeListener {
    path: PathBuf,
    pub pipe: Mutex<Option<File>>,
}

impl PipeListener {
    pub fn bind_private(name: &str) -> io::Result<Self> {
        Self::bind(Path::new(&format!(r"\\.\pipe\{name}")))
    }

    pub fn bind(path: &Path) -> io::Result<Self> {
        let mut wide_path: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide_path.is_empty() || wide_path.contains(&0) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid pipe path"));
        }
        wide_path.push(0);

        let descriptor = private_security_descriptor()?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        // SAFETY: the path is NUL-terminated; the descriptor and SECURITY_ATTRIBUTES remain live
        // until CreateNamedPipeW returns. The first-instance flag prevents attaching to an
        // existing pipe with weaker permissions.
        let raw_pipe = unsafe {
            CreateNamedPipeW(
                wide_path.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_REJECT_REMOTE_CLIENTS,
                1,
                64 * 1024,
                64 * 1024,
                0,
                &attributes,
            )
        };
        if raw_pipe == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateNamedPipeW returned a newly owned, valid file handle.
        let pipe = unsafe { File::from_raw_handle(raw_pipe) };
        Ok(Self { path: path.to_path_buf(), pipe: Mutex::new(Some(pipe)) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn accept(&self) -> io::Result<super::Stream> {
        let pipe = self
            .pipe
            .lock()
            .map_err(|_| io::Error::other("pipe listener poisoned"))?
            .take()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "pipe listener already accepted")
            })?;
        // SAFETY: `pipe` owns a live named-pipe server instance. Null OVERLAPPED selects blocking
        // connect; ERROR_PIPE_CONNECTED means a client connected between create and accept.
        if unsafe { ConnectNamedPipe(pipe.as_raw_handle(), ptr::null_mut()) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
                return Err(error);
            }
        }

        let shared = Arc::new(pipe);
        let closer = Arc::clone(&shared);
        Ok(super::Stream {
            reader: Box::new(SharedPipe(Arc::clone(&shared))),
            writer: Box::new(SharedPipe(shared)),
            closer: Some(Box::new(move || {
                // SAFETY: `closer` keeps the same server handle used for reads alive. Cancelling
                // pending I/O then disconnecting wakes a reader during asynchronous shutdown.
                unsafe {
                    CancelIoEx(closer.as_raw_handle(), ptr::null());
                    DisconnectNamedPipe(closer.as_raw_handle());
                }
            })),
        })
    }
}

/// Arc clones retain the same Win32 handle, so shutdown can cancel an in-flight read through it.
struct SharedPipe(Arc<File>);

impl Read for SharedPipe {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut file = self.0.as_ref();
        file.read(buffer)
    }
}

impl Write for SharedPipe {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut file = self.0.as_ref();
        file.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut file = self.0.as_ref();
        file.flush()
    }
}
