//! M2.1: stdio-side bridge for the streaming attach protocol (PRD #76).
//!
//! `dot-agent-deck daemon attach` runs on the **remote** host. ssh execs it
//! there, the local TUI plumbs frames through ssh's stdin/stdout, and this
//! bridge byte-relays them to the remote's local attach socket:
//!
//! ```text
//! local TUI <—frames—> ssh stdin/stdout <—frames—> [remote: daemon attach <—frames—> /tmp/dot-agent-deck-attach.sock]
//! ```
//!
//! The bridge does **not** parse frames — the existing wire format
//! (length-prefixed binary, see [`crate::daemon_protocol`]) already runs
//! over any `AsyncRead` / `AsyncWrite` pair, so a transparent byte copy in
//! both directions is sufficient.

use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;

/// Errors surfaced by the bridge. The CLI handler renders these to stderr
/// before exiting nonzero; tests match on the variant.
#[derive(Debug, Error)]
pub enum AttachError {
    #[error(
        "daemon attach socket not found at {path}: is the daemon running on this host? (set $DOT_AGENT_DECK_ATTACH_SOCKET to override)"
    )]
    SocketMissing { path: PathBuf },
    #[error("failed to connect to daemon attach socket {path}: {source}")]
    Connect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Run the stdio ↔ attach-socket bridge. Returns once either direction
/// closes:
///
/// - **stdin EOF** (parent ssh hung up): the inbound copy returns; we let
///   the outbound copy finish flushing whatever the daemon already had
///   queued, then exit.
/// - **socket close from daemon side** (daemon shut down or detached): the
///   outbound copy returns; the inbound future is cancelled when `select!`
///   completes, dropping its borrows of stdin and the socket write half.
/// - **broken pipe on stdout** (parent ssh died): the outbound copy returns
///   `Err`; we treat that as "exit cleanly, the parent's gone".
///
/// Generic over `AsyncRead` / `AsyncWrite` so tests can drive it through
/// `tokio::io::duplex` pipes without forking a process.
pub async fn run_daemon_attach<R, W>(
    socket_path: &Path,
    mut stdin: R,
    mut stdout: W,
) -> Result<(), AttachError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if !socket_path.exists() {
        return Err(AttachError::SocketMissing {
            path: socket_path.to_path_buf(),
        });
    }
    let stream = UnixStream::connect(socket_path)
        .await
        .map_err(|source| AttachError::Connect {
            path: socket_path.to_path_buf(),
            source,
        })?;
    let (mut sock_rd, mut sock_wr) = stream.into_split();

    // Two transparent byte copies. Whichever finishes first cancels the
    // other via `select!`, which drops the inactive future and releases its
    // half of the socket FD deterministically — same pattern as
    // `embedded_pane::create_stream_pane`'s reader/writer select! (PRD M1.3
    // fix-up F2). Errors on either copy are treated as "the peer is gone";
    // they're not propagated because there is no useful recovery from
    // here — the caller's only job after the bridge exits is to flush and
    // return.
    let inbound = async {
        let _ = tokio::io::copy(&mut stdin, &mut sock_wr).await;
    };
    let outbound = async {
        let _ = tokio::io::copy(&mut sock_rd, &mut stdout).await;
    };
    tokio::pin!(inbound, outbound);
    tokio::select! {
        _ = &mut inbound => {},
        _ = &mut outbound => {},
    }

    Ok(())
}
