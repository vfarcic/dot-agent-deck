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

/// Whether an endpoint's *presence* can be observed on the filesystem.
///
/// `true` on Unix: the endpoint is a socket file, so `Path::exists()` is a
/// meaningful (if not authoritative) liveness hint — the daemon does not unlink
/// its socket on exit, but a missing file definitely means no daemon.
///
/// `false` on Windows: a `\\.\pipe\…` name has no filesystem presence at all
/// (`GetFileAttributesW` answers `ERROR_BAD_PATHNAME`), so `exists()` is
/// permanently `false` whether or not a daemon is serving. Any code that treats
/// "path missing" as "daemon gone" must therefore consult this first and fall
/// back to an actual connect attempt — see
/// [`crate::build_version_handshake`]'s `poll_daemon_gone`, where an
/// unconditional `exists()` check would declare *every* live Windows daemon gone
/// on the first poll (PRD #163: "`poll_daemon_gone` path-existence check").
pub const ENDPOINT_IS_FILESYSTEM_PATH: bool = cfg!(unix);
