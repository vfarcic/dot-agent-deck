//! Windows IPC backend: native named pipes (PRD #42 M2; secured in #163 M4).
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
//! inode, so there is no stale-socket probe/remove/rebind here (see
//! [`super::ENDPOINT_IS_FILESYSTEM_PATH`], which is what short-circuits that
//! dance in the callers).
//!
//! ## Security (PRD #163 M4 — the release [BLOCKER])
//!
//! #42 shipped this backend with the *default* pipe security descriptor (whose
//! DACL grants `Everyone` read access) and no client-side trust check. Combined
//! with the predictable per-user pipe name that is a pipe-squat: a foreign local
//! user could read agent terminal output and hook payloads, or impersonate the
//! daemon and capture forwarded keystrokes. Both halves are closed here:
//!
//! - **Server.** *Every* pipe instance — the first one from [`IpcListener::bind`]
//!   and every replacement created in [`IpcListener::accept`] — is created with
//!   [`crate::platform::fsperm::pipe_security_descriptor`]: owner = the current
//!   user, protected DACL granting the current user and nobody else. Missing the
//!   accept-path instances would have left every connection after the first one
//!   wide open, so the listener carries what it needs to rebuild the descriptor
//!   per instance.
//! - **Client.** Both entry points — [`IpcStream::connect`] (ui/attach, async)
//!   and [`IpcClient::connect`] (the sync hook + TUI request paths) — verify the
//!   pipe server's owner SID against ours *before returning a usable object*, so
//!   no caller can write a byte first. The hook client matters as much as the
//!   attach client: it is not daemon-gated, so on Windows it will happily connect
//!   to whatever holds the predictable per-user pipe name.
//!
//! Everything fails closed: an unreadable SID, an unbuildable descriptor, or an
//! unverifiable server is an error, never a fallback to the permissive default.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::pin::Pin;
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions,
};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_IO_PENDING, ERROR_PIPE_BUSY,
    GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, ReadFile, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT, WriteFile,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED,
};
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, ResetEvent};

use crate::platform::fsperm::{pipe_security_descriptor, verify_object_owner_is_current_user};

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

/// Default read/write deadline for the **synchronous** [`IpcClient`], applied at
/// connect time (PRD #163 M4, "sync IPC client timeouts").
///
/// Unix has no such default and does not need one: `IpcClient::shutdown_write`
/// there is a real `shutdown(SHUT_WR)`, so a request/response caller can always
/// drive its peer to EOF and every blocking read terminates. A named pipe has no
/// half-close, so an unbounded read against a wedged — or merely older,
/// non-answering — daemon would hang the caller forever. Callers that care state
/// their own budget (`ui::send_daemon_request_blocking` sets 5s explicitly, the
/// same value); this is the floor that keeps the ones that don't from wedging.
const SYNC_CLIENT_DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

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
    ///
    /// **PRD #163 M4:** before the stream is handed back, the pipe server's owner
    /// SID is checked against ours. A foreign-owned pipe at our endpoint name is
    /// refused with [`io::ErrorKind::PermissionDenied`] and the handle dropped, so
    /// no caller can write attach frames — or read a forged reply — from a
    /// squatted pipe.
    ///
    /// The impersonation half of that hardening is stated **explicitly** via
    /// [`SECURITY_IDENTIFICATION`]`| `[`SECURITY_SQOS_PRESENT`] (PRD #163 auditor
    /// low item): a pipe server can impersonate a client that connects at
    /// `SECURITY_IMPERSONATION` or above, and the window before the owner-SID check
    /// is exactly when a squatting server would try. `tokio`'s `ClientOptions`
    /// happens to default to the same pair, but relying on a library's internal
    /// default for a security property is how such properties get lost in a
    /// dependency bump — so it is spelled out here, matching the sync
    /// [`IpcClient::connect`] flags one-for-one.
    pub async fn connect(endpoint: &Path) -> io::Result<Self> {
        let name = endpoint.as_os_str();
        let retry_budget = CONNECT_RETRY_BUDGET;
        let mut waited = Duration::ZERO;
        loop {
            match ClientOptions::new()
                .security_qos_flags(SECURITY_IDENTIFICATION | SECURITY_SQOS_PRESENT)
                .open(name)
            {
                Ok(client) => {
                    verify_server_owner(client.as_raw_handle(), endpoint)?;
                    return Ok(Self::Client(client));
                }
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

    /// Split into owned read/write halves via [`tokio::io::split`]. A named
    /// pipe has no per-half half-close primitive (the Unix `shutdown(SHUT_WR)`
    /// the Unix backend gets from `UnixStream::into_split` has no analogue
    /// here), so both halves keep the pipe alive until *both* drop, at which
    /// point the peer observes EOF.
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
    /// Create the first instance of the daemon's named pipe, with an explicit
    /// owner-only security descriptor (PRD #163 M4).
    ///
    /// `first_pipe_instance(true)` is the singleton-daemon guard: if the name is
    /// already taken — by our own second daemon *or* by a foreign user who got
    /// there first — creation fails with `ERROR_ACCESS_DENIED`, which is mapped to
    /// [`io::ErrorKind::AddrInUse`] so callers see the same "refusing to clobber a
    /// live endpoint" shape the Unix probe produces. Failing to bind is the
    /// fail-closed outcome in the squat case: we never serve on a name we do not
    /// own.
    ///
    /// This replaces #42's `Unsupported` hard-fail, which existed only because the
    /// listener would otherwise have been created with the default
    /// (`Everyone`-readable) descriptor and no client-side verification. Both
    /// preconditions the PRD attached to lifting that gate now hold.
    pub fn bind(endpoint: &Path) -> io::Result<Self> {
        let name = endpoint.as_os_str().to_os_string();
        let first = create_secured_instance(&name, true)?;
        Ok(Self {
            name,
            pending: Mutex::new(Some(first)),
        })
    }

    /// Await a client on the pending instance, then immediately create the next
    /// instance so one is always listening. Returns the now-connected server
    /// end as an [`IpcStream`].
    ///
    /// The replacement instance is created through the same
    /// [`create_secured_instance`] the bind used, so it carries the identical
    /// owner-only descriptor. Each pipe instance has its own security descriptor,
    /// so creating replacements with the default one would have re-opened the
    /// [BLOCKER] for every connection after the first.
    ///
    /// Single-sequential-caller assumption: unlike the Unix
    /// [`tokio::net::UnixListener::accept`] (which takes `&self` and is safe to
    /// poll from several tasks at once), this `accept` `take()`s the one pending
    /// pipe instance out of the `Mutex` before awaiting — so a *second*
    /// concurrent `accept` would find `None` and error. The daemon's serve loop
    /// calls `accept` sequentially (one accept, spawn handler, loop), so this
    /// holds; do not call it from multiple tasks concurrently.
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
        let next = create_secured_instance(&self.name, false)?;
        *self.pending.lock().unwrap_or_else(|p| p.into_inner()) = Some(next);

        Ok(IpcStream::Server(server))
    }
}

/// Create one byte-mode pipe instance carrying the current-user-only security
/// descriptor. `first` selects `FILE_FLAG_FIRST_PIPE_INSTANCE` (bind only).
///
/// The descriptor is rebuilt per instance rather than cached on the listener: a
/// `PSECURITY_DESCRIPTOR` is a raw pointer and therefore not `Send`, while the
/// listener is held across `await` points in the daemon's serve loop. Rebuilding
/// costs one SDDL parse per accepted connection and keeps the listener trivially
/// `Send`.
fn create_secured_instance(name: &std::ffi::OsStr, first: bool) -> io::Result<NamedPipeServer> {
    let descriptor = pipe_security_descriptor()?;
    let mut attrs = descriptor.security_attributes();

    let mut options = ServerOptions::new();
    options.pipe_mode(PipeMode::Byte).first_pipe_instance(first);

    // SAFETY: `attrs` is a fully initialized `SECURITY_ATTRIBUTES` whose
    // `lpSecurityDescriptor` points at `descriptor`, still in scope for the whole
    // call. `CreateNamedPipeW` copies the descriptor into the new object and
    // retains neither pointer.
    let created = unsafe {
        options
            .create_with_security_attributes_raw(name, (&raw mut attrs).cast::<std::ffi::c_void>())
    };
    drop(descriptor);

    created.map_err(|err| {
        if first && err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
            io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "the named pipe {} already exists — another daemon (or another user) holds \
                     it; refusing to clobber it",
                    name.to_string_lossy()
                ),
            )
        } else {
            err
        }
    })
}

/// Run the client-side owner-SID check on a freshly-opened pipe handle, turning a
/// failure into a `PermissionDenied` error that names the endpoint.
fn verify_server_owner(handle: RawHandle, endpoint: &Path) -> io::Result<()> {
    verify_object_owner_is_current_user(handle as HANDLE).map_err(|reason| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing to talk to the named pipe {}: {reason}",
                endpoint.display()
            ),
        )
    })
}

/// Blocking single-shot IPC client for the sync hook/ui paths.
///
/// Windows backend: the pipe is opened with `FILE_FLAG_OVERLAPPED` and each read
/// and write is an overlapped operation reaped through `GetOverlappedResultEx`
/// with a millisecond deadline. That is what gives the synchronous TUI request
/// path (`ui::send_daemon_request_blocking`) the read/write deadline Unix gets
/// from `SO_RCVTIMEO`/`SO_SNDTIMEO`; a plain `std::fs::File` on a pipe honours no
/// timeout at all, so a wedged daemon used to hang the TUI key path outright
/// (PRD #163 M4).
pub struct IpcClient {
    /// The pipe, opened overlapped. `OwnedHandle` gives RAII close plus `Send` +
    /// `Sync`, matching the Unix backend's `UnixStream`.
    handle: OwnedHandle,
    /// Manual-reset event the OVERLAPPED waits on. One per client, reused (and
    /// reset) per operation — the client is strictly one-operation-at-a-time.
    event: OwnedHandle,
    /// Current deadline in milliseconds, or [`INFINITE`]. Atomic because
    /// `set_timeouts` takes `&self` (the Unix signature).
    timeout_ms: AtomicU32,
}

impl IpcClient {
    /// Connect synchronously by opening the pipe for read+write. Retries
    /// briefly on `ERROR_PIPE_BUSY` (mirrors the async client).
    ///
    /// **PRD #163 M4:** the server's owner SID is verified before this returns, so
    /// the hook client — which is *not* daemon-gated and would otherwise write
    /// hook payloads into, and accept replies from, whatever holds the predictable
    /// per-user pipe name — cannot talk to a foreign-squatted pipe.
    /// `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION` additionally denies a
    /// hostile server the ability to impersonate us in the window before the
    /// check — stated explicitly on the async side too
    /// ([`IpcStream::connect`]) rather than inherited from a `tokio` default.
    pub fn connect(endpoint: &Path) -> io::Result<Self> {
        Self::connect_within(endpoint, CONNECT_RETRY_BUDGET)
    }

    /// Connect bounded by `timeout` — the Windows half of issue #435's
    /// `connect_timeout` seam, so the two synchronous callers can state one
    /// deadline that actually covers the connect step on both platforms.
    ///
    /// Windows was never able to hang here: a named-pipe connect either
    /// succeeds, fails outright, or reports `ERROR_PIPE_BUSY` (all instances
    /// taken, the next not yet created — the analogue of a momentarily-full
    /// Unix accept queue), and that last case has always been bounded by
    /// [`CONNECT_RETRY_BUDGET`]. What it could not do is honour a budget
    /// *shorter* than that: the 250 ms hint path in
    /// `ui::send_daemon_request_blocking_with_timeout` would still spend two
    /// seconds retrying. Taking the smaller of the two therefore tightens this
    /// path and can never loosen it.
    pub fn connect_timeout(endpoint: &Path, timeout: Duration) -> io::Result<Self> {
        Self::connect_within(endpoint, timeout.min(CONNECT_RETRY_BUDGET))
    }

    fn connect_within(endpoint: &Path, retry_budget: Duration) -> io::Result<Self> {
        let wide = wide_nul(endpoint.as_os_str());
        let mut waited = Duration::ZERO;
        let raw = loop {
            // SAFETY: `wide` is a NUL-terminated UTF-16 pipe name that outlives
            // the call; the null security-attributes and template-file arguments
            // are the documented "defaults" form. The returned handle is owned by
            // us and closed exactly once, by `OwnedHandle`.
            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE && !handle.is_null() {
                break handle;
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) && waited < retry_budget {
                std::thread::sleep(CONNECT_RETRY_INTERVAL.min(retry_budget - waited));
                waited += CONNECT_RETRY_INTERVAL;
                continue;
            }
            return Err(err);
        };
        // SAFETY: `raw` is a valid, non-null, exclusively-owned handle from the
        // successful `CreateFileW` above and is not used again after this point.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) };

        verify_server_owner(handle.as_raw_handle(), endpoint)?;

        Ok(Self {
            handle,
            event: create_manual_reset_event()?,
            timeout_ms: AtomicU32::new(duration_to_ms(SYNC_CLIENT_DEFAULT_TIMEOUT)),
        })
    }

    /// Read+write deadline, the Windows counterpart of the Unix
    /// `SO_RCVTIMEO`/`SO_SNDTIMEO` pair. Applies to every subsequent operation on
    /// this client; a timed-out read or write reports
    /// [`io::ErrorKind::TimedOut`] (Unix reports `WouldBlock` for the same
    /// condition — callers treat either as "the daemon did not answer").
    pub fn set_timeouts(&self, timeout: std::time::Duration) -> io::Result<()> {
        self.timeout_ms
            .store(duration_to_ms(timeout), Ordering::Relaxed);
        Ok(())
    }

    /// Half-close the write side (Unix `shutdown(Shutdown::Write)`). A named pipe
    /// has no half-close primitive, so this stays a no-op.
    ///
    /// Nothing depends on it any more: `hook::request_from_socket` used to read to
    /// EOF, which on Windows would have deadlocked (the daemon keeps its side open
    /// reading further lines, and we can never send it an EOF while we still want
    /// its reply), so PRD #163 M4 changed it to read exactly the one reply *line*
    /// the daemon writes. That is EOF-independent, identical in result on Unix,
    /// and the reason this can remain honest about doing nothing.
    pub fn shutdown_write(&self) -> io::Result<()> {
        Ok(())
    }

    fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle() as HANDLE
    }

    /// A zeroed `OVERLAPPED` bound to this client's (freshly reset) event.
    fn fresh_overlapped(&self) -> io::Result<OVERLAPPED> {
        let event = self.event.as_raw_handle() as HANDLE;
        // SAFETY: `event` is this client's live manual-reset event handle.
        if unsafe { ResetEvent(event) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `OVERLAPPED` is a plain C struct (its only non-scalar member is
        // a union of integers/pointer), for which all-zero is the documented
        // "fresh operation" initial state.
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event;
        Ok(overlapped)
    }

    /// Wait for `overlapped` to complete, bounded by the current deadline, and
    /// return the transferred byte count.
    ///
    /// On timeout the operation is cancelled **and then reaped with a blocking
    /// `GetOverlappedResult`** before returning. That second wait is not optional:
    /// `CancelIoEx` only *requests* cancellation, and until the operation actually
    /// completes the kernel may still write into the `OVERLAPPED` and into the
    /// caller's buffer — both of which are about to go out of scope.
    fn wait_overlapped(&self, overlapped: &mut OVERLAPPED) -> io::Result<u32> {
        let handle = self.raw();
        let mut transferred: u32 = 0;
        let timeout = self.timeout_ms.load(Ordering::Relaxed);
        // SAFETY: `handle` is our live pipe handle and `overlapped` describes an
        // operation we started on it whose buffer is still borrowed by the caller.
        // Returns immediately if the operation already completed synchronously.
        if unsafe { GetOverlappedResultEx(handle, overlapped, &mut transferred, timeout, 0) } != 0 {
            return Ok(transferred);
        }

        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(WAIT_TIMEOUT as i32) {
            return Err(err);
        }

        // SAFETY: same handle/operation as above; cancelling is safe at any point
        // and only affects I/O this thread issued for this `OVERLAPPED`.
        unsafe { CancelIoEx(handle, overlapped) };
        let mut discarded: u32 = 0;
        // SAFETY: `bWait = TRUE` blocks until the cancelled operation has really
        // finished, after which the kernel is done with `overlapped` and with the
        // caller's buffer. The result itself is deliberately ignored — the
        // operation failed either way.
        unsafe { GetOverlappedResult(handle, overlapped, &mut discarded, 1) };
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("named-pipe I/O timed out after {timeout}ms"),
        ))
    }
}

impl std::io::Read for IpcClient {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let handle = self.raw();
        let mut overlapped = self.fresh_overlapped()?;
        let len = buf.len().min(u32::MAX as usize) as u32;
        // SAFETY: `handle` is our live pipe handle; `buf` is valid for `len`
        // writable bytes and stays borrowed for the whole function; `overlapped`
        // is a fresh operation bound to our event. Every path below reaps the
        // operation before returning, so the kernel never touches `buf` or
        // `overlapped` after this function ends. The null byte-count pointer is
        // required for overlapped reads.
        let started = unsafe {
            ReadFile(
                handle,
                buf.as_mut_ptr(),
                len,
                ptr::null_mut(),
                &mut overlapped,
            )
        };
        if started == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return eof_or_err(err);
            }
        }
        match self.wait_overlapped(&mut overlapped) {
            Ok(n) => Ok(n as usize),
            Err(err) => eof_or_err(err),
        }
    }
}

impl std::io::Write for IpcClient {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let handle = self.raw();
        let mut overlapped = self.fresh_overlapped()?;
        let len = buf.len().min(u32::MAX as usize) as u32;
        // SAFETY: as in `read`, with `buf` read-only. The operation is always
        // reaped before returning, so the kernel is finished with `buf` and
        // `overlapped` by then.
        let started =
            unsafe { WriteFile(handle, buf.as_ptr(), len, ptr::null_mut(), &mut overlapped) };
        if started == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(err);
            }
        }
        self.wait_overlapped(&mut overlapped).map(|n| n as usize)
    }

    /// No-op: an overlapped `WriteFile` that has been reaped is already in the
    /// pipe's kernel buffer, and there is no user-space buffering to push. The
    /// obvious candidate, `FlushFileBuffers`, is *not* the right call here — on a
    /// named pipe it blocks until the peer has drained everything written, which
    /// would turn `flush()` into an unbounded wait on the daemon's read loop.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// EOF mapping shared by the read paths: a broken pipe (the peer closed) is the
/// named-pipe spelling of a zero-length read, not an error. The sync callers
/// (`hook`, `ui`) rely on this to see a clean end of stream.
fn eof_or_err(err: io::Error) -> io::Result<usize> {
    match err.raw_os_error().map(|code| code as u32) {
        Some(ERROR_BROKEN_PIPE) | Some(ERROR_HANDLE_EOF) => Ok(0),
        _ => Err(err),
    }
}

/// Create the manual-reset, initially-unsignalled event used by the overlapped
/// operations. Manual reset (not auto) so `GetOverlappedResult`'s second wait on
/// the cancellation path still sees the completion.
fn create_manual_reset_event() -> io::Result<OwnedHandle> {
    // SAFETY: null attributes/name are the documented "unnamed, default security"
    // form; the two BOOLs are `bManualReset = TRUE`, `bInitialState = FALSE`.
    let handle = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `handle` is a valid, exclusively-owned event handle, closed exactly
    // once by the returned `OwnedHandle`.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) })
}

/// NUL-terminated UTF-16 copy of an `OsStr`, for the `…W` Win32 entry points.
fn wide_nul(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Milliseconds for the Win32 wait APIs. Saturates at `INFINITE - 1` so an
/// absurdly long caller-supplied timeout can never be mistaken for "wait forever",
/// and rounds a sub-millisecond timeout up to 1ms so it stays a deadline rather
/// than becoming a non-blocking poll.
fn duration_to_ms(timeout: Duration) -> u32 {
    let ms = timeout.as_millis();
    if ms == 0 {
        return 1;
    }
    u32::try_from(ms).unwrap_or(INFINITE - 1).min(INFINITE - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Millisecond conversion is pure data; the interesting cases are the two
    /// ends, where getting it wrong turns a deadline into "wait forever" (the
    /// exact hang this milestone removes) or into a non-blocking poll.
    #[test]
    fn timeout_conversion_never_degenerates_to_infinite_or_zero() {
        assert_eq!(duration_to_ms(Duration::from_secs(5)), 5_000);
        assert_eq!(duration_to_ms(Duration::from_micros(1)), 1);
        assert_eq!(duration_to_ms(Duration::ZERO), 1);
        assert_ne!(
            duration_to_ms(Duration::from_secs(u64::MAX / 1000)),
            INFINITE
        );
        assert!(duration_to_ms(Duration::from_secs(u64::MAX / 1000)) < INFINITE);
    }
}
