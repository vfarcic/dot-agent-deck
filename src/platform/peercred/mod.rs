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
//!   Linux, `getsockopt(LOCAL_PEERPID)` on macOS. Symmetric: the same call
//!   answers from either end of the socket.
//! - Windows (PRD #163 M3): named pipes split the two directions into two
//!   calls, so the backend dispatches on which end it holds —
//!   `GetNamedPipeServerProcessId` from the client end,
//!   `GetNamedPipeClientProcessId` from the server end. See that module for why
//!   the dispatch matters even though today's callers are all client-side.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::peer_pid;
#[cfg(windows)]
pub use windows::peer_pid;

#[cfg(test)]
mod tests {
    use super::*;

    /// The API-surface contract both backends must satisfy, from **both** ends of
    /// one connection: `peer_pid` answers with the PID of the process on the other
    /// side. Here the listener and the connector are the same test process, so
    /// both answers must be our own pid — which is exactly what makes the
    /// assertion platform-independent (no helper process, no signalling).
    ///
    /// This is the regression test for the direction bug the Windows backend can
    /// hit and Unix cannot: `GetNamedPipeServerProcessId` called from the *server*
    /// end also returns "our own pid", so it would pass a client-only test while
    /// being wrong in a way that makes a daemon terminate itself. Asserting from
    /// both ends is only meaningful together with the dispatch — see
    /// `peercred::windows`.
    ///
    /// Deliberately not `cfg`-gated: on Unix it pins the `getsockopt` path, and
    /// on the `windows-latest` CI job it becomes a live check of the named-pipe
    /// path as soon as PRD #163 M4 lifts the `IpcListener::bind` security gate
    /// (until then `bind` answers `Unsupported` and the test self-skips rather
    /// than reporting a false pass).
    #[tokio::test]
    async fn peer_pid_reports_the_other_end_from_both_sides() {
        use crate::platform::ipc::{IpcListener, IpcStream};

        let dir = tempfile::tempdir().expect("tempdir");
        // A per-run endpoint of the shape each backend expects: a socket file
        // inside the tempdir on Unix, a `\\.\pipe\…` name on Windows (pipes have
        // no on-disk presence, so the pid keeps concurrent runs apart).
        let endpoint = if cfg!(windows) {
            std::path::PathBuf::from(format!(
                r"\\.\pipe\dot-agent-deck-test-peercred-{}",
                std::process::id()
            ))
        } else {
            dir.path().join("peercred.sock")
        };

        let listener = match IpcListener::bind(&endpoint) {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
                // Windows before #163 M4: no listener may exist yet (the pipe
                // would be created with an Everyone-readable DACL). Nothing to
                // assert, and pretending otherwise would be a false pass.
                eprintln!("skipping: IpcListener::bind is not supported here yet ({e})");
                return;
            }
            Err(e) => panic!("bind({}) failed: {e}", endpoint.display()),
        };

        let connect = tokio::spawn(async move { IpcStream::connect(&endpoint).await });
        let server_end = listener.accept().await.expect("accept");
        let client_end = connect.await.expect("connect task").expect("connect");

        let me = std::process::id();
        // From the client end the peer is the server (both are this process).
        assert_eq!(peer_pid(&client_end).expect("peer_pid from client"), me);
        // From the server end the peer is the client — the direction a
        // server-pid-only implementation would get accidentally "right".
        assert_eq!(peer_pid(&server_end).expect("peer_pid from server"), me);

        // PRD #163 review: the two assertions above cannot *fail* for a swapped
        // Windows dispatch — both ends are this process, so the wrong Win32 call
        // still answers `me`. Pin the direction itself on the connected pair, which
        // is where the mapping is decided and where getting it wrong would make a
        // daemon answering `daemon stop` terminate itself.
        #[cfg(windows)]
        {
            use super::windows::{PeerQuery, peer_query_for};
            assert_eq!(peer_query_for(&client_end), PeerQuery::ServerProcessId);
            assert_eq!(peer_query_for(&server_end), PeerQuery::ClientProcessId);
        }
    }
}
