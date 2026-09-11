//! The attach transport seam (PRD #741 M3).
//!
//! The attach protocol's *framing* was already transport-agnostic — [`read_frame`]
//! and [`write_frame`] take `AsyncRead`/`AsyncWrite` and an SSH channel or a pipe
//! pair satisfies both. What was concrete was the small structural half: the
//! client connected to a [`crate::platform::ipc::IpcStream`] and every long-lived
//! struct that held a connection named that backend's half types. This module is
//! the type that replaces them, so the client can run over something that is not
//! a Unix socket or a named pipe without any of those structs learning what.
//!
//! # Why boxing rather than a type parameter
//!
//! Both work. Boxing wins on cost-of-change and loses ~nothing at run time:
//!
//! - **What genericising costs.** The write half is stored in
//!   `desktop/src-tauri/src/terminal.rs`'s `TerminalSession`, which lives in a
//!   `HashMap` inside `DesktopState`, which is a Tauri-managed singleton reached
//!   from every command. A `T` on the half propagates to `TerminalSession`, to
//!   the session registry, to `DesktopState`, and to the three connection types
//!   here ([`crate::daemon_client::EventSubscription`],
//!   [`crate::daemon_client::AttachConnection`], and `DaemonClient` itself, since
//!   `connect()` is what produces the halves).
//! - **What boxing costs, named.** One vtable dispatch per `poll_write`/
//!   `poll_read` call. On the PTY path that is per **frame**, not per byte:
//!   `write_input` emits one `KIND_STREAM_IN` frame per keystroke batch, and the
//!   output side reads one `KIND_STREAM_OUT` frame per daemon write. An indirect
//!   call is single-digit nanoseconds next to a syscall and a tokio wake-up, so
//!   the cost is unmeasurable against the work it sits on.
//!
//! **What does *not* justify the box, though an earlier draft of these docs said
//! it did.** That draft argued a single registry could not hold a local and a
//! remote session at once without an enum or a box. Under PRD #741's DECISION 1A
//! that is **false**: a remote deck is reached through an `ssh -N -L` *forwarded
//! Unix socket*, so both sessions are a
//! [`crate::platform::ipc::IpcStream`] and one type parameter would hold them
//! both without complaint. The registry is homogeneous, and M5 does not change
//! that.
//!
//! So the box is retained for three reasons, none of which is 1A:
//!
//! 1. The cost-of-change above — a `T` threaded through a Tauri-managed
//!    singleton, for a seam with no second concrete type to justify it.
//! 2. It puts the [`HalfCloseOnDrop`] guard at **one** constructor,
//!    [`TransportWriteHalf::new`], rather than at every site that names the
//!    half's type. (A generic version could carry the same bound; what the box
//!    buys is that there is a single place to carry it.)
//! 3. It makes the transports that *would* be genuine second implementors cheap
//!    to add later without touching a signature: DECISION 1B (`proxy-stdio`,
//!    deferred — its halves are a child process's `ChildStdin`/`ChildStdout`,
//!    see [`HalfCloseOnDrop`] for why that one is not merely a boxing question)
//!    and a non-socket Windows path.
//!
//! # The half-close, which is the trap this module is shaped around
//!
//! [`crate::platform::ipc`]'s module docs record that an earlier draft of the IPC
//! seam split streams with [`tokio::io::split`], whose generic write half does
//! **not** `shutdown(SHUT_WR)` on drop — *"silently regressing the attach
//! half-close on Linux/macOS"*. The protocol depends on that half-close in two
//! places: [`crate::daemon_client::EventSubscription`] holds its write half for
//! no reason other than tripping the daemon's disconnect detector when it drops,
//! and the attach server moves its write half into an output task that can end
//! before the read loop does.
//!
//! Boxing a half does not lose its `Drop` — a `Pin<Box<dyn AsyncWrite + Send>>`
//! built from `tokio::net::unix::OwnedWriteHalf` still half-closes when the box
//! drops. What *would* lose it is splitting with [`tokio::io::split`] on the way
//! in, and nothing about an `AsyncWrite` bound distinguishes the two. So the
//! bound here is not `AsyncWrite`: [`TransportWriteHalf::new`] takes a
//! [`HalfCloseOnDrop`], a marker whose `impl` sites are the places that have to
//! argue the property. There are exactly two, one per IPC backend, and
//! `tokio::io::split`'s `WriteHalf` deliberately has no `impl` on Unix — so
//! reintroducing the regression is a compile error rather than a silent
//! behaviour change.
//!
//! [`read_frame`]: crate::daemon_protocol::read_frame
//! [`write_frame`]: crate::daemon_protocol::write_frame

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A connection the attach protocol can run over.
///
/// Implemented for [`crate::platform::ipc::IpcStream`], and — under PRD #741's
/// DECISION 1A — **that is the only implementor M5 needs**. A remote deck is
/// reached through an `ssh -N -L` forwarded Unix socket, which is an `IpcStream`
/// like any other, so M5 adds no second `impl` here. Do not go looking for one:
/// the seam exists so the client's long-lived structs stop *naming* the IPC
/// backend's half types, not because a second backend is queued behind it. The
/// candidates that would be a second implementor are both deferred — DECISION
/// 1B's `proxy-stdio` and a non-socket Windows path.
///
/// The contract is a **split**, not a stream, because every client call site
/// splits immediately — see [`Self::split_transport`].
pub trait AttachTransport: Send + 'static {
    /// Split into owned halves.
    ///
    /// The write half must be built with [`TransportWriteHalf::new`], so an
    /// implementor is made to name a type whose drop signals end-of-write. An
    /// implementation that splits with [`tokio::io::split`] and hands the result
    /// here does not compile on Unix, which is the point — see the module docs.
    fn split_transport(self) -> (TransportReadHalf, TransportWriteHalf);
}

/// Dropping `Self` makes the peer observe end-of-write, using the strongest
/// primitive the transport has.
///
/// A marker, and the only bound [`TransportWriteHalf::new`] accepts. Writing an
/// `impl` is the act of asserting the property for one concrete half type;
/// `AsyncWrite` cannot express it, which is exactly how the regression recorded
/// in [`crate::platform::ipc`]'s module docs got in.
///
/// **What an `impl` is allowed to mean.** Two answers qualify, and the Windows
/// backend is why the second one does:
///
/// 1. The half itself half-closes on drop — `tokio::net::unix::OwnedWriteHalf`
///    performs `shutdown(SHUT_WR)`, so the peer sees EOF while our read half is
///    still live.
/// 2. The transport has no per-half half-close primitive at all, and the peer
///    observes EOF once *both* halves drop. That is the Windows named pipe
///    (`platform/ipc/windows.rs`), and the protocol already runs on it — so the
///    bar this trait sets is "the strongest the transport has", not "SHUT_WR".
///
/// What does **not** qualify is a wrapper that discards a half-close the
/// underlying transport does have. [`tokio::io::split`] over a Unix socket is
/// precisely that, and has no `impl` here.
///
/// **And what this trait cannot check at all: a half that ends at a different
/// machine than the daemon does.** The two answers above are both about *this*
/// process's relationship to *the* peer, which holds while the write half is one
/// end of a single connection. If PRD #741's deferred DECISION 1B
/// (`proxy-stdio`) is ever picked up, that stops being true: its write half is
/// the `ChildStdin` of a local `ssh` child, and
/// `impl HalfCloseOnDrop for ChildStdin` **would compile while being wrong**.
/// Dropping it closes the local child's stdin, which is a half-close of the
/// *ssh hop*; the daemon at the far end sees EOF only if the remote
/// `proxy-stdio` process notices its own stdin closing and propagates it as a
/// half-close on the socket it holds. That is a property of a program on another
/// host, and nothing here — not the type, not `Drop`, not the bound — can
/// verify it.
///
/// So for a cross-hop half an `impl` is an assertion about the **far end's**
/// behaviour rather than about this type's drop glue, and it earns its place
/// only once that propagation is implemented and tested end to end. Writing one
/// because it compiles is how [`crate::platform::ipc`]'s recorded regression
/// gets back in by a longer route: the client would close, the daemon would keep
/// the session live, and nothing local would look wrong.
pub trait HalfCloseOnDrop: AsyncWrite + Send + 'static {}

/// Owned read half of an [`AttachTransport`].
///
/// A boxed `dyn AsyncRead`, so the three connection types in
/// [`crate::daemon_client`] name one type regardless of what is underneath.
/// Carries no drop contract — end-of-read is the peer's business.
pub struct TransportReadHalf(Pin<Box<dyn AsyncRead + Send>>);

impl TransportReadHalf {
    /// Box a concrete read half.
    pub fn new<R: AsyncRead + Send + 'static>(inner: R) -> Self {
        Self(Box::pin(inner))
    }
}

impl std::fmt::Debug for TransportReadHalf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TransportReadHalf")
    }
}

impl AsyncRead for TransportReadHalf {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.get_mut().0.as_mut().poll_read(cx, buf)
    }
}

/// Owned write half of an [`AttachTransport`], whose drop signals end-of-write
/// to the peer.
///
/// The drop behaviour is inherited from the concrete half inside the box — a
/// boxed `OwnedWriteHalf` still runs `OwnedWriteHalf::drop`. What the type adds
/// is the *gate* on what may go in: [`Self::new`] takes a [`HalfCloseOnDrop`],
/// not an `AsyncWrite`.
pub struct TransportWriteHalf(Pin<Box<dyn AsyncWrite + Send>>);

impl TransportWriteHalf {
    /// Box a concrete write half whose drop signals end-of-write.
    ///
    /// The bound is the whole mechanism; see [`HalfCloseOnDrop`].
    pub fn new<W: HalfCloseOnDrop>(inner: W) -> Self {
        Self(Box::pin(inner))
    }
}

impl std::fmt::Debug for TransportWriteHalf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TransportWriteHalf")
    }
}

impl AsyncWrite for TransportWriteHalf {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().0.as_mut().poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().0.as_mut().poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().0.as_mut().poll_shutdown(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().0.as_mut().poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use tokio::io::AsyncWriteExt;

    use super::*;
    use crate::daemon_protocol::{KIND_REQ, KIND_RESP, read_frame, write_frame};
    use crate::platform::ipc::IpcStream;

    /// PRD #741 M3's test-plan item 4, at the seam itself.
    ///
    /// Drives a **scripted socket** — a one-shot listener that accepts, reads a
    /// frame and answers, the pattern `daemon_bridge.rs`'s `scripted_daemon`
    /// already uses — and asserts that dropping the client's **write half
    /// alone**, while its read half is still live, makes the server observe EOF.
    ///
    /// This is the property `platform/ipc/mod.rs:19-25` records an earlier draft
    /// silently regressing by splitting with [`tokio::io::split`], and it is
    /// what `EventSubscription` holds `_wr` for. It fails as a hang-then-timeout
    /// rather than a wrong value, which is why the server's second read is
    /// bounded: an unbounded `read_frame` on a regressed build would wedge the
    /// test run instead of reporting.
    ///
    /// Binds a plain `tokio::net::UnixListener` rather than `IpcListener::bind`,
    /// for the reason `scripted_daemon` gives: that helper flips the
    /// process-global umask around `bind(2)`, and under single-process
    /// `cargo test` that can hand a sibling test's tempdir mode 0o600.
    #[tokio::test]
    async fn dropping_the_write_half_alone_gives_the_server_eof() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("s");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind the scripted daemon");

        let server = tokio::spawn(async move {
            let (stream, _peer) = listener.accept().await.expect("accept one client");
            let (mut reader, mut writer) = stream.into_split();
            let (kind, _payload) = read_frame(&mut reader)
                .await
                .expect("read the request frame")
                .expect("the client sent a frame");
            assert_eq!(kind, KIND_REQ);
            write_frame(&mut writer, KIND_RESP, b"{}")
                .await
                .expect("answer the client");
            // The assertion. With a half-close this returns `Ok(None)` the
            // moment the client's write half drops; without one it blocks until
            // the client's READ half drops too, which never happens here.
            tokio::time::timeout(Duration::from_secs(5), read_frame(&mut reader)).await
        });

        let transport = IpcStream::connect(&socket)
            .await
            .expect("connect to the scripted daemon");
        let (mut rd, mut wr) = transport.split_transport();
        write_frame(&mut wr, KIND_REQ, b"{\"op\":\"hello\"}")
            .await
            .expect("send the request");
        wr.flush().await.expect("flush the request");
        read_frame(&mut rd)
            .await
            .expect("read the reply")
            .expect("the scripted daemon answered");

        drop(wr);
        // `rd` is deliberately still alive and deliberately not read from: the
        // regression this pins is invisible unless the read half outlives the
        // write half, because dropping both closes the socket either way.
        let observed = server
            .await
            .expect("the scripted daemon must not panic")
            .expect(
                "the server must observe EOF once the client's write half drops — a write half \
                 that does not half-close on drop leaves it blocked here forever",
            )
            .expect("EOF is not an I/O error");
        assert!(
            observed.is_none(),
            "EOF must arrive as a clean end-of-stream, got frame {observed:?}"
        );
        drop(rd);
    }
}
