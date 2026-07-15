//! Unix IPC backend: a behavior-preserving lift of the
//! `tokio::net::UnixListener`/`UnixStream` and `std::os::unix::net::UnixStream`
//! usage that previously lived inline in `daemon*`/`hook`/`ui`.
//!
//! Nothing here changes the wire framing, the connection lifecycle, or the
//! stale-socket dance (that orchestration stays in `daemon.rs` /
//! `daemon_attach.rs`). [`IpcListener::bind`] folds in the two socket-coupled
//! permission steps M1 left at the call sites — the umask-before-bind dance
//! ([`crate::platform::fsperm::with_socket_umask`]) and the defense-in-depth
//! 0o600 restate ([`crate::platform::fsperm::set_endpoint_mode_owner_only`]) —
//! so every bound endpoint ends up owner-only exactly as before.

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{UnixListener, UnixStream};

/// Owned read half of an [`IpcStream`]. This is `tokio::net::unix`'s native
/// [`OwnedReadHalf`](tokio::net::unix::OwnedReadHalf) — **not**
/// [`tokio::io::split`]'s generic half — so that dropping the paired
/// [`IpcWriteHalf`] performs a real `SHUT_WR` on the socket (see
/// [`IpcStream::into_split`]). The Windows named-pipe backend keeps the generic
/// `tokio::io::split` halves; callers reference these types only through the
/// `IpcReadHalf` / `IpcWriteHalf` aliases, so the per-backend divergence is
/// invisible above the seam.
pub type IpcReadHalf = tokio::net::unix::OwnedReadHalf;
/// Owned write half of an [`IpcStream`] — see [`IpcReadHalf`]. Its `Drop`
/// half-closes the socket for writing (`shutdown(SHUT_WR)`), which the attach
/// protocol relies on to signal the peer independently of the read half.
pub type IpcWriteHalf = tokio::net::unix::OwnedWriteHalf;

/// Async bidirectional IPC stream. Unix backend: a thin newtype over
/// [`tokio::net::UnixStream`]. `AsyncRead`/`AsyncWrite` delegate to the inner
/// socket so the framing helpers in `daemon_protocol` and `daemon_client`
/// operate on it unchanged.
#[derive(Debug)]
pub struct IpcStream(UnixStream);

impl IpcStream {
    /// Connect to a daemon endpoint. Lift of `UnixStream::connect`; preserves
    /// the exact `io::Error` kinds callers match on (`ConnectionRefused` for a
    /// stale inode, `NotFound` for a missing socket file).
    pub async fn connect(endpoint: &Path) -> io::Result<Self> {
        Ok(Self(UnixStream::connect(endpoint).await?))
    }

    /// Split into owned read/write halves via
    /// [`UnixStream::into_split`](tokio::net::UnixStream::into_split), which
    /// yields `tokio::net::unix`'s native [`OwnedReadHalf`] /
    /// [`OwnedWriteHalf`]. Crucially — unlike [`tokio::io::split`] — dropping
    /// the write half alone performs a `shutdown(SHUT_WR)` on the socket, so
    /// the peer observes a write-side EOF the moment the write half drops, even
    /// while the read half is still live. The attach server
    /// (`daemon_protocol::handle_attach_stream`) moves its write half into a
    /// spawned output task that can end — dropping the write half — *before*
    /// the input loop that owns the read half; that independent SHUT_WR is the
    /// behavior this preserves byte-for-byte from `main` (which split a
    /// `UnixStream` directly). The Windows named-pipe backend keeps
    /// `tokio::io::split` (no per-half half-close primitive there).
    ///
    /// [`OwnedReadHalf`]: tokio::net::unix::OwnedReadHalf
    /// [`OwnedWriteHalf`]: tokio::net::unix::OwnedWriteHalf
    pub fn into_split(self) -> (IpcReadHalf, IpcWriteHalf) {
        self.0.into_split()
    }
}

impl AsyncRead for IpcStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for IpcStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }
}

impl std::os::unix::io::AsRawFd for IpcStream {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.0.as_raw_fd()
    }
}

/// Async IPC listener. Unix backend: a newtype over [`tokio::net::UnixListener`].
pub struct IpcListener(UnixListener);

impl IpcListener {
    /// Bind a listener at `endpoint`, creating the socket inode owner-only.
    ///
    /// Folds the two socket-coupled permission steps M1 left at the call sites:
    /// the umask-before-`bind(2)` dance (creates the inode at 0o600 atomically,
    /// closing the bind→chmod TOCTOU) and a defense-in-depth 0o600 restate.
    /// The caller still owns the stale-socket probe/remove orchestration.
    pub fn bind(endpoint: &Path) -> io::Result<Self> {
        let listener = crate::platform::fsperm::with_socket_umask(|| UnixListener::bind(endpoint))?;
        crate::platform::fsperm::set_endpoint_mode_owner_only(endpoint)?;
        Ok(Self(listener))
    }

    /// Accept the next connection. Lift of `UnixListener::accept`; the peer
    /// address is discarded (callers never used it).
    pub async fn accept(&self) -> io::Result<IpcStream> {
        let (stream, _addr) = self.0.accept().await?;
        Ok(IpcStream(stream))
    }

    /// Test-only: adopt an already-bound [`tokio::net::UnixListener`] as an
    /// `IpcListener` **without** the umask/permission dance [`bind`] performs.
    /// The daemon hook-ingestion tests bind their socket with a plain
    /// `UnixListener::bind` on purpose — [`bind`]'s process-global umask flip
    /// races sibling tests under single-process `cargo test` — yet still need to
    /// hand the listener to `run_hook_loop`, which takes an `IpcListener`.
    #[cfg(test)]
    pub(crate) fn from_tokio_listener(listener: UnixListener) -> Self {
        Self(listener)
    }
}

/// Blocking single-shot IPC client for the sync hook/ui paths. Unix backend:
/// wraps [`std::os::unix::net::UnixStream`]; implements [`std::io::Read`] /
/// [`std::io::Write`] by delegation so callers write/read frames directly.
pub struct IpcClient(std::os::unix::net::UnixStream);

impl IpcClient {
    /// Connect synchronously. Lift of `std::os::unix::net::UnixStream::connect`.
    pub fn connect(endpoint: &Path) -> io::Result<Self> {
        Ok(Self(std::os::unix::net::UnixStream::connect(endpoint)?))
    }

    /// Apply a read+write timeout (used by the ui request/response path so a
    /// wedged daemon can't hang the sync TUI key path). Lift of the paired
    /// `set_read_timeout` / `set_write_timeout` calls.
    pub fn set_timeouts(&self, timeout: std::time::Duration) -> io::Result<()> {
        self.0.set_read_timeout(Some(timeout))?;
        self.0.set_write_timeout(Some(timeout))?;
        Ok(())
    }

    /// Half-close the write side, leaving the read side open. Lift of the
    /// `stream.shutdown(std::net::Shutdown::Write)` call in
    /// `hook::request_from_socket`: after writing its single request line the
    /// client half-closes so the daemon's line reader observes EOF and stops
    /// waiting for more input — the daemon's per-connection task then drops its
    /// write half, which is what lets the client's subsequent `read_to_string`
    /// see EOF instead of blocking forever. Without this the get-seed
    /// request/response deadlocks.
    pub fn shutdown_write(&self) -> io::Result<()> {
        self.0.shutdown(std::net::Shutdown::Write)
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
