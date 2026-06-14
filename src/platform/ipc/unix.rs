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

/// Owned read half of an [`IpcStream`]. Per the PRD #42 Trait-shape note this
/// is [`tokio::io::ReadHalf`] over the stream (not `tokio::net::unix`'s owned
/// half) so the type is identical across the Unix and Windows backends.
pub type IpcReadHalf = tokio::io::ReadHalf<IpcStream>;
/// Owned write half of an [`IpcStream`] — see [`IpcReadHalf`].
pub type IpcWriteHalf = tokio::io::WriteHalf<IpcStream>;

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

    /// Split into owned read/write halves via [`tokio::io::split`]. Both halves
    /// keep the underlying socket alive until *both* are dropped, at which
    /// point the socket closes and the peer observes EOF — matching the
    /// disconnect semantics the `daemon_client` subscription/attach paths rely
    /// on.
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
