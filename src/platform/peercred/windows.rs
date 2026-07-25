//! Windows peer-PID discovery (PRD #163 M3): `GetNamedPipeServerProcessId` /
//! `GetNamedPipeClientProcessId` on the connected pipe handle.
//!
//! Named pipes expose the two directions as two separate calls, so — unlike the
//! symmetric Unix `getsockopt(SO_PEERCRED)` — the backend has to know which end
//! of the pipe it is holding to answer "who is on the *other* side". [`IpcStream`]
//! already tracks that (`Client` from `IpcStream::connect`, `Server` from
//! `IpcListener::accept`), so we dispatch on it:
//!
//! | this end | peer | call |
//! |---|---|---|
//! | `Client` | the daemon (server) | `GetNamedPipeServerProcessId` |
//! | `Server` | the connected client | `GetNamedPipeClientProcessId` |
//!
//! Getting this wrong is silent and dangerous rather than loud: asking for the
//! *server* pid from the server end returns the caller's **own** pid, and the
//! consumer of `peer_pid` is `daemon stop`, which then terminates that pid — i.e.
//! a daemon that answers a stop request would kill itself instead of the process
//! it was asked about. Hence the explicit dispatch, even though today's callers
//! ([`crate::daemon_stop`], [`crate::build_version_handshake`]) only ever hold
//! the client end.
//!
//! Like the Unix `getsockopt` path this exchanges **zero protocol bytes**, so it
//! works against any daemon version — the property that lets `daemon stop` drive
//! a stale daemon that predates every new protocol surface.
//!
//! Written and `cargo check`'d against `x86_64-pc-windows-msvc`; validated at
//! runtime on a Windows runner per PRD #42's testability split.

use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId};

use crate::platform::ipc::IpcStream;

/// Return the PID of the process holding the *other* end of `stream`.
pub fn peer_pid(stream: &IpcStream) -> std::io::Result<u32> {
    let mut pid: u32 = 0;
    let handle = stream.as_raw_handle() as HANDLE;
    // SAFETY (both arms): `handle` is a valid, open named-pipe handle owned by
    // `stream` for the duration of this call; `pid` is a stack-allocated `u32`
    // that outlives the call and is the only thing the API writes to. Each
    // function returns a BOOL (nonzero on success) and retains neither argument.
    let rc = match stream {
        IpcStream::Client(_) => unsafe { GetNamedPipeServerProcessId(handle, &mut pid) },
        IpcStream::Server(_) => unsafe { GetNamedPipeClientProcessId(handle, &mut pid) },
    };
    if rc == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(pid)
}
