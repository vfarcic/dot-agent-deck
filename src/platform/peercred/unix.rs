//! Unix peer-credential discovery. [`peer_pid`] is a behavior-preserving lift
//! of `daemon_attach::peer_pid` (`SO_PEERCRED` on Linux, `LOCAL_PEERPID` on
//! macOS); [`peer_uid_raw`] is issue #1121 round two's addition, read the same
//! way and used by [`crate::platform::ipc`]'s connect path. Both read from the
//! kernel's record of the connection on the stream's raw fd — zero protocol
//! bytes, so both work against any daemon version.

use std::os::unix::io::{AsRawFd, RawFd};

use crate::platform::ipc::IpcStream;

/// The connected peer's effective **uid**, on a raw descriptor (issue #1121
/// round two).
///
/// A raw fd rather than an [`IpcStream`] because the caller is
/// [`crate::platform::ipc`]'s connect path, which has to run this on both of
/// its stream types — the async [`IpcStream`] and the synchronous
/// `IpcClient` — before either is handed back.
///
/// **Why a peer credential and not another `stat`.** Everything else that
/// guards an endpoint on Unix reads the *pathname*:
/// [`crate::platform::fsperm::verify_endpoint_trusted`] `lstat`s it, and the
/// connect that follows resolves the same name a second time. The two
/// resolutions are not one operation, and in a world-writable directory an old
/// daemon unlinking its socket during shutdown frees the name for someone else
/// to bind in between. No amount of care with `lstat` closes that, because the
/// property wanted is about the process on the other end and a directory entry
/// cannot state it. `SO_PEERCRED` / `getpeereid` read it from the kernel's
/// record of the connection itself, which is the same thing the Windows
/// transport has always done with the pipe server's owner SID.
///
/// **What it does not establish.** The peer of a connection to an
/// ssh-forwarded socket is the *local* `ssh` process, which is ours by
/// construction — so this says nothing about who listens on the far host. That
/// is a separate gap (remote endpoint discovery has no authentication at all)
/// and is tracked on its own.
#[cfg(target_os = "linux")]
pub fn peer_uid_raw(fd: RawFd) -> std::io::Result<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: identical to `peer_pid` below — `cred` is a freshly-zeroed
    // `libc::ucred` on this stack frame and outlives the syscall, `len` tracks
    // its size by value, and `getsockopt` writes at most `len` bytes into the
    // pointee. The fd is owned by the caller for the duration of the call.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(cred.uid)
}

/// macOS/BSD variant — `getpeereid(2)`.
///
/// Deliberately **not** the `getsockopt(LOCAL_PEERCRED)` shape its `peer_pid`
/// sibling uses. That option answers with a `struct xucred` whose layout this
/// code would have to restate, and the uid is the one field a plain libc
/// function already returns by pointer; `getpeereid` is the smaller unsafe
/// surface for the same answer, and it is the portable BSD spelling rather
/// than an Apple-specific one.
#[cfg(not(target_os = "linux"))]
pub fn peer_uid_raw(fd: RawFd) -> std::io::Result<u32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: both outputs are stack-allocated scalars that outlive the call,
    // and `getpeereid` writes exactly one of each. The fd is owned by the
    // caller for the duration of the call.
    let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(uid)
}

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
