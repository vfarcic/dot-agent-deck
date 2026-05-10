use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::PtySize;

use std::any::Any;

use crate::agent_pty::{self, AgentPty, DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use crate::daemon_client::{AttachConnection, DaemonClient, StartAgentOptions};
use crate::hyperlink::{HyperlinkMap, Osc8Filter, Osc8Segment};
use crate::pane::{PaneController, PaneDirection, PaneError, PaneInfo};

/// Result of [`EmbeddedPaneController::hydrate_from_daemon`]. One entry per
/// daemon-side agent that was successfully reconnected on TUI bootstrap; the
/// caller uses the pair to register the pane with [`crate::state::AppState`]
/// and seed the UI's display-name maps. Agents that fail to attach (e.g.
/// terminated between list and attach) are not represented here.
#[derive(Debug, Clone)]
pub struct HydratedPane {
    /// Local pane id assigned by the controller.
    pub pane_id: String,
    /// Daemon-side agent id this pane is attached to. The TUI uses this as
    /// the default display name on rehydration since the daemon does not
    /// track per-agent display metadata (PRD #76 M2.x).
    pub agent_id: String,
}

/// PTY-backed pane state: this process owns the PTY master and child. The
/// historical (and only) backend before M1.3.
struct PtyBackend {
    /// Writer to send input to the PTY.
    writer: Box<dyn std::io::Write + Send>,
    /// The child process handle.
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// Master PTY handle (kept alive for resize).
    master: Box<dyn portable_pty::MasterPty + Send>,
}

/// Commands the per-pane I/O task drains from `input_rx`. `Input` carries
/// raw keystroke bytes that get framed as `KIND_STREAM_IN`. `Detach`
/// triggers an explicit `KIND_DETACH` frame and ends the writer half of the
/// task — used by the M2.5 explicit-detach keybinding so the daemon can
/// distinguish voluntary detach from abrupt disconnect (PRD #76, M2.5).
enum StreamCmd {
    Input(Vec<u8>),
    Detach,
}

/// Stream-backed pane state (PRD #76, M1.3): the PTY lives in the daemon,
/// and this side owns one [`crate::daemon_client::AttachConnection`] per
/// pane. Bytes flow daemon → STREAM_OUT → vt100 parser; keystrokes flow
/// vt100 → input channel → STREAM_IN → daemon. The daemon-side agent
/// outlives the TUI by design (PRD line 199), so dropping this struct is
/// implicit detach (the `io_task` stops draining and the socket closes —
/// the daemon's input-loop treats EOF as DETACH). Sending `stop-agent` is
/// reserved for the explicit user-driven Ctrl+W close path in `close_pane`.
struct StreamBackend {
    /// Daemon-side agent id used for `stop-agent` on close.
    agent_id: String,
    /// Channel drained by the per-pane I/O task. `Input` becomes one
    /// `KIND_STREAM_IN` frame on the wire; `Detach` becomes one
    /// `KIND_DETACH` frame and ends the writer. Unbounded because the TUI
    /// keystroke rate is human-paced; backpressure here would block the
    /// input thread for no benefit.
    input_tx: tokio::sync::mpsc::UnboundedSender<StreamCmd>,
    /// Owns the I/O task. The `Option` exists so `detach_pane` can `take()`
    /// the handle, await the writer briefly so the `KIND_DETACH` frame
    /// flushes, and then drop. On plain `Drop` (TUI exit / pane close) the
    /// handle is aborted instead, which closes the attach socket and the
    /// daemon sees EOF — implicit detach (M1.3 survival property).
    io_task: Option<tokio::task::JoinHandle<()>>,
    /// Tokio handle so the (blocking) `close_pane` path can issue
    /// `stop-agent` over a fresh short-lived connection. Also used by the
    /// M2.5 detach path to await the writer briefly while the explicit
    /// `KIND_DETACH` frame is flushed before the socket is dropped.
    runtime: tokio::runtime::Handle,
    /// Daemon attach socket path used to build the `stop-agent` connection
    /// — held here rather than referenced from the controller because the
    /// pane outlives any borrow of the controller's path.
    daemon_path: PathBuf,
    /// Single-slot coalescing channel for resize requests. Each
    /// `resize_pane_pty` overwrites the latest `(rows, cols)` here; the
    /// per-pane `resize_task` reads the most recent value and dispatches
    /// it to the daemon. Intermediate values during rapid layout churn
    /// are dropped on the floor — only the latest size is sent on the
    /// wire (PRD #76 M2.10 audit follow-up).
    resize_tx: tokio::sync::watch::Sender<Option<(u16, u16)>>,
    /// Per-pane resize worker. Aborted on `Drop` so a pane removal can't
    /// leak a task or an in-flight daemon connection past the pane's
    /// lifetime. The worker would also exit on its own when `resize_tx`
    /// drops (the receiver's `changed()` returns `Err`), but explicitly
    /// aborting bounds the cleanup window.
    resize_task: Option<tokio::task::JoinHandle<()>>,
}

enum PaneBackend {
    Pty(PtyBackend),
    Stream(StreamBackend),
}

impl Drop for StreamBackend {
    /// Plain drop = implicit detach (PRD #76 line 199 — agents survive the
    /// TUI). Aborting the io_task closes the attach socket; the daemon
    /// sees EOF on its read half and treats it as a detach. The
    /// `stop-agent` path lives only in `close_pane` for the explicit
    /// Ctrl+W close.
    fn drop(&mut self) {
        if let Some(h) = self.io_task.take() {
            h.abort();
        }
        // The resize worker would exit on its own once `resize_tx` drops
        // (its receiver's `changed()` returns `Err`), but it might be mid
        // I/O against the daemon when that happens. Aborting here bounds
        // the cleanup window so a slow daemon can't keep the worker (and
        // its open socket FD) alive past pane removal.
        if let Some(h) = self.resize_task.take() {
            h.abort();
        }
    }
}

/// State for a single embedded terminal pane.
struct Pane {
    /// Either a locally-owned PTY or a connection to a daemon-managed agent.
    backend: PaneBackend,
    /// Parsed terminal screen (vt100). Shared between the renderer and the
    /// background producer task (PTY reader thread or stream-backed I/O
    /// task).
    screen: Arc<Mutex<vt100::Parser>>,
    /// Display name for this pane.
    name: String,
    /// Whether this pane is currently focused.
    is_focused: bool,
    /// The command that was used to create this pane.
    command: Option<String>,
    /// Whether the child app has enabled mouse reporting (e.g., TUI apps like opencode).
    mouse_mode: Arc<AtomicBool>,
    /// Hyperlink URLs extracted from OSC 8 escape sequences, keyed by screen row.
    hyperlinks: Arc<Mutex<HyperlinkMap>>,
}

/// Thread-safe pane registry.
type PaneRegistry = Arc<Mutex<HashMap<String, Pane>>>;

/// Encode the payload portion of a pane input (content + bracketed paste markers if
/// multi-line) without the trailing submit byte. Trailing whitespace is stripped.
fn encode_pane_payload(text: &str) -> Vec<u8> {
    let trimmed = text.trim_end_matches(['\n', '\r', ' ', '\t']);
    let mut out = Vec::with_capacity(trimmed.len() + 16);
    if trimmed.contains('\n') {
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(trimmed.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
    } else {
        out.extend_from_slice(trimmed.as_bytes());
    }
    out
}

/// Delay between writing input bytes and the submit CR. Agent TUIs like claude
/// treat a CR that arrives fused to the preceding text as newline-in-input; only
/// a CR that arrives as a separate event after a pause is honored as Enter. The
/// same applies after a bracketed-paste close marker. 150ms tuned empirically.
const SUBMIT_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

/// Selects how `create_pane` builds new panes:
/// - `LocalDeck` spawns a PTY in this process (unchanged from pre-M1.3).
/// - `RemoteDeckLocal` issues `start-agent` + `attach-stream` against the
///   daemon at the given socket path. PRD #76, M1.3.
enum ControllerMode {
    LocalDeck,
    RemoteDeckLocal {
        client: DaemonClient,
        runtime: tokio::runtime::Handle,
    },
}

/// Embedded terminal pane controller using portable-pty + vt100.
///
/// Replaces `ZellijController` by spawning PTY processes directly and parsing
/// their output with a VT100 terminal emulator. In M1.3's `RemoteDeckLocal`
/// mode, panes are stream-backed against the daemon's M1.2 attach protocol
/// instead — same vt100-based render path, different byte source.
pub struct EmbeddedPaneController {
    panes: PaneRegistry,
    next_id: Arc<Mutex<u64>>,
    mode: ControllerMode,
}

impl Default for EmbeddedPaneController {
    fn default() -> Self {
        Self::new()
    }
}

impl EmbeddedPaneController {
    pub fn new() -> Self {
        Self {
            panes: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(Mutex::new(1)),
            mode: ControllerMode::LocalDeck,
        }
    }

    /// Build a controller whose panes are stream-backed against the daemon
    /// at `socket_path`. Caller is responsible for ensuring the daemon is
    /// actually running — `DaemonClient::ensure_socket_exists` is the
    /// recommended pre-flight from `main`.
    pub fn with_remote_deck(socket_path: PathBuf, runtime: tokio::runtime::Handle) -> Self {
        Self {
            panes: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(Mutex::new(1)),
            mode: ControllerMode::RemoteDeckLocal {
                client: DaemonClient::new(socket_path),
                runtime,
            },
        }
    }

    /// Access the vt100 screen for a pane (used by the terminal widget for rendering).
    pub fn get_screen(&self, pane_id: &str) -> Option<Arc<Mutex<vt100::Parser>>> {
        let panes = self.panes.lock().unwrap();
        panes.get(pane_id).map(|p| Arc::clone(&p.screen))
    }

    /// Access the hyperlink map for a pane (used for click-to-open).
    pub fn get_hyperlinks(&self, pane_id: &str) -> Option<Arc<Mutex<HyperlinkMap>>> {
        let panes = self.panes.lock().unwrap();
        panes.get(pane_id).map(|p| Arc::clone(&p.hyperlinks))
    }

    /// Return all pane IDs in insertion order (by numeric ID).
    pub fn pane_ids(&self) -> Vec<String> {
        let panes = self.panes.lock().unwrap();
        let mut ids: Vec<String> = panes.keys().cloned().collect();
        ids.sort_by_key(|id| id.parse::<u64>().unwrap_or(0));
        ids
    }

    /// Get the currently focused pane ID, if any.
    pub fn focused_pane_id(&self) -> Option<String> {
        let panes = self.panes.lock().unwrap();
        panes
            .iter()
            .find(|(_, p)| p.is_focused)
            .map(|(id, _)| id.clone())
    }

    /// Write raw bytes directly to a pane's PTY stdin without appending CR.
    /// Used for interactive keyboard input forwarding. For stream-backed
    /// panes the bytes are queued for the per-pane I/O task to forward as
    /// `STREAM_IN` on the wire.
    pub fn write_raw_bytes(&self, pane_id: &str, bytes: &[u8]) -> Result<(), PaneError> {
        let mut panes = self.panes.lock().unwrap();
        if let Some(pane) = panes.get_mut(pane_id) {
            match &mut pane.backend {
                PaneBackend::Pty(p) => {
                    p.writer.write_all(bytes).map_err(PaneError::Io)?;
                    p.writer.flush().map_err(PaneError::Io)?;
                }
                PaneBackend::Stream(s) => {
                    if s.input_tx.send(StreamCmd::Input(bytes.to_vec())).is_err() {
                        return Err(PaneError::CommandFailed(format!(
                            "Pane {pane_id} stream I/O task ended"
                        )));
                    }
                }
            }
            Ok(())
        } else {
            Err(PaneError::CommandFailed(format!(
                "Pane {pane_id} not found"
            )))
        }
    }

    /// Scroll a pane's view by `delta` lines (positive = scroll up into history).
    /// vt100 0.16 clamps the offset to the actual scrollback buffer size.
    pub fn scroll_pane(&self, pane_id: &str, delta: isize) {
        let panes = self.panes.lock().unwrap();
        if let Some(pane) = panes.get(pane_id)
            && let Ok(mut parser) = pane.screen.lock()
        {
            let current = parser.screen().scrollback();
            let new_offset = if delta > 0 {
                current.saturating_add(delta as usize)
            } else {
                current.saturating_sub((-delta) as usize)
            };
            parser.screen_mut().set_scrollback(new_offset);
        }
    }

    /// Reset a pane's scrollback offset to 0 (show latest output).
    pub fn reset_scrollback(&self, pane_id: &str) {
        let panes = self.panes.lock().unwrap();
        if let Some(pane) = panes.get(pane_id)
            && let Ok(mut parser) = pane.screen.lock()
        {
            parser.screen_mut().set_scrollback(0);
        }
    }

    /// Resize a pane's PTY and VT100 parser to the given dimensions. For
    /// stream-backed panes, the local vt100 parser is resized synchronously
    /// and the new dimensions are written to a per-pane single-slot
    /// coalescing channel (PRD #76, M2.10): the per-pane `resize_task`
    /// drains the latest value and forwards a `Resize` op to the daemon
    /// with a bounded timeout. Intermediate values during rapid layout
    /// churn are dropped on the floor — only the latest size reaches the
    /// wire, with at most one in-flight daemon connection per pane.
    pub fn resize_pane_pty(&self, pane_id: &str, rows: u16, cols: u16) -> Result<(), PaneError> {
        let panes = self.panes.lock().unwrap();
        let pane = panes
            .get(pane_id)
            .ok_or_else(|| PaneError::CommandFailed(format!("Pane {pane_id} not found")))?;
        match &pane.backend {
            PaneBackend::Pty(p) => {
                p.master
                    .resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .map_err(|e| PaneError::CommandFailed(format!("PTY resize failed: {e}")))?;
            }
            PaneBackend::Stream(s) => {
                // `send_replace` overwrites whatever value was pending and
                // ignores the no-receivers case (the worker would only be
                // gone if the pane was being torn down — losing the resize
                // is the right outcome there). The watch channel cannot
                // block, so this returns immediately and never holds the
                // pane lock across daemon I/O.
                let _ = s.resize_tx.send_replace(Some((rows, cols)));
            }
        }
        if let Ok(mut parser) = pane.screen.lock() {
            parser.screen_mut().set_size(rows, cols);
        }
        Ok(())
    }

    /// Check if a pane's child app has enabled mouse reporting.
    pub fn mouse_mode_enabled(&self, pane_id: &str) -> bool {
        let panes = self.panes.lock().unwrap();
        panes
            .get(pane_id)
            .is_some_and(|p| p.mouse_mode.load(Ordering::Relaxed))
    }

    /// Forward a mouse scroll event to the child app via SGR extended mouse encoding.
    /// Coordinates are pane-relative (0-indexed) and converted to 1-indexed for the protocol.
    /// Also resets vt100 scrollback to 0 so the terminal widget shows live output.
    pub fn forward_mouse_scroll(
        &self,
        pane_id: &str,
        up: bool,
        col: u16,
        row: u16,
    ) -> Result<(), PaneError> {
        // Ensure we're showing live output, not a stale scrollback position.
        self.reset_scrollback(pane_id);
        let button = if up { 64 } else { 65 };
        let seq = format!("\x1b[<{};{};{}M", button, col + 1, row + 1);
        self.write_raw_bytes(pane_id, seq.as_bytes())
    }

    fn allocate_id(&self) -> String {
        let mut id = self.next_id.lock().unwrap();
        let current = *id;
        *id += 1;
        current.to_string()
    }

    /// Build a PTY-backed pane (default `LocalDeck` mode). Behavior is
    /// byte-identical to the pre-M1.3 path — extracted from `create_pane`
    /// so the M1.3 `RemoteDeckLocal` mode can sit alongside it without
    /// disturbing this branch.
    fn create_local_pane(
        &self,
        pane_id: String,
        command: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<String, PaneError> {
        // Tag the spawned process so hooks can identify which pane it belongs to.
        let env = vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.clone())];

        let AgentPty {
            child,
            master,
            writer,
            mut reader,
        } = agent_pty::spawn(SpawnOptions {
            command,
            cwd,
            rows: 24,
            cols: 80,
            env,
        })
        .map_err(|e| PaneError::CommandFailed(e.to_string()))?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 10_000)));
        let mouse_mode = Arc::new(AtomicBool::new(false));
        let hyperlinks = Arc::new(Mutex::new(HyperlinkMap::new()));

        // Background thread: pump PTY bytes through the shared output
        // pipeline. Same processing path the stream-backed I/O task uses
        // — see `process_agent_output_chunk`.
        let parser_clone = Arc::clone(&parser);
        let mouse_mode_clone = Arc::clone(&mouse_mode);
        let hyperlinks_clone = Arc::clone(&hyperlinks);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut osc8 = Osc8Filter::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => process_agent_output_chunk(
                        &buf[..n],
                        &mut osc8,
                        &parser_clone,
                        &mouse_mode_clone,
                        &hyperlinks_clone,
                    ),
                    Err(_) => break,
                }
            }
        });

        let pane = Pane {
            backend: PaneBackend::Pty(PtyBackend {
                writer,
                child,
                master,
            }),
            screen: parser,
            name: command.unwrap_or("shell").to_string(),
            is_focused: false,
            command: command.map(|c| c.to_string()),
            mouse_mode,
            hyperlinks,
        };

        self.panes.lock().unwrap().insert(pane_id.clone(), pane);

        Ok(pane_id)
    }

    /// Build a stream-backed pane against the daemon (M1.3
    /// `RemoteDeckLocal` mode). The PTY lives in the daemon; this side
    /// holds an [`crate::daemon_client::AttachConnection`] and feeds the
    /// shared vt100 parser from STREAM_OUT bytes.
    fn create_stream_pane(
        &self,
        pane_id: String,
        command: Option<&str>,
        cwd: Option<&str>,
        client: DaemonClient,
        runtime: tokio::runtime::Handle,
    ) -> Result<String, PaneError> {
        // Same hook-tagging as the local path so daemon-spawned agents
        // see DOT_AGENT_DECK_PANE_ID and can emit hook events back to
        // this UI's pane.
        let env = vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.clone())];

        let opts = StartAgentOptions {
            command: command.map(|c| c.to_string()),
            cwd: cwd.map(|c| c.to_string()),
            rows: 24,
            cols: 80,
            env,
        };

        // Start-agent + attach happen on the daemon's runtime; we
        // `block_on` here because `create_pane` is called from the TUI's
        // blocking thread.
        let daemon_path = client.socket_path().to_path_buf();
        let client_for_calls = client.clone();
        let (agent_id, conn) = runtime
            .block_on(async move {
                let id = client_for_calls.start_agent(opts).await?;
                let conn = client_for_calls.attach(&id).await?;
                Ok::<_, crate::daemon_client::ClientError>((id, conn))
            })
            .map_err(|e| PaneError::CommandFailed(format!("daemon: {e}")))?;

        let name = command.unwrap_or("shell").to_string();
        let command = command.map(|c| c.to_string());
        self.wire_stream_pane(
            pane_id.clone(),
            agent_id,
            conn,
            name,
            command,
            runtime,
            daemon_path,
        );
        Ok(pane_id)
    }

    /// Internal helper that takes an already-resolved `agent_id` plus an
    /// active [`AttachConnection`] and stitches together the local-side
    /// pane state: vt100 parser, mouse-mode flag, hyperlink map, the input
    /// channel + writer task, and the per-pane resize worker. Pulled out
    /// of `create_stream_pane` so the M2.x rehydration path
    /// (`hydrate_from_daemon`) can reuse the exact same wiring without
    /// re-issuing `start-agent`. Behavior on the wire is identical: the
    /// daemon replays its scrollback snapshot via STREAM_OUT before live
    /// bytes (see `daemon_protocol::handle_attach_stream`), so a hydrated
    /// pane renders the agent's current screen on first paint.
    #[allow(clippy::too_many_arguments)]
    fn wire_stream_pane(
        &self,
        pane_id: String,
        agent_id: String,
        conn: AttachConnection,
        name: String,
        command: Option<String>,
        runtime: tokio::runtime::Handle,
        daemon_path: PathBuf,
    ) {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 10_000)));
        let mouse_mode = Arc::new(AtomicBool::new(false));
        let hyperlinks = Arc::new(Mutex::new(HyperlinkMap::new()));

        let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<StreamCmd>();
        let (resize_tx, resize_rx) = tokio::sync::watch::channel::<Option<(u16, u16)>>(None);

        // Per-pane resize worker: at-most-one in-flight daemon Resize per
        // pane, with intermediate values coalesced via the watch channel.
        // Survives until either `resize_tx` drops (pane removed) or the
        // worker is aborted by `StreamBackend::drop`. See the comment on
        // `resize_pane_pty` for the full rationale.
        let resize_task = runtime.spawn(resize_worker(
            resize_rx,
            daemon_path.clone(),
            agent_id.clone(),
        ));

        let parser_for_task = Arc::clone(&parser);
        let mouse_mode_for_task = Arc::clone(&mouse_mode);
        let hyperlinks_for_task = Arc::clone(&hyperlinks);

        let io_task = runtime.spawn(async move {
            let (mut rd, mut wr) = conn.into_split();

            // Reader half: STREAM_OUT → process pipeline.
            let reader = async {
                let mut osc8 = Osc8Filter::new();
                loop {
                    match crate::daemon_protocol::read_frame(&mut rd).await {
                        Ok(None) => break,
                        Ok(Some((kind, bytes))) => match kind {
                            crate::daemon_protocol::KIND_STREAM_OUT => {
                                process_agent_output_chunk(
                                    &bytes,
                                    &mut osc8,
                                    &parser_for_task,
                                    &mouse_mode_for_task,
                                    &hyperlinks_for_task,
                                );
                            }
                            crate::daemon_protocol::KIND_STREAM_END => break,
                            _ => break,
                        },
                        Err(_) => break,
                    }
                }
            };

            // Input forwarder: drain the keystroke channel and emit frames.
            // `Input` becomes one `KIND_STREAM_IN`; `Detach` (M2.5) becomes
            // one `KIND_DETACH` and ends the writer so the daemon observes
            // an explicit detach before the socket closes. A failed write
            // or a closed channel also ends the task — the daemon treats
            // the resulting EOF as implicit detach (the agent keeps
            // running).
            let writer = async {
                while let Some(cmd) = input_rx.recv().await {
                    match cmd {
                        StreamCmd::Input(bytes) => {
                            if crate::daemon_protocol::write_frame(
                                &mut wr,
                                crate::daemon_protocol::KIND_STREAM_IN,
                                &bytes,
                            )
                            .await
                            .is_err()
                            {
                                break;
                            }
                        }
                        StreamCmd::Detach => {
                            // Best-effort: even if the write errors, exiting
                            // here closes the socket and the daemon will
                            // observe EOF — the agent still survives.
                            let _ = crate::daemon_protocol::write_frame(
                                &mut wr,
                                crate::daemon_protocol::KIND_DETACH,
                                &[],
                            )
                            .await;
                            break;
                        }
                    }
                }
            };

            // `select!` ensures whichever half completes first takes the
            // other down with it: the inactive future is cancelled and
            // both `rd` and `wr` go out of scope here, releasing the
            // socket FD deterministically. Without this, a STREAM_END
            // from the daemon would end the reader but leave the writer
            // parked on `input_rx.recv()` indefinitely, holding the
            // socket open until the StreamBackend itself was dropped.
            tokio::pin!(reader, writer);
            tokio::select! {
                _ = &mut reader => {},
                _ = &mut writer => {},
            }
        });

        let pane = Pane {
            backend: PaneBackend::Stream(StreamBackend {
                agent_id,
                input_tx,
                io_task: Some(io_task),
                runtime,
                daemon_path,
                resize_tx,
                resize_task: Some(resize_task),
            }),
            screen: parser,
            name,
            is_focused: false,
            command,
            mouse_mode,
            hyperlinks,
        };

        self.panes.lock().unwrap().insert(pane_id, pane);
    }

    /// Reconnect to every daemon-side agent on TUI bootstrap (PRD #76
    /// M2.x). In external-daemon mode the agents the user spawned in a
    /// previous session are still alive in the daemon; without this step
    /// the dashboard would show "No active sessions" even though the
    /// daemon owns live PTYs.
    ///
    /// For each id returned by `list_agents`, builds a fresh
    /// `PaneBackend::Stream` and opens an `AttachStream` (no
    /// `start-agent` — the agent already exists). The daemon replays its
    /// scrollback snapshot before live bytes, so hydrated panes render
    /// the agent's current screen on first paint.
    ///
    /// Errors are absorbed rather than propagated:
    /// - `list_agents` failure (transient daemon hiccup): logged at debug,
    ///   treated as empty. The user can retry by reconnecting.
    /// - Per-agent `attach` failure (race: the agent terminated between
    ///   list and attach): logged at debug, that agent is skipped, others
    ///   continue.
    /// - In `LocalDeck` mode this is a no-op: the in-process daemon either
    ///   has no agents yet or shares the same registry as the TUI, so
    ///   re-discovery is meaningless.
    ///
    /// Returns one [`HydratedPane`] per successfully attached agent, in
    /// the order returned by the daemon. Callers are expected to register
    /// each pane id with [`crate::state::AppState`] and seed the UI's
    /// display-name maps with `agent_id` (the daemon does not track
    /// display metadata yet — out of scope for M2.x).
    pub fn hydrate_from_daemon(&self) -> Vec<HydratedPane> {
        let (client, runtime) = match &self.mode {
            ControllerMode::LocalDeck => return Vec::new(),
            ControllerMode::RemoteDeckLocal { client, runtime } => {
                (client.clone(), runtime.clone())
            }
        };

        let agents = match runtime.block_on(client.list_agents()) {
            Ok(a) => a,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "hydrate_from_daemon: list_agents failed, treating as empty"
                );
                return Vec::new();
            }
        };

        let daemon_path = client.socket_path().to_path_buf();
        let mut hydrated = Vec::new();
        for agent_id in agents {
            let client_for_attach = client.clone();
            let id_for_attach = agent_id.clone();
            let conn = match runtime
                .block_on(async move { client_for_attach.attach(&id_for_attach).await })
            {
                Ok(c) => c,
                Err(e) => {
                    // Race: agent terminated between list_agents and
                    // attach, or transient daemon error. Skip this id
                    // and keep going so a single missing agent doesn't
                    // sink the rest of the rehydration.
                    tracing::debug!(
                        agent_id = %agent_id,
                        error = %e,
                        "hydrate_from_daemon: attach failed, skipping"
                    );
                    continue;
                }
            };
            let pane_id = self.allocate_id();
            // Use agent_id as the pane name. Daemon-side metadata (display
            // name, cwd, modes) is not persisted yet — the user can rename
            // via the existing rename flow.
            self.wire_stream_pane(
                pane_id.clone(),
                agent_id.clone(),
                conn,
                agent_id.clone(),
                None,
                runtime.clone(),
                daemon_path.clone(),
            );
            hydrated.push(HydratedPane { pane_id, agent_id });
        }
        hydrated
    }

    /// Explicit M2.5 detach: tell the daemon "I'm leaving voluntarily,
    /// keep the agent running." The pane is removed from the registry and
    /// its writer is given a brief window to flush a `KIND_DETACH` frame
    /// before the connection closes. After that window the I/O task is
    /// aborted (via Drop), the socket closes, and the daemon — having
    /// already seen the explicit detach — keeps the PTY alive.
    ///
    /// Differences from [`PaneController::close_pane`]:
    /// - `close_pane` issues `stop-agent` so the daemon SIGKILLs the child.
    /// - `detach_pane` issues `KIND_DETACH` so the daemon does *not*.
    ///
    /// For PTY-backed panes this is a no-op (the PTY is owned by this
    /// process; "leaving it running" outside this process is meaningless),
    /// and an unknown `pane_id` is a soft error so callers iterating across
    /// all panes don't have to filter first.
    pub fn detach_pane(&self, pane_id: &str) -> Result<(), PaneError> {
        let pane = {
            let mut panes = self.panes.lock().unwrap();
            match panes.remove(pane_id) {
                Some(p) => p,
                None => {
                    return Err(PaneError::CommandFailed(format!(
                        "Pane {pane_id} not found"
                    )));
                }
            }
        };
        match pane.backend {
            PaneBackend::Pty(_) => {
                // Local PTYs can't survive process exit. Treat detach as a
                // no-op: don't kill the child here (close_pane already
                // covers that), but don't silently leave the pane in the
                // registry either — the caller is detaching everything in
                // preparation for quit, and re-inserting would break that
                // invariant. Restoring the pane is wrong; dropping it kills
                // the child via Drop, which matches "we're about to exit."
                let _ = pane;
                Ok(())
            }
            PaneBackend::Stream(mut s) => {
                // Surface a closed channel as `CommandFailed` so callers
                // (e.g. `detach_all_streams`) can include it in their
                // per-pane error list. Survival is preserved either way:
                // if the writer task already exited, the socket has
                // already closed and the daemon has already observed EOF
                // (implicit detach). The error is purely observability —
                // the user should know the explicit signal didn't reach
                // the wire.
                if s.input_tx.send(StreamCmd::Detach).is_err() {
                    return Err(PaneError::CommandFailed(format!(
                        "Pane {pane_id} stream I/O task ended"
                    )));
                }
                if let Some(handle) = s.io_task.take() {
                    // Hand the runtime a brief window to drain the queued
                    // `Detach` and put the `KIND_DETACH` frame on the wire
                    // before the socket goes away. Bound the wait at
                    // 200ms — generous for a 5-byte frame on a local
                    // socket. On timeout `tokio::time::timeout` drops the
                    // wrapped JoinHandle, which only *detaches* the task;
                    // it does not cancel it. So we capture an
                    // `AbortHandle` first and call `.abort()`
                    // unconditionally afterward to terminate the writer
                    // deterministically. `abort()` on a finished task is
                    // a no-op, so this is safe regardless of which branch
                    // (timeout vs. completion) fired.
                    let abort = handle.abort_handle();
                    let _ = s.runtime.block_on(async move {
                        tokio::time::timeout(Duration::from_millis(200), handle).await
                    });
                    abort.abort();
                }
                // `s` drops here → channel sender drops. The socket halves
                // owned by the (now-aborted) task will be dropped on the
                // next runtime tick.
                Ok(())
            }
        }
    }

    /// Detach every stream-backed pane. Used by the M2.5 "Detach (leave
    /// agents running)" option in the quit dialog: a single keystroke
    /// signals voluntary detach for all remote agents before the TUI
    /// exits. Returns the list of `(pane_id, error)` pairs for any panes
    /// that failed to detach — the caller can decide whether to surface
    /// them; a non-empty result does not block the quit.
    pub fn detach_all_streams(&self) -> Vec<(String, PaneError)> {
        let stream_ids: Vec<String> = {
            let panes = self.panes.lock().unwrap();
            panes
                .iter()
                .filter(|(_, p)| matches!(p.backend, PaneBackend::Stream(_)))
                .map(|(id, _)| id.clone())
                .collect()
        };
        let mut errors = Vec::new();
        for id in stream_ids {
            if let Err(e) = self.detach_pane(&id) {
                errors.push((id, e));
            }
        }
        errors
    }
}

/// Bounded wait for an in-flight daemon resize call. Two seconds is far
/// longer than a healthy local Unix-socket round-trip for a single Resize op
/// but short enough that a wedged daemon can't park the worker indefinitely.
/// On timeout the underlying `DaemonClient` connection drops, releasing the
/// FD and any per-connection daemon-side task.
const RESIZE_DAEMON_TIMEOUT: Duration = Duration::from_secs(2);

/// Per-pane resize worker (PRD #76 M2.10 audit follow-up). Reads the most
/// recent `(rows, cols)` from the watch receiver and dispatches it to the
/// daemon with [`RESIZE_DAEMON_TIMEOUT`]. While a dispatch is in flight,
/// `resize_pane_pty` calls keep overwriting the watch value; the worker
/// re-reads via `borrow_and_update` after each dispatch so only the latest
/// size reaches the wire. Exits when `resize_tx` drops (`changed()` returns
/// `Err`) — the watch sender is owned by `StreamBackend`, so this happens
/// exactly when the pane is dropped.
async fn resize_worker(
    mut rx: tokio::sync::watch::Receiver<Option<(u16, u16)>>,
    daemon_path: PathBuf,
    agent_id: String,
) {
    // Mark the initial `None` value as seen so the first `changed()` call
    // waits for an actual resize, not the channel's seed value.
    let _ = rx.borrow_and_update();
    while rx.changed().await.is_ok() {
        let dims = *rx.borrow_and_update();
        let Some((rows, cols)) = dims else { continue };

        let client = DaemonClient::new(daemon_path.clone());
        match tokio::time::timeout(
            RESIZE_DAEMON_TIMEOUT,
            client.resize_agent(&agent_id, rows, cols),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::debug!(
                    agent_id = %agent_id,
                    rows, cols,
                    error = %e,
                    "resize-agent failed (transient — next resize will reconcile)"
                );
            }
            Err(_) => {
                tracing::debug!(
                    agent_id = %agent_id,
                    rows, cols,
                    timeout_ms = RESIZE_DAEMON_TIMEOUT.as_millis() as u64,
                    "resize-agent timed out (transient — next resize will reconcile)"
                );
            }
        }
    }
}

/// Write `payload` to whichever backend a pane uses. Pulled out as a free
/// helper so `write_to_pane` can dispatch on `&mut PaneBackend` from inside
/// the `panes` mutex without a closure that re-locks it.
fn write_payload_to_backend(
    backend: &mut PaneBackend,
    payload: &[u8],
    pane_id: &str,
) -> Result<(), PaneError> {
    match backend {
        PaneBackend::Pty(p) => {
            p.writer.write_all(payload).map_err(PaneError::Io)?;
            p.writer.flush().map_err(PaneError::Io)?;
        }
        PaneBackend::Stream(s) => {
            if s.input_tx.send(StreamCmd::Input(payload.to_vec())).is_err() {
                return Err(PaneError::CommandFailed(format!(
                    "Pane {pane_id} stream I/O task ended"
                )));
            }
        }
    }
    Ok(())
}

/// Scan PTY output bytes for mouse mode enable/disable escape sequences.
/// Sets the atomic flag when the child app requests mouse reporting.
fn scan_mouse_mode(data: &[u8], flag: &AtomicBool) {
    // Mouse mode sequences: \x1b[?{mode}h (enable) or \x1b[?{mode}l (disable)
    // Modes: 1000 (basic), 1002 (button-motion), 1003 (any-motion), 1006 (SGR extended)
    let enable_patterns: &[&[u8]] = &[
        b"\x1b[?1000h",
        b"\x1b[?1002h",
        b"\x1b[?1003h",
        b"\x1b[?1006h",
    ];
    let disable_patterns: &[&[u8]] = &[
        b"\x1b[?1000l",
        b"\x1b[?1002l",
        b"\x1b[?1003l",
        b"\x1b[?1006l",
    ];
    for pat in enable_patterns {
        if contains_bytes(data, pat) {
            flag.store(true, Ordering::Relaxed);
            return;
        }
    }
    for pat in disable_patterns {
        if contains_bytes(data, pat) {
            flag.store(false, Ordering::Relaxed);
            return;
        }
    }
}

/// Simple byte pattern search.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Feed a chunk of agent-output bytes through the OSC 8 filter, the vt100
/// parser, and the hyperlink map. Shared between the local-PTY reader thread
/// and the stream-backed I/O task so both backends produce identical render
/// state from identical bytes.
fn process_agent_output_chunk(
    data: &[u8],
    osc8: &mut Osc8Filter,
    parser: &Mutex<vt100::Parser>,
    mouse_mode: &AtomicBool,
    hyperlinks: &Mutex<HyperlinkMap>,
) {
    scan_mouse_mode(data, mouse_mode);

    let segments = osc8.process(data);
    let mut new_links: Vec<(u16, String)> = Vec::new();
    let mut scroll_amount: u16 = 0;

    if let Ok(mut p) = parser.lock() {
        let max_row = p.screen().size().0.saturating_sub(1);
        for segment in &segments {
            match segment {
                Osc8Segment::Text(bytes) => {
                    let rb = p.screen().cursor_position().0;
                    p.process(bytes);
                    let ra = p.screen().cursor_position().0;
                    if rb >= max_row && ra >= max_row {
                        let nl = bytes.iter().filter(|&&b| b == b'\n').count() as u16;
                        scroll_amount += nl;
                    }
                }
                Osc8Segment::LinkedText { url, bytes } => {
                    let row = p.screen().cursor_position().0;
                    let rb = row;
                    p.process(bytes);
                    let ra = p.screen().cursor_position().0;
                    new_links.push((row, url.clone()));
                    if rb >= max_row && ra >= max_row {
                        let nl = bytes.iter().filter(|&&b| b == b'\n').count() as u16;
                        scroll_amount += nl;
                    }
                }
            }
        }
    }

    if (!new_links.is_empty() || scroll_amount > 0)
        && let Ok(mut hmap) = hyperlinks.lock()
    {
        if scroll_amount > 0 {
            hmap.shift_up(scroll_amount);
        }
        for (row, url) in &new_links {
            hmap.set_row(*row, url);
        }
    }
}

impl PaneController for EmbeddedPaneController {
    fn focus_pane(&self, pane_id: &str) -> Result<(), PaneError> {
        let mut panes = self.panes.lock().unwrap();
        if !panes.contains_key(pane_id) {
            return Err(PaneError::CommandFailed(format!(
                "Pane {pane_id} not found"
            )));
        }
        for (id, pane) in panes.iter_mut() {
            pane.is_focused = id == pane_id;
        }
        Ok(())
    }

    fn create_pane(&self, command: Option<&str>, cwd: Option<&str>) -> Result<String, PaneError> {
        // The pane ID is allocated up front because it has to be injected into
        // the child's environment as DOT_AGENT_DECK_PANE_ID. If the spawn
        // below fails, the ID is intentionally consumed (a gap in the
        // sequence is harmless and avoids racing concurrent `create_pane`
        // calls to revert the counter).
        let pane_id = self.allocate_id();

        match &self.mode {
            ControllerMode::LocalDeck => self.create_local_pane(pane_id, command, cwd),
            ControllerMode::RemoteDeckLocal { client, runtime } => {
                self.create_stream_pane(pane_id, command, cwd, client.clone(), runtime.clone())
            }
        }
    }

    fn close_pane(&self, pane_id: &str) -> Result<(), PaneError> {
        // Hold the registry lock across the whole close so a stop-agent
        // failure can leave the pane in place for the user to retry. The
        // backend teardown (blocking child reap or async stop-agent) is
        // performed without the lock by detaching the pane first and only
        // re-inserting on the failure path.
        let pane = {
            let mut panes = self.panes.lock().unwrap();
            match panes.remove(pane_id) {
                Some(p) => p,
                None => {
                    return Err(PaneError::CommandFailed(format!(
                        "Pane {pane_id} not found"
                    )));
                }
            }
        };
        match pane.backend {
            PaneBackend::Pty(mut p) => {
                let _ = p.child.kill();
                let _ = p.child.wait();
                Ok(())
            }
            PaneBackend::Stream(s) => {
                // Ctrl+W on a stream-backed pane is the explicit
                // "kill the agent" path per PRD #76 line 220 — it must
                // send `stop-agent` over the protocol so the daemon
                // SIGKILLs the underlying child. Plain TUI exit takes a
                // different path: panes are dropped, `StreamBackend::drop`
                // aborts the I/O task, and the daemon sees the closed
                // socket as implicit detach. Order here matters: send
                // `stop-agent` first (over a fresh connection), then let
                // the drop abort the I/O task. If we aborted first, the
                // daemon would treat the dropped attach connection as a
                // detach and the agent would survive.
                let client = DaemonClient::new(s.daemon_path.clone());
                let agent_id = s.agent_id.clone();
                match s.runtime.block_on(client.stop_agent(&agent_id)) {
                    Ok(()) => {
                        // Drop `s` → io_task aborts. No explicit abort needed.
                        Ok(())
                    }
                    Err(e) => {
                        // Don't silently degrade to detach: a swallowed
                        // stop-agent error would close the socket, the
                        // daemon would treat the close as implicit detach,
                        // and the agent would survive on the remote with
                        // no signal to the user. Re-insert the pane so a
                        // retry remains possible (the io_task is still
                        // alive at this point — `s` has not been dropped).
                        tracing::error!(
                            agent_id = %agent_id,
                            error = %e,
                            "stop-agent failed during Ctrl+W close — pane retained for retry"
                        );
                        let restored = Pane {
                            backend: PaneBackend::Stream(s),
                            screen: pane.screen,
                            name: pane.name,
                            is_focused: pane.is_focused,
                            command: pane.command,
                            mouse_mode: pane.mouse_mode,
                            hyperlinks: pane.hyperlinks,
                        };
                        self.panes
                            .lock()
                            .unwrap()
                            .insert(pane_id.to_string(), restored);
                        Err(PaneError::CommandFailed(format!(
                            "stop-agent failed for pane {pane_id}: {e}"
                        )))
                    }
                }
            }
        }
    }

    fn list_panes(&self) -> Result<Vec<PaneInfo>, PaneError> {
        let panes = self.panes.lock().unwrap();
        let mut list: Vec<(u64, PaneInfo)> = panes
            .iter()
            .map(|(id, p)| {
                (
                    id.parse::<u64>().unwrap_or(0),
                    PaneInfo {
                        pane_id: id.clone(),
                        title: p.name.clone(),
                        is_focused: p.is_focused,
                        command: p.command.clone(),
                    },
                )
            })
            .collect();
        list.sort_by_key(|(num, _)| *num);
        Ok(list.into_iter().map(|(_, info)| info).collect())
    }

    fn resize_pane(
        &self,
        _pane_id: &str,
        _direction: PaneDirection,
        _amount: u16,
    ) -> Result<(), PaneError> {
        // Resize is handled by the layout engine in future milestones.
        // For now, this is a no-op.
        Ok(())
    }

    fn rename_pane(&self, pane_id: &str, name: &str) -> Result<(), PaneError> {
        let mut panes = self.panes.lock().unwrap();
        if let Some(pane) = panes.get_mut(pane_id) {
            pane.name = name.to_string();
            Ok(())
        } else {
            Err(PaneError::CommandFailed(format!(
                "Pane {pane_id} not found"
            )))
        }
    }

    fn toggle_layout(&self) -> Result<(), PaneError> {
        // Layout toggling will be implemented in the layout engine milestone.
        Ok(())
    }

    /// Concurrency contract: callers must not invoke `write_to_pane` concurrently
    /// for the same `pane_id`. The pane lock is released around `SUBMIT_DELAY` so
    /// other panes can be drawn — but interleaved writes for the *same* pane would
    /// produce `payload_A + payload_B + CR + CR`, fusing two prompts. The current
    /// architecture is single-threaded for pane I/O, so this is a latent constraint
    /// rather than an active hazard; a per-pane submit mutex would enforce it if
    /// concurrent callers are ever introduced.
    fn write_to_pane(&self, pane_id: &str, text: &str) -> Result<(), PaneError> {
        let payload = encode_pane_payload(text);
        // Write the payload (content, optionally bracketed-paste-wrapped), flush, then
        // pause briefly before sending the submit CR. Agent TUIs like claude treat a
        // CR that arrives fused to the preceding text as newline-in-input; only a CR
        // that arrives as a separate event after a pause is honored as Enter. The
        // pane lock is released during the sleep so the UI thread can keep drawing.
        {
            let mut panes = self.panes.lock().unwrap();
            let pane = panes
                .get_mut(pane_id)
                .ok_or_else(|| PaneError::CommandFailed(format!("Pane {pane_id} not found")))?;
            write_payload_to_backend(&mut pane.backend, &payload, pane_id)?;
        }
        std::thread::sleep(SUBMIT_DELAY);
        {
            let mut panes = self.panes.lock().unwrap();
            let pane = panes
                .get_mut(pane_id)
                .ok_or_else(|| PaneError::CommandFailed(format!("Pane {pane_id} not found")))?;
            write_payload_to_backend(&mut pane.backend, b"\r", pane_id)?;
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "embedded"
    }

    fn is_available(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_list_panes() {
        let ctrl = EmbeddedPaneController::new();
        assert!(ctrl.list_panes().unwrap().is_empty());

        let id = ctrl.create_pane(None, None).unwrap();
        assert!(!id.is_empty());

        let panes = ctrl.list_panes().unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, id);

        ctrl.close_pane(&id).unwrap();
        assert!(ctrl.list_panes().unwrap().is_empty());
    }

    #[test]
    fn focus_pane_updates_state() {
        let ctrl = EmbeddedPaneController::new();
        let id1 = ctrl.create_pane(None, None).unwrap();
        let id2 = ctrl.create_pane(None, None).unwrap();

        ctrl.focus_pane(&id1).unwrap();
        let panes = ctrl.list_panes().unwrap();
        assert!(panes.iter().find(|p| p.pane_id == id1).unwrap().is_focused);
        assert!(!panes.iter().find(|p| p.pane_id == id2).unwrap().is_focused);

        ctrl.focus_pane(&id2).unwrap();
        let panes = ctrl.list_panes().unwrap();
        assert!(!panes.iter().find(|p| p.pane_id == id1).unwrap().is_focused);
        assert!(panes.iter().find(|p| p.pane_id == id2).unwrap().is_focused);

        ctrl.close_pane(&id1).unwrap();
        ctrl.close_pane(&id2).unwrap();
    }

    #[test]
    fn rename_pane_works() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane(None, None).unwrap();

        ctrl.rename_pane(&id, "my-agent").unwrap();
        let panes = ctrl.list_panes().unwrap();
        assert_eq!(panes[0].title, "my-agent");

        ctrl.close_pane(&id).unwrap();
    }

    #[test]
    fn close_nonexistent_pane_errors() {
        let ctrl = EmbeddedPaneController::new();
        assert!(ctrl.close_pane("999").is_err());
    }

    #[test]
    fn focus_nonexistent_pane_errors() {
        let ctrl = EmbeddedPaneController::new();
        assert!(ctrl.focus_pane("999").is_err());
    }

    #[test]
    fn write_to_pane_works() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane(None, None).unwrap();

        // Should not error — just sends bytes to PTY stdin
        ctrl.write_to_pane(&id, "echo hello").unwrap();

        ctrl.close_pane(&id).unwrap();
    }

    #[test]
    fn encode_pane_payload_single_line() {
        assert_eq!(encode_pane_payload("ls -la"), b"ls -la");
    }

    #[test]
    fn encode_pane_payload_strips_trailing_whitespace() {
        assert_eq!(encode_pane_payload("ls -la\n"), b"ls -la");
        assert_eq!(encode_pane_payload("ls -la  \n\n"), b"ls -la");
    }

    #[test]
    fn encode_pane_payload_wraps_multiline() {
        assert_eq!(
            encode_pane_payload("line1\nline2\nline3"),
            b"\x1b[200~line1\nline2\nline3\x1b[201~"
        );
    }

    #[test]
    fn encode_pane_payload_multiline_with_trailing_newline() {
        // Trailing newline is stripped, but embedded newlines still trigger paste wrapping.
        assert_eq!(
            encode_pane_payload("line1\nline2\n"),
            b"\x1b[200~line1\nline2\x1b[201~"
        );
    }

    #[test]
    fn encode_pane_payload_empty() {
        assert_eq!(encode_pane_payload(""), b"");
        // Edge case: trailing whitespace stripped to empty → no embedded newline → no markers.
        assert_eq!(encode_pane_payload("\n\n"), b"");
    }

    #[test]
    fn controller_metadata() {
        let ctrl = EmbeddedPaneController::new();
        assert_eq!(ctrl.name(), "embedded");
        assert!(ctrl.is_available());
    }

    #[test]
    fn screen_access_works() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane(Some("echo hello"), None).unwrap();

        // Give the PTY a moment to produce output
        std::thread::sleep(std::time::Duration::from_millis(200));

        let screen = ctrl.get_screen(&id).expect("screen should exist");
        let parser = screen.lock().unwrap();
        let contents = parser.screen().contents();
        // The screen should have some content (at minimum the echoed text or shell prompt)
        assert!(!contents.trim().is_empty());

        ctrl.close_pane(&id).unwrap();
    }

    #[test]
    fn pane_ids_are_sequential() {
        let ctrl = EmbeddedPaneController::new();
        let id1 = ctrl.create_pane(None, None).unwrap();
        let id2 = ctrl.create_pane(None, None).unwrap();
        let id3 = ctrl.create_pane(None, None).unwrap();

        let n1: u64 = id1.parse().unwrap();
        let n2: u64 = id2.parse().unwrap();
        let n3: u64 = id3.parse().unwrap();
        assert_eq!(n2, n1 + 1);
        assert_eq!(n3, n2 + 1);

        ctrl.close_pane(&id1).unwrap();
        ctrl.close_pane(&id2).unwrap();
        ctrl.close_pane(&id3).unwrap();
    }

    #[test]
    fn pane_ids_sorted_in_list() {
        let ctrl = EmbeddedPaneController::new();
        let id1 = ctrl.create_pane(None, None).unwrap();
        let id2 = ctrl.create_pane(None, None).unwrap();
        let id3 = ctrl.create_pane(None, None).unwrap();

        let ids = ctrl.pane_ids();
        assert_eq!(ids, vec![id1.clone(), id2.clone(), id3.clone()]);

        ctrl.close_pane(&id1).unwrap();
        ctrl.close_pane(&id2).unwrap();
        ctrl.close_pane(&id3).unwrap();
    }

    #[test]
    fn focused_pane_id_tracks_focus() {
        let ctrl = EmbeddedPaneController::new();
        assert!(ctrl.focused_pane_id().is_none());

        let id1 = ctrl.create_pane(None, None).unwrap();
        let id2 = ctrl.create_pane(None, None).unwrap();

        ctrl.focus_pane(&id1).unwrap();
        assert_eq!(ctrl.focused_pane_id().as_deref(), Some(id1.as_str()));

        ctrl.focus_pane(&id2).unwrap();
        assert_eq!(ctrl.focused_pane_id().as_deref(), Some(id2.as_str()));

        ctrl.close_pane(&id1).unwrap();
        ctrl.close_pane(&id2).unwrap();
    }

    #[test]
    fn write_raw_bytes_no_cr_appended() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane(None, None).unwrap();

        // write_raw_bytes should succeed without error
        ctrl.write_raw_bytes(&id, b"hello").unwrap();

        ctrl.close_pane(&id).unwrap();
    }

    #[test]
    fn write_raw_bytes_nonexistent_pane_errors() {
        let ctrl = EmbeddedPaneController::new();
        assert!(ctrl.write_raw_bytes("999", b"hello").is_err());
    }

    #[test]
    fn rename_nonexistent_pane_errors() {
        let ctrl = EmbeddedPaneController::new();
        assert!(ctrl.rename_pane("999", "name").is_err());
    }

    #[test]
    fn create_pane_with_command() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane(Some("echo test"), None).unwrap();

        let panes = ctrl.list_panes().unwrap();
        assert_eq!(panes[0].title, "echo test");
        assert_eq!(panes[0].command.as_deref(), Some("echo test"));

        ctrl.close_pane(&id).unwrap();
    }

    #[test]
    fn create_pane_default_name_is_shell() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane(None, None).unwrap();

        let panes = ctrl.list_panes().unwrap();
        assert_eq!(panes[0].title, "shell");
        assert!(panes[0].command.is_none());

        ctrl.close_pane(&id).unwrap();
    }
}
