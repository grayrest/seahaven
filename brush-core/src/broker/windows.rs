//! The Windows half of the broker (D24): a named pipe and `DuplicateHandle`.
//!
//! **This inverts the Unix mechanism rather than mirroring it.** `SCM_RIGHTS`
//! puts descriptors in a control message travelling *alongside* the payload;
//! Windows has no such channel, so the parent duplicates each handle **into the
//! child's process** and sends the resulting numeric values *in* the payload.
//! The child therefore reads integers and must not treat them as anything it
//! chose: a handle value it did not receive by duplication is not a handle it
//! can use, and the pipe it read them from is one only its parent could write.
//!
//! **Nothing in this file has been run.** It is developed on macOS and reaches
//! Windows only through `cargo xtask check build`'s cross target, so its
//! correctness rests on review rather than on a passing test.

use std::os::windows::io::{
    AsRawHandle as _, BorrowedHandle, FromRawHandle as _, OwnedHandle, RawHandle,
};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_GENERIC_WRITE, OPEN_EXISTING,
    PIPE_ACCESS_OUTBOUND, ReadFile, WriteFile,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE};

use super::{Decoded, SessionPayload, assemble, decode, encode};

/// Grants the owner and LocalSystem full control, and nobody else anything.
///
/// The named pipe's namespace is machine-wide, so this DACL is what the Unix
/// side gets from a `0700` directory. It matters more here than there for the
/// same reason it matters there: the handshake checks a pid and never a uid.
const OWNER_ONLY_SDDL: &str = "D:P(A;;GA;;;OW)(A;;GA;;;SY)";

/// How long the parent waits for its child to connect, and the child for the
/// parent to serve it. See the Unix module for why it is generous.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The parent's end: a named pipe only this user can open.
pub struct Rendezvous {
    path: PathBuf,
    pipe: OwnedHandle,
}

impl Rendezvous {
    /// Creates the pipe.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the security descriptor or the pipe cannot
    /// be created.
    pub fn create() -> std::io::Result<Self> {
        let name = format!(
            r"\\.\pipe\brush-broker-{}-{}",
            std::process::id(),
            next_id()
        );
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();

        let mut descriptor: *mut std::ffi::c_void = std::ptr::null_mut();
        let sddl: Vec<u16> = OWNER_ONLY_SDDL
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: `sddl` is a NUL-terminated UTF-16 string that outlives the
        // call, and `descriptor` is a valid out-pointer.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &raw mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error("building the pipe's security descriptor"));
        }

        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };

        // SAFETY: `wide` is NUL-terminated and outlives the call; `attributes`
        // points at a descriptor this function just built.
        let raw = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_OUTBOUND,
                PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                // One instance: acceptance is one-shot, and the kernel refusing
                // a second connection is a better guarantee than code that
                // remembers not to accept one.
                1,
                0,
                0,
                0,
                &raw mut attributes,
            )
        };
        // SAFETY: the descriptor was allocated by the conversion call above and
        // is no longer referenced once the pipe exists.
        unsafe { windows_sys::Win32::Foundation::LocalFree(descriptor) };

        if raw == INVALID_HANDLE_VALUE || raw.is_null() {
            return Err(last_error("creating the session pipe"));
        }
        // SAFETY: `CreateNamedPipeW` returned a handle this process owns.
        let pipe = unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) };
        Ok(Self {
            path: PathBuf::from(name),
            pipe,
        })
    }

    /// The name to hand the child in [`RENDEZVOUS_ENV`].
    ///
    /// [`RENDEZVOUS_ENV`]: super::RENDEZVOUS_ENV
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Waits for `pid` to connect, then duplicates the session into it.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if nobody connects, if the connecting process
    /// is not `pid`, or if a handle cannot be duplicated into it.
    pub fn serve(self, pid: u32, payload: SessionPayload) -> std::io::Result<()> {
        let handle = self.pipe.as_raw_handle() as HANDLE;

        // SAFETY: `handle` is the pipe this struct owns.
        let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
        if connected == 0 {
            // ERROR_PIPE_CONNECTED means the client won the race to connect
            // before this call, which is success rather than failure.
            const ERROR_PIPE_CONNECTED: u32 = 535;
            // SAFETY: reading the calling thread's last error.
            if unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
                return Err(last_error("waiting for the child to connect"));
            }
        }

        let mut peer: u32 = 0;
        // SAFETY: `handle` is a connected pipe; `peer` is a valid out-pointer.
        if unsafe { GetNamedPipeClientProcessId(handle, &raw mut peer) } == 0 {
            return Err(last_error("reading the child's process id"));
        }
        if peer != pid {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("session broker: expected a connection from pid {pid}, got {peer}"),
            ));
        }

        let child = ChildProcess::open(pid)?;
        let mut values = Vec::with_capacity(payload.mounts.len());
        for (_, _, mount) in &payload.mounts {
            values.push(child.duplicate_in(mount.as_raw_handle())?);
        }

        let mut bytes = encode(&payload.cwd, &payload.mounts);
        for value in values {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let result = write_all(handle, &bytes);

        // The child holds duplicates now; the parent's originals are dead
        // weight. Dropped after the write because `DuplicateHandle` had to see
        // them live.
        drop(payload);
        result
    }
}

/// A borrowed `PROCESS_DUP_HANDLE` handle on the child, closed on drop.
struct ChildProcess(HANDLE);

impl ChildProcess {
    fn open(pid: u32) -> std::io::Result<Self> {
        // SAFETY: a plain call with no pointer arguments.
        let handle = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, pid) };
        if handle.is_null() {
            return Err(last_error("opening the child for handle duplication"));
        }
        Ok(Self(handle))
    }

    /// Duplicates one handle into the child and returns the value it will see.
    fn duplicate_in(&self, source: RawHandle) -> std::io::Result<u64> {
        let mut target: HANDLE = std::ptr::null_mut();
        // SAFETY: `source` is a live handle owned by this process, `self.0` is
        // a process handle with `PROCESS_DUP_HANDLE`, and `target` is a valid
        // out-pointer.
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                source as HANDLE,
                self.0,
                &raw mut target,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == 0 {
            return Err(last_error("duplicating a mount handle into the child"));
        }
        Ok(target as usize as u64)
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from `OpenProcess` and is closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

/// Duplicates a lent mount handle so the payload can outlive the borrow.
pub fn duplicate(loan: &brush_vfs::MountLoan<'_>) -> std::io::Result<OwnedHandle> {
    // SAFETY: the loan borrows a `Dir` owned by the caller's mount table, which
    // outlives this call, and `try_clone_to_owned` duplicates rather than
    // taking ownership.
    let borrowed = unsafe { BorrowedHandle::borrow_raw(loan.as_raw_handle()) };
    borrowed.try_clone_to_owned()
}

/// The child's end: open the pipe, read the payload, adopt the handle values.
pub fn receive(path: &Path) -> std::io::Result<brush_vfs::Session> {
    let name = path.to_string_lossy();
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();

    let deadline = std::time::Instant::now() + TIMEOUT;
    let pipe = loop {
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if raw != INVALID_HANDLE_VALUE && !raw.is_null() {
            // SAFETY: `CreateFileW` returned a handle this process owns.
            break unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) };
        }
        if std::time::Instant::now() >= deadline {
            return Err(last_error("opening the session pipe"));
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    };

    let bytes = read_all(pipe.as_raw_handle() as HANDLE)?;
    let decoded: Decoded = decode(&bytes)?;

    // The handle values are appended after everything `decode` consumed, eight
    // bytes each, in mount order.
    let count = decoded.mounts.len();
    let tail = bytes
        .len()
        .checked_sub(count * 8)
        .ok_or_else(|| super::malformed("session payload is missing its handle values"))?;
    let mut handles = Vec::with_capacity(count);
    for chunk in bytes[tail..].chunks_exact(8) {
        let value = u64::from_le_bytes(chunk.try_into().unwrap_or([0; 8]));
        if value == 0 {
            return Err(super::malformed("session payload carries a null handle"));
        }
        // SAFETY: the value names a handle the parent duplicated into this
        // process, delivered over a pipe only the parent could write. Ownership
        // transferred with the duplication.
        handles.push(unsafe { OwnedHandle::from_raw_handle(value as usize as RawHandle) });
    }

    assemble(decoded, handles)
}

fn write_all(handle: HANDLE, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let mut written: u32 = 0;
        let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        // SAFETY: `bytes` is a live slice of `len` bytes; `written` is a valid
        // out-pointer.
        let ok = unsafe {
            WriteFile(
                handle,
                bytes.as_ptr(),
                len,
                &raw mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || written == 0 {
            return Err(last_error("sending the session"));
        }
        bytes = &bytes[written as usize..];
    }
    Ok(())
}

fn read_all(handle: HANDLE) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let mut read: u32 = 0;
        // SAFETY: `buf` is a live buffer of its own length; `read` is a valid
        // out-pointer.
        let ok = unsafe {
            ReadFile(
                handle,
                buf.as_mut_ptr(),
                u32::try_from(buf.len()).unwrap_or(u32::MAX),
                &raw mut read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            // A clean end-of-pipe is how the parent signals it is done.
            const ERROR_BROKEN_PIPE: u32 = 109;
            // SAFETY: reading the calling thread's last error.
            if unsafe { GetLastError() } == ERROR_BROKEN_PIPE {
                return Ok(out);
            }
            return Err(last_error("receiving the session"));
        }
        if read == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..read as usize]);
    }
}

fn last_error(doing: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::Error::last_os_error().kind(),
        format!(
            "session broker: {doing}: {}",
            std::io::Error::last_os_error()
        ),
    )
}

/// A counter so one parent can hold several rendezvous at once.
fn next_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
