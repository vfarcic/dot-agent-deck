//! IPC transport abstraction (PRD #42 M2).
//!
//! Hides the Unix-domain-socket / Windows-named-pipe split behind a single
//! `cfg`-dispatched API so the daemon, attach protocol, and the hook/ui sync
//! clients are transport-agnostic. The Unix backend is a behavior-preserving
//! **lift** of today's `tokio::net::UnixListener`/`UnixStream` and
//! `std::os::unix::net::UnixStream` usage; the Windows backend is the native
//! named-pipe implementation (byte mode, per-instance accept loop).
//!
//! Three types make up the surface:
//!
//! - [`IpcListener`] — `bind(endpoint)` + async `accept() -> IpcStream`.
//!   Replaces the hook listener in `daemon.rs` and the attach listener in
//!   `daemon_protocol.rs`.
//! - [`IpcStream`] — `AsyncRead + AsyncWrite + Unpin + Send`, with async
//!   `connect(endpoint)` and `into_split()`. Callers only ever name the halves
//!   through the [`IpcReadHalf`] / [`IpcWriteHalf`] aliases, so each backend is
//!   free to choose the concrete half type that matches its transport. The
//!   **Unix** backend uses `tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf}`
//!   (via [`tokio::net::UnixStream::into_split`]) so that dropping the write
//!   half alone performs `shutdown(SHUT_WR)` — the native half-close the attach
//!   protocol depends on and that `main` had. (An earlier draft used
//!   [`tokio::io::split`] here, but its generic write half does *not* `SHUT_WR`
//!   on drop, silently regressing the attach half-close on Linux/macOS.) The
//!   **Windows** named-pipe backend has no per-half half-close primitive, so it
//!   keeps [`tokio::io::split`]. `daemon_client.rs`'s protocol helpers were
//!   written against `tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf}` and
//!   stay compatible with both.
//! - [`IpcClient`] — a blocking, single-shot connect handle (`std::io::Read +
//!   Write`) for `hook::send_to_socket` and `ui::send_daemon_request_blocking`.
//!
//! Endpoint resolution lives in [`crate::platform::paths`] (a socket path on
//! Unix, a `\\.\pipe\dot-agent-deck-{user}-{hook|attach}` name on Windows);
//! callers pass the resolved [`std::path::Path`] in and this layer consumes it
//! opaquely.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{IpcClient, IpcListener, IpcReadHalf, IpcStream, IpcWriteHalf};
#[cfg(windows)]
pub use windows::{IpcClient, IpcListener, IpcReadHalf, IpcStream, IpcWriteHalf};
