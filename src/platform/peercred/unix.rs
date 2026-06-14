//! Unix peer-PID discovery: behavior-preserving lift of
//! `daemon_attach::peer_pid` (`SO_PEERCRED` on Linux, `LOCAL_PEERPID` on
//! macOS). Reads the connected peer's PID via `getsockopt` on the stream's raw
//! fd — zero protocol bytes, works against any daemon version.

use std::os::unix::io::AsRawFd;

use crate::platform::ipc::IpcStream;

/// Linux variant — `getsockopt(SOL_SOCKET, SO_PEERCRED)`.
///
/// The PRD considered `std::os::unix::net::UnixStream::peer_cred()` and
/// rejected it — on the pinned stable toolchain that API is still nightly-only
/// behind the `peer_credentials_unix_socket` feature, so depending on it would
/// not compile.
#[cfg(target_os = "linux")]
pub fn peer_pid(stream: &IpcStream) -> std::io::Result<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` is a freshly-zeroed `libc::ucred` allocated on the
    // stack and outlives the syscall; `len` tracks its size by value.
    // `getsockopt` writes at most `len` bytes into the pointee, which is
    // exactly the layout libc guarantees for `ucred`. The fd comes from
    // `AsRawFd` so it's owned by the caller for the duration of this
    // call. No unwinding can leak resources because there are no Drop
    // types involved.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(cred.pid as u32)
}

/// macOS variant — uses `LOCAL_PEERPID` (not `LOCAL_PEERCRED`, which
/// returns a `struct xucred` without a PID). `nix` does not yet ship a
/// typed wrapper for `LOCAL_PEERPID`, so a small `libc::getsockopt` call
/// is fine; the unsafe surface is one syscall with a stack-allocated
/// output.
#[cfg(target_os = "macos")]
pub fn peer_pid(stream: &IpcStream) -> std::io::Result<u32> {
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: `pid` is a stack-allocated `pid_t` that outlives the call;
    // `len` matches its size by value. `getsockopt(LOCAL_PEERPID)`
    // writes at most `len` bytes into the pointee, which is exactly
    // `sizeof(pid_t)`. The fd is owned by the caller for the duration
    // of this call.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            &mut pid as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(pid as u32)
}
