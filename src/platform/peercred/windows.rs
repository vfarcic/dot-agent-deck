//! Windows peer-PID discovery: `GetNamedPipeServerProcessId` on the connected
//! pipe handle. This is the client→server direction the consumers need
//! (`daemon stop` learns the daemon's PID from its client end). Like the Unix
//! `getsockopt` path it exchanges zero protocol bytes and works against any
//! daemon version.
//!
//! Written and `cargo check`'d against `x86_64-pc-windows-msvc`; validated at
//! runtime on a Windows runner per PRD #42's testability split.

use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;

use crate::platform::ipc::IpcStream;

/// Return the daemon (server) PID behind a connected client pipe.
pub fn peer_pid(stream: &IpcStream) -> std::io::Result<u32> {
    let mut pid: u32 = 0;
    let handle = stream.as_raw_handle() as HANDLE;
    // SAFETY: `handle` is a valid, open named-pipe handle owned by `stream`
    // for the duration of this call; `pid` is a stack-allocated `u32` that
    // outlives the call and is the only thing the API writes to. The function
    // returns a BOOL (nonzero on success) and does not retain either argument.
    let rc = unsafe { GetNamedPipeServerProcessId(handle, &mut pid) };
    if rc == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(pid)
}
