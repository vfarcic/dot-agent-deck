//! Windows IPC backend: native named pipes (PRD #42 M2).
//!
//! Mirrors the Unix backend's public surface ([`IpcListener`], [`IpcStream`],
//! [`IpcClient`], [`IpcReadHalf`], [`IpcWriteHalf`]) so the daemon / attach
//! protocol / sync clients compile unchanged. Named pipes run in **byte mode**
//! ([`PipeMode::Byte`]) so the existing length-prefixed / newline framing is
//! transport-agnostic.
//!
//! Connection model. Unlike a Unix socket — where `accept()` yields a ready
//! stream — a named-pipe server pre-creates an instance and `connect().await`s
//! a client onto it. [`IpcListener::bind`] creates the first instance (with
//! `first_pipe_instance(true)`, which is the singleton-daemon guard: a second
//! daemon's `bind` fails because the name is taken). [`IpcListener::accept`]
//! awaits a client on the pending instance, then *immediately* creates the next
//! instance so there is always one pending — replicating the always-listening
//! invariant a Unix `accept` loop has for free. Named pipes have no on-disk
//! inode, so there is no stale-socket probe/remove/rebind here.
//!
//! This backend is written and `cargo check`'d on Linux against the
//! `x86_64-pc-windows-msvc` target; its *runtime* behavior (accept/serve,
//! `ERROR_PIPE_BUSY` retry, byte-mode framing) is validated on a Windows
//! runner per PRD #42's testability split.

use std::io;
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::path::Path;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions,
};
use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;

/// Owned read half of an [`IpcStream`] — [`tokio::io::ReadHalf`] over the
/// stream, identical in shape to the Unix backend's alias.
pub type IpcReadHalf = tokio::io::ReadHalf<IpcStream>;
/// Owned write half of an [`IpcStream`] — see [`IpcReadHalf`].
pub type IpcWriteHalf = tokio::io::WriteHalf<IpcStream>;

/// Total budget for the `ERROR_PIPE_BUSY` connect retry. The busy error is the
/// named-pipe analogue of a momentarily-unaccepted socket: all server
/// instances are connected and the next one hasn't been created yet. We retry
/// briefly rather than fail the first connect that races the server's
/// instance-recreation.
const CONNECT_RETRY_BUDGET: Duration = Duration::from_secs(2);
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// Async bidirectional IPC stream. Windows backend: either the client end
/// ([`NamedPipeClient`], from [`IpcStream::connect`]) or the server end
/// ([`NamedPipeServer`], from [`IpcListener::accept`]). `AsyncRead`/`AsyncWrite`
/// and `AsRawHandle` delegate to whichever end this is.
#[derive(Debug)]
pub enum IpcStream {
    Client(NamedPipeClient),
    Server(NamedPipeServer),
}

impl IpcStream {
    /// Connect to a daemon endpoint (`\\.\pipe\…`). Retries briefly on
    /// `ERROR_PIPE_BUSY`; any other error (notably `ERROR_FILE_NOT_FOUND`,
    /// surfaced as `io::ErrorKind::NotFound` — "no daemon") propagates so the
    /// daemon-stop path can fold it into "no daemon running".
    pub async fn connect(endpoint: &Path) -> io::Result<Self> {
        let name = endpoint.as_os_str();
        let retry_budget = CONNECT_RETRY_BUDGET;
        let mut waited = Duration::ZERO;
        loop {
            match ClientOptions::new().open(name) {
                Ok(client) => return Ok(Self::Client(client)),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                    if waited >= retry_budget {
                        return Err(e);
                    }
                    tokio::time::sleep(CONNECT_RETRY_INTERVAL).await;
                    waited += CONNECT_RETRY_INTERVAL;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Split into owned read/write halves via [`tokio::io::split`] — see the
    /// Unix backend for the disconnect-on-double-drop semantics this preserves.
    pub fn into_split(self) -> (IpcReadHalf, IpcWriteHalf) {
        tokio::io::split(self)
    }
}

impl AsyncRead for IpcStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            IpcStream::Client(c) => Pin::new(c).poll_read(cx, buf),
            IpcStream::Server(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for IpcStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            IpcStream::Client(c) => Pin::new(c).poll_write(cx, buf),
            IpcStream::Server(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            IpcStream::Client(c) => Pin::new(c).poll_flush(cx),
            IpcStream::Server(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            IpcStream::Client(c) => Pin::new(c).poll_shutdown(cx),
            IpcStream::Server(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

impl AsRawHandle for IpcStream {
    fn as_raw_handle(&self) -> RawHandle {
        match self {
            IpcStream::Client(c) => c.as_raw_handle(),
            IpcStream::Server(s) => s.as_raw_handle(),
        }
    }
}

/// Async IPC listener. Windows backend: holds the pipe name plus the single
/// pre-created pending server instance.
pub struct IpcListener {
    name: std::ffi::OsString,
    pending: Mutex<Option<NamedPipeServer>>,
}

impl IpcListener {
    /// SECURITY GATE (PRD #42 foundation / #163): refuse to ever create the
    /// Windows daemon listener.
    ///
    /// The named-pipe instance below would be created with the **default**
    /// security descriptor, whose DACL grants `Everyone` read access, and
    /// [`crate::platform::peercred`]'s `verify_endpoint_trusted` is a no-op on
    /// Windows. With the predictable endpoint name from
    /// [`crate::platform::paths`], a foreign local user could pipe-squat the
    /// daemon: read agent terminal output / hook payloads or impersonate the
    /// daemon to capture forwarded keystrokes. Unix defends this with the
    /// owner-only socket mode + owner/mode verify; the hardened Windows
    /// equivalent (a current-user-SID DACL via `ServerOptions::security_attributes`
    /// plus client-SID verification) lands in **PRD #163**.
    ///
    /// Until then the foundation must guarantee that **no insecure listener is
    /// ever created on Windows**, so `bind` returns [`io::ErrorKind::Unsupported`]
    /// *before* any `ServerOptions::create` call. The daemon-server entry points
    /// ([`crate::daemon::run_daemon_with`] hook listener,
    /// [`crate::daemon_protocol::bind_attach_listener`] attach listener) surface
    /// this as a clean "not yet supported" error. The Windows *client* side
    /// (`IpcStream::connect` / `IpcClient`) is left intact; the vulnerability is
    /// the listening pipe, not connecting to one.
    pub fn bind(_endpoint: &Path) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the dot-agent-deck daemon is not yet supported on native Windows — tracked in PRD #163",
        ))
    }

    /// Await a client on the pending instance, then immediately create the next
    /// instance so one is always listening. Returns the now-connected server
    /// end as an [`IpcStream`].
    ///
    /// Single-sequential-caller assumption: unlike the Unix
    /// [`tokio::net::UnixListener::accept`] (which takes `&self` and is safe to
    /// poll from several tasks at once), this `accept` `take()`s the one pending
    /// pipe instance out of the `Mutex` before awaiting — so a *second*
    /// concurrent `accept` would find `None` and error. The daemon's serve loop
    /// calls `accept` sequentially (one accept, spawn handler, loop), so this
    /// holds; do not call it from multiple tasks concurrently. (Moot while the
    /// `bind` security gate above is in force, but kept correct for #163.)
    pub async fn accept(&self) -> io::Result<IpcStream> {
        let server = self
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .ok_or_else(|| io::Error::other("IPC listener has no pending pipe instance"))?;

        server.connect().await?;

        // Keep one instance pending so there is no window where a client sees
        // ERROR_FILE_NOT_FOUND between connections.
        let next = ServerOptions::new()
            .pipe_mode(PipeMode::Byte)
            .create(&self.name)?;
        *self.pending.lock().unwrap_or_else(|p| p.into_inner()) = Some(next);

        Ok(IpcStream::Server(server))
    }
}

/// Blocking single-shot IPC client for the sync hook/ui paths. Windows backend:
/// opens the named pipe as a file (`\\.\pipe\…`), sufficient for the
/// newline-JSON hook write and the ui request/response over the same handle.
pub struct IpcClient(std::fs::File);

impl IpcClient {
    /// Connect synchronously by opening the pipe for read+write. Retries
    /// briefly on `ERROR_PIPE_BUSY` (mirrors the async client).
    pub fn connect(endpoint: &Path) -> io::Result<Self> {
        let mut waited = Duration::ZERO;
        loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(endpoint)
            {
                Ok(file) => return Ok(Self(file)),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                    if waited >= CONNECT_RETRY_BUDGET {
                        return Err(e);
                    }
                    std::thread::sleep(CONNECT_RETRY_INTERVAL);
                    waited += CONNECT_RETRY_INTERVAL;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Read+write timeout. A file handle to a named pipe does not honor
    /// socket-style timeouts; the real wait-bounded client lands in PRD #163.
    /// Kept as a signature-stable no-op so the ui path compiles unchanged.
    pub fn set_timeouts(&self, _timeout: std::time::Duration) -> io::Result<()> {
        Ok(())
    }

    /// Half-close the write side (Unix `shutdown(Shutdown::Write)`). A named
    /// pipe opened as a `std::fs::File` has no half-close primitive; the real
    /// message-mode client (which does not need one) lands in PRD #163. Kept as
    /// a signature-stable no-op so `hook::request_from_socket` compiles
    /// unchanged. Unreachable in #42: the Windows daemon hard-fails at
    /// `IpcListener::bind`, so no daemon ever answers the hook socket here.
    pub fn shutdown_write(&self) -> io::Result<()> {
        Ok(())
    }
}

impl std::io::Read for IpcClient {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl std::io::Write for IpcClient {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
