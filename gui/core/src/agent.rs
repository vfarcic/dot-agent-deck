//! Agent listing, the embedded-terminal attach stream, and resize coalescing
//! (PRD #176 M1.3).
//!
//! These are the per-operation pieces the Tauri shell drives to put a LIVE
//! terminal in the webview, kept here in the plain-Rust core so the Rust gates
//! (`fmt`/`clippy`/`test-fast`) exercise the protocol logic — the shell on top
//! only marshals bytes to/from xterm.js.
//!
//! The connection model mirrors the TUI's `DaemonClient` exactly (Design
//! Decision #1 — the GUI is a *fourth client*, not a fork): every request opens
//! its own short-lived socket connection, **except** [`attach_stream`], which
//! after a successful `RESP` keeps the socket open as a bidirectional pipe —
//! `KIND_STREAM_OUT` daemon→client (PTY bytes), `KIND_STREAM_IN` /
//! `KIND_DETACH` client→daemon. Resize is a separate short connection per op
//! ([`resize_agent`]), coalesced single-slot latest-wins via [`ResizeHandle`]
//! exactly like `embedded_pane.rs`'s per-pane resize worker.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::watch;

use protocol::{
    AgentEvent, AgentRecord, AttachRequest, AttachResponse, BroadcastMsg, KIND_DETACH, KIND_EVENT,
    KIND_REQ, KIND_RESP, KIND_STREAM_END, KIND_STREAM_IN, KIND_STREAM_OUT, read_frame, write_frame,
};

/// Bounded wait for an in-flight daemon `Resize` round-trip, mirroring
/// `embedded_pane::RESIZE_DAEMON_TIMEOUT`. Far longer than a healthy local
/// Unix-socket resize but short enough that a wedged daemon can't park the
/// per-stream resize worker forever (the connection drops on timeout, freeing
/// the FD).
const RESIZE_DAEMON_TIMEOUT: Duration = Duration::from_secs(2);

/// Errors from the per-operation daemon calls. The shell maps these to a
/// user-visible message; the variants intentionally mirror the TUI's
/// `daemon_client::ClientError` shape so behavior stays recognizable.
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("daemon attach socket {0} does not exist (is the daemon running?)")]
    SocketMissing(PathBuf),
    #[error("I/O error talking to the daemon: {0}")]
    Io(io::Error),
    #[error("daemon rejected the request: {0}")]
    Server(String),
    #[error("malformed daemon reply: {0}")]
    Malformed(String),
}

/// Open a connection to the daemon attach socket, surfacing a clear
/// "not running" error before any I/O (mirrors the TUI's
/// `DaemonClient::ensure_socket_exists`).
async fn connect(socket: &Path) -> Result<UnixStream, ClientError> {
    if !socket.exists() {
        return Err(ClientError::SocketMissing(socket.to_path_buf()));
    }
    UnixStream::connect(socket).await.map_err(ClientError::Io)
}

/// Send one `REQ` and read the matching `RESP`. Shared by the request/response
/// operations below (everything except the streaming attach).
async fn issue(
    rd: &mut OwnedReadHalf,
    wr: &mut OwnedWriteHalf,
    req: &AttachRequest,
) -> Result<AttachResponse, ClientError> {
    let payload = serde_json::to_vec(req).map_err(|e| ClientError::Malformed(e.to_string()))?;
    write_frame(wr, KIND_REQ, &payload)
        .await
        .map_err(ClientError::Io)?;
    match read_frame(rd).await.map_err(ClientError::Io)? {
        None => Err(ClientError::Malformed(
            "daemon closed the connection before replying".into(),
        )),
        Some((KIND_RESP, body)) => {
            serde_json::from_slice(&body).map_err(|e| ClientError::Malformed(e.to_string()))
        }
        Some((kind, _)) => Err(ClientError::Malformed(format!(
            "expected RESP, got frame kind 0x{kind:02x}"
        ))),
    }
}

/// List the agents the daemon is currently managing. Prefers the richer
/// `agent_records` (carrying display names, cwd, dims, tab membership) and
/// falls back to synthesising bare records from the legacy `agents` id list
/// when talking to an older daemon — the same forward-compat preference the
/// TUI's `hydrate_from_daemon` uses.
pub async fn list_agents(socket: &Path) -> Result<Vec<AgentRecord>, ClientError> {
    let stream = connect(socket).await?;
    let (mut rd, mut wr) = stream.into_split();
    let resp = issue(&mut rd, &mut wr, &AttachRequest::ListAgents).await?;
    if !resp.ok {
        return Err(ClientError::Server(
            resp.error.unwrap_or_else(|| "list-agents failed".into()),
        ));
    }
    if let Some(records) = resp.agent_records {
        return Ok(records);
    }
    // Older daemon: only bare ids. Synthesise minimal records so the rest of
    // the GUI can treat both shapes uniformly.
    Ok(resp
        .agents
        .unwrap_or_default()
        .into_iter()
        .map(|id| AgentRecord {
            id,
            pane_id_env: None,
            display_name: None,
            cwd: None,
            tab_membership: None,
            agent_type: None,
            rows: 0,
            cols: 0,
        })
        .collect())
}

/// Propagate a webview-side pane resize to the daemon's PTY (one short
/// connection per call). Coalescing happens upstream in [`ResizeHandle`]; this
/// is the single wire op the resize worker dispatches.
pub async fn resize_agent(
    socket: &Path,
    id: &str,
    rows: u16,
    cols: u16,
) -> Result<(), ClientError> {
    let stream = connect(socket).await?;
    let (mut rd, mut wr) = stream.into_split();
    let resp = issue(
        &mut rd,
        &mut wr,
        &AttachRequest::Resize {
            id: id.to_string(),
            rows,
            cols,
        },
    )
    .await?;
    if !resp.ok {
        return Err(ClientError::Server(
            resp.error.unwrap_or_else(|| "resize failed".into()),
        ));
    }
    Ok(())
}

/// A live attach stream to a daemon-managed agent. After [`attach_stream`]
/// returns, the daemon first replays its scrollback snapshot as
/// `KIND_STREAM_OUT` (so the terminal paints the current screen on first
/// frame) and then streams live PTY output; the writer half carries keystrokes
/// back as `KIND_STREAM_IN`. Mirrors the TUI's `daemon_client::AttachConnection`.
#[derive(Debug)]
pub struct AgentStream {
    rd: OwnedReadHalf,
    wr: OwnedWriteHalf,
}

impl AgentStream {
    /// Read the next chunk of agent output. `Ok(None)` on `KIND_STREAM_END`,
    /// clean EOF, or an unexpected frame kind (logged) — the stream is over.
    pub async fn next_output(&mut self) -> io::Result<Option<Vec<u8>>> {
        match read_frame(&mut self.rd).await? {
            None => Ok(None),
            Some((KIND_STREAM_OUT, bytes)) => Ok(Some(bytes)),
            Some((KIND_STREAM_END, _)) => Ok(None),
            Some((kind, _)) => {
                tracing_warn(kind);
                Ok(None)
            }
        }
    }

    /// Forward a chunk of keystrokes to the daemon's PTY stdin.
    pub async fn write_input(&mut self, bytes: &[u8]) -> io::Result<()> {
        write_frame(&mut self.wr, KIND_STREAM_IN, bytes).await
    }

    /// Send an explicit `KIND_DETACH` so the daemon keeps the agent running
    /// (voluntary detach). Best-effort: dropping the stream without this is
    /// still observed as detach when the socket closes.
    pub async fn detach(mut self) -> io::Result<()> {
        write_frame(&mut self.wr, KIND_DETACH, &[]).await
    }

    /// Split into independent halves so the shell can run a read-pump task
    /// while a writer task drains queued keystrokes (the typical wiring).
    pub fn into_split(self) -> (AgentStreamReader, AgentStreamWriter) {
        (
            AgentStreamReader { rd: self.rd },
            AgentStreamWriter { wr: self.wr },
        )
    }
}

/// Emit the "unexpected frame kind on attach stream" warning without forcing a
/// `tracing` dependency on this crate — `eprintln!` keeps the core dependency
/// surface minimal while still surfacing protocol violations during dev.
fn tracing_warn(kind: u8) {
    eprintln!("dad-gui-core: unexpected frame kind 0x{kind:02x} on attach stream — ending");
}

/// Read half of a split [`AgentStream`] — yields agent output chunks.
#[derive(Debug)]
pub struct AgentStreamReader {
    rd: OwnedReadHalf,
}

impl AgentStreamReader {
    /// See [`AgentStream::next_output`].
    pub async fn next_output(&mut self) -> io::Result<Option<Vec<u8>>> {
        match read_frame(&mut self.rd).await? {
            None => Ok(None),
            Some((KIND_STREAM_OUT, bytes)) => Ok(Some(bytes)),
            Some((KIND_STREAM_END, _)) => Ok(None),
            Some((kind, _)) => {
                tracing_warn(kind);
                Ok(None)
            }
        }
    }
}

/// Write half of a split [`AgentStream`] — carries keystrokes + detach.
#[derive(Debug)]
pub struct AgentStreamWriter {
    wr: OwnedWriteHalf,
}

impl AgentStreamWriter {
    /// See [`AgentStream::write_input`].
    pub async fn write_input(&mut self, bytes: &[u8]) -> io::Result<()> {
        write_frame(&mut self.wr, KIND_STREAM_IN, bytes).await
    }

    /// See [`AgentStream::detach`].
    pub async fn detach(mut self) -> io::Result<()> {
        write_frame(&mut self.wr, KIND_DETACH, &[]).await
    }
}

/// Open an attach-stream connection to `id`. Returns once the daemon confirms
/// the attach with an OK `RESP`; the next reads are the scrollback snapshot
/// followed by live `KIND_STREAM_OUT` frames.
pub async fn attach_stream(socket: &Path, id: &str) -> Result<AgentStream, ClientError> {
    let stream = connect(socket).await?;
    let (mut rd, mut wr) = stream.into_split();
    let resp = issue(
        &mut rd,
        &mut wr,
        &AttachRequest::AttachStream { id: id.to_string() },
    )
    .await?;
    if !resp.ok {
        return Err(ClientError::Server(
            resp.error.unwrap_or_else(|| "attach-stream failed".into()),
        ));
    }
    Ok(AgentStream { rd, wr })
}

// ---------------------------------------------------------------------------
// Event subscription (pane status / live roster) — mirrors the TUI's M2.17
// event consumer.
// ---------------------------------------------------------------------------

/// A live `SubscribeEvents` subscription. After the daemon acks the request it
/// streams `KIND_EVENT` frames carrying `BroadcastMsg::Event(AgentEvent)` hook
/// events (agent activity → pane status, plus session start/end for the live
/// roster). The GUI surfaces these as status badges and auto-refreshes the
/// agent list, retiring the manual Refresh.
#[derive(Debug)]
pub struct EventStream {
    rd: OwnedReadHalf,
    /// Keep the write half alive so the connection stays fully open for the
    /// subscription's lifetime — the client never writes after the request.
    _wr: OwnedWriteHalf,
}

impl EventStream {
    /// Read the next hook event. `Ok(None)` on clean EOF. Non-event frames and
    /// unparseable event payloads are skipped (logged) rather than ending the
    /// stream, so one malformed broadcast can't kill the subscription.
    pub async fn next_event(&mut self) -> io::Result<Option<AgentEvent>> {
        loop {
            match read_frame(&mut self.rd).await? {
                None => return Ok(None),
                Some((KIND_EVENT, payload)) => {
                    match serde_json::from_slice::<BroadcastMsg>(&payload) {
                        Ok(BroadcastMsg::Event(ev)) => return Ok(Some(ev)),
                        Err(e) => {
                            eprintln!("dad-gui-core: skipping unparseable event frame: {e}");
                            continue;
                        }
                    }
                }
                // Ignore any non-event frame kinds the daemon might interleave.
                Some((_other, _)) => continue,
            }
        }
    }
}

/// Open a `SubscribeEvents` subscription. Returns once the daemon acks with an
/// OK `RESP`; subsequent reads are `KIND_EVENT` broadcasts via
/// [`EventStream::next_event`].
pub async fn subscribe_events(socket: &Path) -> Result<EventStream, ClientError> {
    let stream = connect(socket).await?;
    let (mut rd, mut wr) = stream.into_split();
    let resp = issue(&mut rd, &mut wr, &AttachRequest::SubscribeEvents).await?;
    if !resp.ok {
        return Err(ClientError::Server(
            resp.error
                .unwrap_or_else(|| "subscribe-events failed".into()),
        ));
    }
    Ok(EventStream { rd, _wr: wr })
}

// ---------------------------------------------------------------------------
// Resize coalescing (single-slot, latest-wins) — mirrors embedded_pane.rs
// ---------------------------------------------------------------------------

/// The producer side of the single-slot resize channel. The webview calls
/// [`ResizeHandle::resize`] on every xterm `onResize`; rapid layout churn is
/// **coalesced** — each call overwrites the pending `(rows, cols)`, so only the
/// latest size reaches the wire. This is the GUI mirror of
/// `EmbeddedPaneController::resize_pane_pty`'s `send_replace` on a
/// `tokio::sync::watch` channel (PRD #76 M2.10).
#[derive(Debug, Clone)]
pub struct ResizeHandle {
    tx: watch::Sender<Option<(u16, u16)>>,
}

impl ResizeHandle {
    /// Record the latest desired dimensions, overwriting any pending value.
    /// Never blocks and never holds a lock across `.await`; a dropped worker
    /// (no receivers) simply discards the resize — the right outcome when the
    /// pane is being torn down.
    pub fn resize(&self, rows: u16, cols: u16) {
        let _ = self.tx.send_replace(Some((rows, cols)));
    }
}

/// Create a resize channel: a [`ResizeHandle`] for the webview-facing command
/// and the [`watch::Receiver`] the worker drains. Seeded with `None` so the
/// worker's first `changed()` waits for a real resize, not the seed.
pub fn resize_channel() -> (ResizeHandle, watch::Receiver<Option<(u16, u16)>>) {
    let (tx, rx) = watch::channel(None);
    (ResizeHandle { tx }, rx)
}

/// Per-stream resize worker: drains the latest coalesced `(rows, cols)` and
/// dispatches a single bounded daemon `Resize`, at most one in flight. Exits
/// when the [`ResizeHandle`] (and thus every sender) drops. Mirrors
/// `embedded_pane::resize_worker`: intermediate sizes during a burst are
/// dropped on the floor, only the latest is sent.
pub async fn run_resize_worker(
    mut rx: watch::Receiver<Option<(u16, u16)>>,
    socket: PathBuf,
    id: String,
) {
    // Process-then-wait: take the latest coalesced value at the top of each
    // iteration (the seed `None` on the first pass, skipped by the `if let`),
    // dispatch it, then block for the next change. Taking the value *before*
    // awaiting `changed()` closes the startup race where a resize that lands
    // between channel creation and the worker's first poll would otherwise be
    // consumed-and-skipped as if it were the seed. `borrow_and_update` always
    // returns the most recent value, so a burst still coalesces to one send.
    loop {
        let dims = *rx.borrow_and_update();
        if let Some((rows, cols)) = dims {
            match tokio::time::timeout(
                RESIZE_DAEMON_TIMEOUT,
                resize_agent(&socket, &id, rows, cols),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    eprintln!("dad-gui-core: resize {rows}x{cols} for {id} failed: {e}");
                }
                Err(_) => {
                    eprintln!(
                        "dad-gui-core: resize {rows}x{cols} for {id} timed out after {}ms",
                        RESIZE_DAEMON_TIMEOUT.as_millis()
                    );
                }
            }
        }
        // All senders dropped (pane torn down) → end the worker.
        if rx.changed().await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Single-slot, latest-wins: three rapid `resize` calls with no drain in
    /// between collapse to ONE pending value (the last), and the receiver sees
    /// exactly one pending change — proving intermediate sizes are coalesced
    /// off the wire, the M2.10 contract `embedded_pane` relies on.
    #[tokio::test]
    async fn resize_coalesces_to_latest() {
        let (handle, mut rx) = resize_channel();
        // Seed is None and seen.
        assert_eq!(*rx.borrow_and_update(), None);

        handle.resize(10, 20);
        handle.resize(30, 40);
        handle.resize(50, 60);

        // One coalesced notification despite three sends.
        assert!(rx.has_changed().unwrap(), "a resize should be pending");
        assert_eq!(
            *rx.borrow_and_update(),
            Some((50, 60)),
            "only the latest size survives"
        );
        assert!(
            !rx.has_changed().unwrap(),
            "the three sends coalesced into a single pending value"
        );
    }

    /// Dropping the handle ends the worker loop (no senders left), so a spawned
    /// `run_resize_worker` future completes instead of hanging — the teardown
    /// property the shell relies on when a pane closes.
    #[tokio::test]
    async fn resize_worker_exits_when_handle_dropped() {
        let (handle, rx) = resize_channel();
        let worker = tokio::spawn(run_resize_worker(
            rx,
            PathBuf::from("/nonexistent/never-connected.sock"),
            "agent-1".into(),
        ));
        drop(handle);
        // Without a 5s safety bound a regression here would hang the suite.
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("worker should exit promptly once the handle drops")
            .expect("worker task should not panic");
    }
}
