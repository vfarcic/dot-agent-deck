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
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
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

/// PRD #741 M3: the Unix write half half-closes on drop, so it may be boxed into
/// a [`TransportWriteHalf`](crate::platform::transport::TransportWriteHalf).
///
/// This is answer (1) on [`HalfCloseOnDrop`]: `OwnedWriteHalf::drop` performs
/// `shutdown(SHUT_WR)`, so the peer observes end-of-write the moment this half
/// drops, *while our read half is still live*. That independent EOF is what the
/// attach protocol depends on — `EventSubscription` holds its write half purely
/// to trip the daemon's disconnect detector on drop, and the attach server moves
/// its own write half into an output task that can end before the read loop.
///
/// **There is deliberately no `impl` for [`tokio::io::split`]'s `WriteHalf`
/// here.** Its drop does nothing to the socket, and `platform/ipc/mod.rs`'s
/// module docs record that an earlier draft used it and silently regressed
/// exactly this behaviour. Re-introducing that split is now a compile error at
/// the `TransportWriteHalf::new` call rather than a behaviour change nothing
/// catches.
///
/// [`HalfCloseOnDrop`]: crate::platform::transport::HalfCloseOnDrop
impl crate::platform::transport::HalfCloseOnDrop for IpcWriteHalf {}

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
    ///
    /// Since issue #1121 round two it also refuses a listener owned by another
    /// uid — see [`refuse_foreign_peer`]. That adds one error kind,
    /// `PermissionDenied`, and it is the same kind `connect(2)` itself reports
    /// for a socket we may not talk to.
    pub async fn connect(endpoint: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(endpoint).await?;
        refuse_foreign_peer(stream.as_raw_fd(), endpoint)?;
        Ok(Self(stream))
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
    /// Connect synchronously, with **no** bound on how long the connect itself
    /// may take. Lift of `std::os::unix::net::UnixStream::connect`.
    ///
    /// `connect(2)` on a blocking `AF_UNIX` socket parks indefinitely when the
    /// listener's accept queue is full — it does *not* report `ECONNREFUSED`,
    /// and it does not report `EAGAIN` either (that is what a *non-blocking*
    /// socket gets, which is why only this path is exposed). Any caller that
    /// advertises a deadline must therefore use [`connect_timeout`] instead;
    /// this entry point is for the fire-and-forget senders that advertise none
    /// (`hook::send_to_socket`), where dropping an event on a momentarily-full
    /// queue would be a worse trade than waiting for a slot.
    ///
    /// [`connect_timeout`]: Self::connect_timeout
    pub fn connect(endpoint: &Path) -> io::Result<Self> {
        let stream = std::os::unix::net::UnixStream::connect(endpoint)?;
        refuse_foreign_peer(stream.as_raw_fd(), endpoint)?;
        Ok(Self(stream))
    }

    /// Connect synchronously, bounded by `timeout` (issue #435).
    ///
    /// `std::os::unix::net::UnixStream` has no `connect_timeout` (unlike
    /// `TcpStream`), so this is the manual form: create the socket
    /// non-blocking, then retry `connect(2)` until it succeeds, fails for a
    /// real reason, or the budget runs out — restoring blocking mode before the
    /// client is handed back, so the result is indistinguishable from a
    /// [`connect`](Self::connect) one (blocking reads and writes, bounded by
    /// [`set_timeouts`](Self::set_timeouts)).
    ///
    /// **Retry, not `poll`.** The obvious shape — connect once, `poll(2)` for
    /// writability, read the outcome out of `SO_ERROR` — is the `TcpStream`
    /// recipe and it is silently wrong here. `connect(2)` documents
    /// `EINPROGRESS` as the pollable "started, not finished" state and says in
    /// as many words that "UNIX domain sockets failed with `EAGAIN` instead":
    /// on `AF_UNIX` a queue-full connect starts nothing, so there is no pending
    /// operation for writability to report the completion of. Worse, an
    /// *unconnected* Unix socket already polls writable (the kernel's
    /// `unix_writable` only excludes a listening socket and a full send
    /// buffer), so the `poll` returns instantly, `SO_ERROR` is 0 — nothing
    /// failed, nothing was attempted — and the caller is handed a client that
    /// is not connected to anything. Measured, not reasoned: that version
    /// returned `Ok` against a saturated listener in single-digit milliseconds.
    /// `EINPROGRESS`/`EALREADY` are still folded into the retry so this stays
    /// correct if the endpoint type ever changes.
    ///
    /// Retrying is also the right *behaviour* for the condition, not merely the
    /// available one: a full accept queue is transient, and the Windows backend
    /// already retries its exact analogue (`ERROR_PIPE_BUSY`) against a budget.
    ///
    /// Error kinds are preserved, because everything except "try again" is
    /// reported by `connect(2)` itself and propagated verbatim: `NotFound` for
    /// a missing socket file, `ConnectionRefused` for a stale inode (and, on
    /// the BSDs, for the queue-full case Linux makes you wait through),
    /// `PermissionDenied` for a socket we may not talk to. Blowing the budget
    /// is [`io::ErrorKind::TimedOut`], which no caller had a way to see before.
    pub fn connect_timeout(endpoint: &Path, timeout: std::time::Duration) -> io::Result<Self> {
        let (addr, addr_len) = sockaddr_un(endpoint)?;
        let stream = cloexec_stream_socket()?;
        let fd = stream.as_raw_fd();
        stream.set_nonblocking(true)?;

        let started = std::time::Instant::now();
        let mut backoff = CONNECT_RETRY_INITIAL;
        loop {
            // SAFETY: `addr` is a well-formed `sockaddr_un` living on this stack
            // frame for the whole loop and `addr_len` is its exact length; `fd`
            // is the socket `stream` owns, so it stays open throughout.
            let rc = unsafe { libc::connect(fd, std::ptr::addr_of!(addr).cast(), addr_len) };
            if rc == 0 {
                break;
            }
            let err = io::Error::last_os_error();
            let remaining = timeout.saturating_sub(started.elapsed());
            match err.raw_os_error() {
                // An attempt from an earlier iteration completed underneath us.
                Some(libc::EISCONN) => break,
                // Retry at once rather than sleeping, but still against the
                // deadline: a signal storm must not turn this into the
                // unbounded loop the whole change exists to remove.
                Some(libc::EINTR) if !remaining.is_zero() => continue,
                Some(libc::EAGAIN) | Some(libc::EINPROGRESS) | Some(libc::EALREADY)
                    if !remaining.is_zero() =>
                {
                    std::thread::sleep(backoff.min(remaining));
                    backoff = (backoff * 2).min(CONNECT_RETRY_MAX);
                }
                Some(libc::EINTR)
                | Some(libc::EAGAIN)
                | Some(libc::EINPROGRESS)
                | Some(libc::EALREADY) => return Err(connect_timed_out(endpoint, timeout)),
                _ => return Err(err),
            }
        }

        stream.set_nonblocking(false)?;
        refuse_foreign_peer(fd, endpoint)?;
        Ok(Self(stream))
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

/// Refuse a connection whose peer is not the uid running this process
/// (issue #1121 round two).
///
/// **This is the clause that makes the endpoint trust check mean what it has
/// always claimed.** [`crate::platform::fsperm::verify_endpoint_trusted`]'s
/// doc used to say the descriptor a caller connects with is "anchored to the
/// inode the kernel resolves during this single call". It is not: the `lstat`
/// and the `connect(2)` are two separate pathname resolutions, and every
/// caller that probes and then connects again resolves it a third time. In a
/// sticky world-writable directory a foreign uid cannot replace a *live*
/// victim-owned inode — but when an old daemon unlinks its socket during
/// shutdown the name is free, and a permissive listener bound in that window
/// passes an `lstat` taken before it and receives whatever the caller sends.
///
/// A peer credential is not subject to that, because it is not a name: the
/// kernel records who is on the other end of *this* connection when it is
/// established, and `SO_PEERCRED` / `getpeereid` read that record back. It is
/// welded into all three Unix connect entry points rather than offered as
/// something a caller may ask for, which is exactly the shape the Windows
/// backend has always had — `IpcStream::connect` and `IpcClient::connect`
/// there both compare the pipe server's owner SID with ours, whether or not a
/// caller asks. Until now `src/platform/fsperm/mod.rs`'s own site table said
/// so, and recorded Unix's connect as "unguarded".
///
/// **Two things it does not do.** It says nothing about who listens on the far
/// end of an ssh-forwarded socket, because the peer of that connection is the
/// *local* `ssh` process and is ours by construction. And it is not a
/// substitute for [`crate::platform::fsperm::verify_endpoint_trusted`], which
/// runs *before* a connect is attempted and is what keeps the deck from
/// touching a foreign entry at all; this runs after, on a connection that was
/// allowed to be made.
///
/// Root is outside this, as it is outside every uid check — a process running
/// as uid 0 can bind wherever it likes. It is not the actor this is written
/// against.
fn refuse_foreign_peer(fd: RawFd, endpoint: &Path) -> io::Result<()> {
    let peer_uid = crate::platform::peercred::peer_uid_raw(fd)?;
    let our_uid = crate::platform::paths::current_uid();
    if peer_uid != our_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing the daemon connection at {}: the process listening there runs as \
                 uid {peer_uid} and we are uid {our_uid}. Another user may have bound this \
                 endpoint. Set DOT_AGENT_DECK_SOCKET and DOT_AGENT_DECK_ATTACH_SOCKET to \
                 paths under a directory only you can write.",
                endpoint.display()
            ),
        ));
    }
    Ok(())
}

/// A fresh, unconnected `AF_UNIX` stream socket with close-on-exec set.
///
/// Close-on-exec matters because the deck `exec`s agents constantly (every PTY
/// pane); an fd that survives into an agent's process is a leaked handle on the
/// daemon socket. [`std::os::unix::net::UnixStream::connect`] sets it, so
/// [`IpcClient::connect_timeout`] — which has to build the socket itself in
/// order to connect non-blocking — must too. Linux gets it atomically via
/// `SOCK_CLOEXEC`; elsewhere a follow-up `fcntl` leaves a window only a
/// concurrent `fork`+`exec` between the two calls could exploit, which is what
/// the stdlib does on those platforms as well.
fn cloexec_stream_socket() -> io::Result<std::os::unix::net::UnixStream> {
    #[cfg(target_os = "linux")]
    let socket_type = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let socket_type = libc::SOCK_STREAM;

    // SAFETY: `socket(2)` takes no pointers; the arguments are the constant
    // AF_UNIX/SOCK_STREAM pair. The returned fd is owned by us.
    let fd = unsafe { libc::socket(libc::AF_UNIX, socket_type, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh, valid, exclusively-owned socket fd, and the raw
    // handle is not used to close it anywhere — `stream` owns it from here and
    // closes it exactly once, including on the error path below.
    let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };

    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: `fd` is the socket `stream` owns; `F_SETFD` takes an int
        // argument, no pointers. `FD_CLOEXEC` is the only descriptor flag, so
        // setting it outright cannot clear another.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(stream)
}

/// Gap before the FIRST `connect(2)` retry in [`IpcClient::connect_timeout`].
/// Short enough that a listener draining its queue costs the caller nothing
/// measurable.
const CONNECT_RETRY_INITIAL: std::time::Duration = std::time::Duration::from_millis(1);
/// Ceiling the gap doubles up to. Keeps a full 5s budget down to a few hundred
/// cheap syscalls rather than a spin, while staying far below any budget a
/// caller states (the shortest is the 250ms hint path in `ui`).
const CONNECT_RETRY_MAX: std::time::Duration = std::time::Duration::from_millis(20);

fn connect_timed_out(endpoint: &Path, timeout: std::time::Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "connecting to {} timed out after {timeout:?}",
            endpoint.display()
        ),
    )
}

/// Encode `endpoint` as a filesystem `AF_UNIX` address, returning the address
/// and the exact `socklen_t` to pass to `connect(2)`/`bind(2)`.
///
/// Mirrors what `std::os::unix::net::UnixStream::connect` does internally (the
/// stdlib keeps its version private): reject an over-long path or one carrying
/// an interior NUL up front with `InvalidInput`, then copy the bytes into a
/// zeroed `sockaddr_un` and size the address to `offsetof(sun_path) + len + 1`
/// rather than `size_of::<sockaddr_un>()`, so the trailing NUL is included and
/// nothing beyond it is.
fn sockaddr_un(endpoint: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    use std::os::unix::ffi::OsStrExt;

    // SAFETY: `sockaddr_un` is a plain C struct of scalars and a byte array,
    // for which all-zero is a valid (and the conventional) initial state.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;

    let bytes = endpoint.as_os_str().as_bytes();
    if bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path contains an interior NUL byte",
        ));
    }
    // `<` and not `<=`: `sun_path` must still hold the terminating NUL.
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path is longer than the platform's sun_path limit",
        ));
    }
    // SAFETY: `bytes.len()` is strictly less than `sun_path`'s length (checked
    // above), the two regions cannot overlap (`addr` is a fresh stack local),
    // and `sun_path` is a byte array whose element type is `c_char` — written
    // through a `u8` pointer to avoid a same-width cast that flips signedness
    // between targets.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            addr.sun_path.as_mut_ptr().cast::<u8>(),
            bytes.len(),
        );
    }

    let len = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    Ok((addr, len as libc::socklen_t))
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;

    /// How long the reproduction waits for a connect to come back before
    /// declaring it hung. Comfortably above [`CONNECT_BUDGET`] so a slow CI box
    /// cannot make a *bounded* connect look like a hung one.
    const WATCHDOG: Duration = Duration::from_secs(5);
    /// The deadline the connect under test advertises.
    const CONNECT_BUDGET: Duration = Duration::from_millis(250);

    /// How a platform answers a `connect(2)` to a listener whose accept queue
    /// is full — the one place the two Unixes genuinely differ, so the test
    /// asserts against what the platform actually did rather than guessing.
    #[derive(Debug, PartialEq, Eq)]
    enum QueueFull {
        /// Linux: a *blocking* `connect(2)` parks indefinitely. This is the
        /// hang issue #435 is about.
        Parks,
        /// The BSDs (macOS included): `sonewconn` refuses over-limit
        /// connections outright, so connect fails fast with `ECONNREFUSED` and
        /// was never able to hang. The client still has to come back inside its
        /// budget, which is what the test asserts either way.
        Refuses,
    }

    /// A listener whose accept queue is full and that never calls `accept(2)`.
    ///
    /// This is the shape issue #435 is about: on Linux a *blocking* `connect(2)`
    /// to such an endpoint parks indefinitely — it does not get
    /// `ECONNREFUSED`, and it does not get `EAGAIN` either (that is what a
    /// *non-blocking* socket gets, which is precisely why only the blocking
    /// path is affected).
    ///
    /// The backlog is set to 1 via a raw `listen(2)` because
    /// `UnixListener::bind` hardcodes 128, and queuing 129 connections to reach
    /// the same state is slower and no more faithful.
    struct SaturatedListener {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
        listener: UnixListener,
        /// Connections already sitting in the accept queue. Held so they are
        /// not closed, and so the queue stays full for the whole test.
        _queued: Vec<UnixStream>,
        /// The receivers of filling connects that were slow to answer but
        /// turned out not to be parked (issue #1137). Held so a connect that
        /// completes late still has somewhere to deliver its stream, instead of
        /// the send failing and the stream closing — which would drop a queued
        /// connection and could leave the queue short of full.
        _late: Vec<mpsc::Receiver<io::Result<UnixStream>>>,
        behaviour: QueueFull,
    }

    /// One non-blocking `connect(2)` to `path`, the probe
    /// [`SaturatedListener::new`] uses to tell a full queue from a slow thread.
    ///
    /// A non-blocking socket never parks: against a full queue Linux answers
    /// `EAGAIN` (`WouldBlock`) and the BSDs `ECONNREFUSED`, and against a queue
    /// with room the connect completes on the spot, because an `AF_UNIX`
    /// connect has no handshake to wait for.
    fn nonblocking_connect(path: &std::path::Path) -> io::Result<UnixStream> {
        let stream = cloexec_stream_socket()?;
        stream.set_nonblocking(true)?;
        let (addr, len) = sockaddr_un(path)?;
        // SAFETY: `addr`/`len` describe a well-formed `sockaddr_un` that
        // outlives the call, and the fd is the socket `stream` owns.
        let rc = unsafe { libc::connect(stream.as_raw_fd(), std::ptr::addr_of!(addr).cast(), len) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(stream)
    }

    impl SaturatedListener {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("saturated.sock");

            // SAFETY: a plain `socket(2)` with the constant AF_UNIX/SOCK_STREAM
            // pair; the returned fd is handed to `UnixListener::from_raw_fd`
            // below, which takes sole ownership and closes it exactly once.
            let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
            assert!(fd >= 0, "socket: {}", io::Error::last_os_error());
            // SAFETY: `fd` is a fresh, valid, exclusively-owned socket fd that
            // is not used through the raw handle again after this point.
            let listener = unsafe { UnixListener::from_raw_fd(fd) };

            let (addr, len) = sockaddr_un(&path).expect("encode the socket path");
            // SAFETY: `addr`/`len` describe a well-formed `sockaddr_un` that
            // outlives the call, and `fd` is our own listening socket.
            let rc = unsafe { libc::bind(fd, std::ptr::addr_of!(addr).cast(), len) };
            assert_eq!(rc, 0, "bind: {}", io::Error::last_os_error());
            // SAFETY: `fd` is our own bound socket; `listen(2)` has no
            // pointer arguments.
            let rc = unsafe { libc::listen(fd, 1) };
            assert_eq!(rc, 0, "listen: {}", io::Error::last_os_error());

            // Fill the queue. Exactly how many connections fit is a kernel
            // detail (Linux admits backlog + 1), so keep connecting until the
            // queue stops taking them rather than assuming a count — and let
            // the platform tell us *how* it stops. Each filling connect runs on
            // its own thread because on Linux the one that finds the queue full
            // parks: that parked connect is the bug under test, and it stays
            // parked until the process exits, which is harmless.
            //
            // Silence alone is NOT read as "full" (issue #1137): a filling
            // thread starved for longer than the wait against a queue that
            // still had room would otherwise be classified `Parks`, leaving a
            // listener that is not saturated and a connect under test that
            // simply succeeds. So a timeout is confirmed with a non-blocking
            // probe, which cannot park, and filling resumes if the probe gets
            // in.
            let mut queued = Vec::new();
            let mut late = Vec::new();
            for _ in 0..64 {
                let probe = path.clone();
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(UnixStream::connect(&probe));
                });
                let behaviour = match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(Ok(stream)) => {
                        queued.push(stream);
                        continue;
                    }
                    Ok(Err(_)) => QueueFull::Refuses,
                    Err(_) => match nonblocking_connect(&path) {
                        Ok(stream) => {
                            // The queue had room, so the silence was a slow
                            // thread rather than a parked connect.
                            queued.push(stream);
                            late.push(rx);
                            continue;
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => QueueFull::Parks,
                        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                            QueueFull::Refuses
                        }
                        Err(e) => panic!("probe connect to the filling listener: {e}"),
                    },
                };
                return Self {
                    _dir: dir,
                    path,
                    listener,
                    _queued: queued,
                    _late: late,
                    behaviour,
                };
            }
            panic!("could not saturate the listener's accept queue");
        }
    }

    /// Run [`IpcClient::connect_timeout`] on its own thread and report how long
    /// it took, or `None` if it never came back within [`WATCHDOG`].
    ///
    /// The connect runs on a thread of its own precisely because the defect
    /// under test is an *uninterruptible* block: calling it inline would hang
    /// the test process instead of failing it, which is not a reproduction.
    fn timed_connect(path: &std::path::Path) -> Option<(Duration, io::Result<IpcClient>)> {
        let path = path.to_path_buf();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let started = Instant::now();
            let result = IpcClient::connect_timeout(&path, CONNECT_BUDGET);
            let _ = tx.send((started.elapsed(), result));
        });
        rx.recv_timeout(WATCHDOG).ok()
    }

    /// Issue #435: the synchronous client's connect must honour the caller's
    /// deadline. Against a listener whose backlog is full and that never
    /// accepts, a blocking `connect(2)` parks forever, so the deadline the two
    /// sync IPC callers advertise never covers the connect step at all.
    #[test]
    fn connect_against_a_saturated_listener_returns_within_the_deadline() {
        let listener = SaturatedListener::new();

        let (elapsed, result) = timed_connect(&listener.path)
            .expect("connect never returned — it is still parked in connect(2)");

        let Err(err) = result else {
            panic!("a saturated listener must not hand back a connected client");
        };
        let expected = match listener.behaviour {
            QueueFull::Parks => io::ErrorKind::TimedOut,
            QueueFull::Refuses => io::ErrorKind::ConnectionRefused,
        };
        assert_eq!(
            err.kind(),
            expected,
            "a {:?} platform must report {expected:?}, got {err:?}",
            listener.behaviour
        );
        // Generous multiple of the budget: the assertion is "bounded, not
        // parked", and a loaded CI box may schedule the thread late.
        assert!(
            elapsed < CONNECT_BUDGET * 8,
            "connect must return within its budget, took {elapsed:?}"
        );
    }

    /// The retry loop must be able to *succeed*, not merely to give up on time.
    /// Once the listener drains its queue, a connect that was refused a
    /// millisecond earlier has to complete inside the budget — otherwise a
    /// `connect_timeout` that could never connect at all would still satisfy the
    /// saturated-listener test above, and the deck would simply stop talking to
    /// a busy daemon instead of hanging on one.
    #[test]
    fn connect_retries_until_the_listener_drains_its_queue() {
        let listener = SaturatedListener::new();
        if listener.behaviour == QueueFull::Refuses {
            // There is no retry to observe on the BSDs: the kernel answers a
            // full queue with `ECONNREFUSED`, which is a FINAL answer that
            // `connect_timeout` must propagate rather than retry — a stale
            // socket inode reports the same errno, and the daemon's
            // stale-endpoint probe depends on seeing it immediately. What
            // replaces this test there is asserted directly by
            // `connect_against_a_saturated_listener_returns_within_the_deadline`
            // (queue-full is `ConnectionRefused`) and by
            // `connect_does_not_retry_a_stale_socket_inode` (that errno is not
            // retried anywhere).
            eprintln!(
                "skipped: this platform refuses a full accept queue rather than parking on it, \
                 so connect_timeout's retry loop is unreachable here"
            );
            return;
        }
        // Non-blocking so the drain loop below can poll for a queued connection
        // without parking once the queue is empty.
        listener
            .listener
            .set_nonblocking(true)
            .expect("make the listener non-blocking");

        let path = listener.path.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let started = Instant::now();
            let result = IpcClient::connect_timeout(&path, Duration::from_secs(5));
            let _ = tx.send((started.elapsed(), result));
        });

        // Drain one connection at a time until the client gets in. Accepting
        // once is not enough: the filling connect that `SaturatedListener`
        // left parked is queued as well and may take the first freed slot.
        let mut drained = Vec::new();
        let mut outcome = None;
        for _ in 0..64 {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(v) => {
                    outcome = Some(v);
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Ok((stream, _)) = listener.listener.accept() {
                        drained.push(stream);
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        let (elapsed, result) =
            outcome.expect("the connect never returned while the queue drained");
        if let Err(err) = result {
            panic!("a connect must succeed once the queue drains, got {err:?} after {elapsed:?}");
        }
        assert!(
            elapsed < Duration::from_secs(5),
            "the successful connect must land inside the budget, took {elapsed:?}"
        );
    }

    /// Issue #1137: the probe that confirms a saturated queue must tell the two
    /// states apart — it gets in while the queue has room, and it reports the
    /// platform's queue-full answer once it has none. If it could not, the
    /// fill loop would either stop early (the bug) or never stop.
    #[test]
    fn the_saturation_probe_tells_a_full_queue_from_one_with_room() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("roomy.sock");
        let _roomy = UnixListener::bind(&path).expect("bind a listener with room");
        nonblocking_connect(&path).expect("a queue with room must admit the probe");

        let saturated = SaturatedListener::new();
        let err = nonblocking_connect(&saturated.path)
            .expect_err("a full queue must not admit the probe");
        let expected = match saturated.behaviour {
            QueueFull::Parks => io::ErrorKind::WouldBlock,
            QueueFull::Refuses => io::ErrorKind::ConnectionRefused,
        };
        assert_eq!(err.kind(), expected, "{err:?}");
    }

    /// The retry loop must not swallow a FINAL answer. A socket inode with no
    /// listener behind it — what a crashed daemon leaves — reports
    /// `ECONNREFUSED`, the same errno the BSDs use for a full accept queue, and
    /// `daemon.rs`'s stale-endpoint dance keys off seeing it *promptly*: it is
    /// the probe that decides whether to unlink the leftover and rebind. Adding
    /// `ECONNREFUSED` to the retryable set — the obvious way to "fix" a BSD
    /// running the test above — would leave that classification intact while
    /// making every stale-socket probe cost the caller's whole budget, so this
    /// pins the timing as well as the kind.
    #[test]
    fn connect_does_not_retry_a_stale_socket_inode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("stale.sock");
        // Bind and drop: the stdlib does not unlink on drop, so the inode
        // outlives the listener exactly as a crashed daemon's does.
        drop(UnixListener::bind(&path).expect("bind"));
        assert!(path.exists(), "the stale inode must survive the listener");

        let started = Instant::now();
        let Err(err) = IpcClient::connect_timeout(&path, Duration::from_secs(5)) else {
            panic!("a socket with no listener must not connect");
        };
        assert_eq!(
            err.kind(),
            io::ErrorKind::ConnectionRefused,
            "the stale-inode classification the daemon probes for must survive, got {err:?}"
        );
        assert!(
            started.elapsed() < CONNECT_BUDGET,
            "a final answer must be returned at once, not retried to the deadline — took {:?}",
            started.elapsed()
        );
    }

    /// The control for the test above: the same client, the same code path, a
    /// listener that is merely *not* accepting yet rather than saturated. This
    /// must still connect, and promptly — otherwise the failure above would
    /// only show that the deck cannot talk to an un-accepted socket at all.
    #[test]
    fn connect_against_a_listener_with_room_still_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("roomy.sock");
        let _listener = UnixListener::bind(&path).expect("bind");

        let (elapsed, result) = timed_connect(&path).expect("connect never returned");
        if let Err(err) = result {
            panic!("connecting to a listener with a free backlog slot must succeed: {err}");
        }
        assert!(
            elapsed < CONNECT_BUDGET,
            "a healthy connect must be fast, took {elapsed:?}"
        );
    }
}
