//! Peer-credential PID discovery on a connected IPC stream (PRD #42 M2).
//!
//! Returns the PID of the process holding the *other* end of a connected
//! [`crate::platform::ipc::IpcStream`]. The daemon-stop path (`daemon_stop.rs`,
//! `build_version_handshake::terminate_daemon_graceful`) calls this from the
//! client side to learn the *server* (daemon) PID before terminating it.
//!
//! Load-bearing property, preserved on both platforms: **zero protocol bytes
//! are exchanged**, so it works against *any* daemon version (the whole point
//! is to drive `daemon stop` against a stale daemon that predates every new
//! protocol surface).
//!
//! - Unix: lifts `daemon_attach::peer_pid` — `getsockopt(SO_PEERCRED)` on
//!   Linux, `getsockopt(LOCAL_PEERPID)` on macOS.
//! - Windows: `GetNamedPipeServerProcessId` on the connected handle (the
//!   client→server direction the consumers need).

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::peer_pid;
#[cfg(windows)]
pub use windows::peer_pid;
