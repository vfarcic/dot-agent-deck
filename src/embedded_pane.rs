use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use std::any::Any;

use crate::agent_pty::{self, DOT_AGENT_DECK_PANE_ID, PTY_RESIZE_DIM_MAX, TabMembership};
use crate::daemon_client::{AttachConnection, DaemonClient, StartAgentOptions};
use crate::event::AgentType;
use crate::hyperlink::{HyperlinkMap, Osc8Filter, Osc8Segment};
use crate::pane::{
    AgentSpawnOptions, PaneController, PaneDirection, PaneError, PaneInfo, RenameOutcome,
};

/// Result of [`EmbeddedPaneController::hydrate_from_daemon`]. One entry per
/// daemon-side agent that was successfully reconnected on TUI bootstrap; the
/// caller uses the pair to register the pane with [`crate::state::AppState`]
/// and seed the UI's display-name maps. Agents that fail to attach (e.g.
/// terminated between list and attach) are not represented here.
#[derive(Debug, Clone)]
pub struct HydratedPane {
    /// Local pane id assigned by the controller.
    pub pane_id: String,
    /// Daemon-side agent id this pane is attached to.
    pub agent_id: String,
    /// Display name as last stored on the daemon (M2.11). `None` means
    /// either the agent was started without a name or the daemon ran an
    /// older binary that didn't persist it. Callers fall back to
    /// `agent_id` in that case.
    pub display_name: Option<String>,
    /// Working directory captured at spawn time on the daemon (M2.11).
    /// `None` mirrors the same forward-compat reasoning as `display_name`.
    pub cwd: Option<String>,
    /// Which tab the agent belonged to at spawn time (PRD #76 M2.12).
    /// Drives the hydration partition in `ui.rs`: `None` → dashboard,
    /// `Some(Mode { ... })` → mode tab rebuild, `Some(Orchestration {
    /// ... })` → orchestration tab rebuild. `None` is also the
    /// older-daemon fallback (the field is omitted from the wire shape
    /// via `skip_serializing_if`), which keeps every legacy agent on
    /// the dashboard — same behavior as before M2.12.
    pub tab_membership: Option<TabMembership>,
    /// Which AI agent the daemon recorded for this pane at spawn time
    /// (PRD #76 M2.13). Threaded into `insert_placeholder_session` so
    /// the hydrated session's `agent_type` reflects the daemon's known
    /// value instead of defaulting to `AgentType::None` (which the
    /// dashboard renders as "No agent"). `None` means either the daemon
    /// is older / didn't persist the field, or the spawn command wasn't
    /// recognized as an agent by [`AgentType::from_command`].
    pub agent_type: Option<AgentType>,
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

/// Backing state for a single pane: the PTY lives in the daemon, and this
/// side owns one [`crate::daemon_client::AttachConnection`]. Bytes flow
/// daemon → STREAM_OUT → vt100 parser; keystrokes flow vt100 → input
/// channel → STREAM_IN → daemon. The daemon-side agent outlives the TUI
/// by design (PRD #76 line 199), so dropping this struct is implicit
/// detach (the `io_task` stops draining and the socket closes — the
/// daemon's input loop treats EOF as DETACH). Sending `stop-agent` is
/// reserved for the explicit user-driven Ctrl+W close path in
/// `close_pane`.
///
/// PRD #93 Phase 2: an in-process variant of this backend used to sit
/// next to it to host local-mode PTY children. It's deleted now — the
/// daemon owns every agent regardless of whether the user invoked the
/// deck locally or over `dot-agent-deck connect`. `Pane.backend` is
/// just a `StreamBackend`.
struct StreamBackend {
    /// Daemon-side agent id used for `stop-agent` on close and
    /// `resize-agent` from the per-pane resize worker. Shared with the
    /// per-pane I/O task so that PRD #92 F12's auto-renew-on-respawn path
    /// can swap in the NEW agent's id after the daemon respawns the agent
    /// behind this pane (clear=true delegate flow). All readers take a
    /// brief lock + clone before issuing the RPC — never held across
    /// `.await`.
    agent_id: Arc<Mutex<String>>,
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
    /// Connection to the daemon-managed agent the pane is attached to.
    backend: StreamBackend,
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
    /// Working directory recorded at spawn time (M2.11). Cached here so the
    /// rename flow can re-send it alongside the new display_name in
    /// `set_agent_label` — the daemon-side API uses `None to clear`
    /// semantics, so callers that want to update one field must echo
    /// the other.
    cwd: Option<String>,
    /// Whether the child app has enabled mouse reporting (e.g., TUI apps like opencode).
    mouse_mode: Arc<AtomicBool>,
    /// Hyperlink URLs extracted from OSC 8 escape sequences, keyed by screen row.
    hyperlinks: Arc<Mutex<HyperlinkMap>>,
}

/// Thread-safe pane registry.
type PaneRegistry = Arc<Mutex<HashMap<String, Pane>>>;

/// Resolve the (rows, cols) the local vt100 parser should be initialised
/// at on hydration (PRD #104 M2).
///
/// The daemon now echoes its current PTY dims via `AgentRecord.rows/cols`.
/// Three cases need handling:
///
/// - **Sane dims** (`1..=PTY_RESIZE_DIM_MAX`): use them. This is the
///   normal new-daemon path — snapshot bytes parse at the dims they
///   were written at.
/// - **Zero** (`0, 0`): the daemon predates this PRD and doesn't carry
///   the field on the wire. Fall back to the historical 24×80
///   placeholder; the post-hydration resize sweep in `ui.rs` lands
///   the real viewport dims a frame later.
/// - **Out of range** (e.g. `> PTY_RESIZE_DIM_MAX`): a daemon-side bug
///   or hostile peer sending nonsense. Same fall-back as the zero
///   case — vt100 has subtle edge cases at zero / huge sizes and a
///   panic in the parser would take down the whole TUI hydration
///   path, so we refuse to construct one with those values.
///
/// In all fall-back cases we emit a single debug log so the case is
/// observable in operation without spamming every hydration call.
///
/// Public so the PRD #104 M4 reproducer (`tests/snapshot_replay_dims.rs`)
/// can pin the contract end to end without spinning up a daemon: the
/// test reads the same dims this function resolves to and constructs a
/// `vt100::Parser` at the same geometry the hydration path would.
pub fn parser_init_dims(rows: u16, cols: u16) -> (u16, u16) {
    let in_range = |v: u16| (1..=PTY_RESIZE_DIM_MAX).contains(&v);
    if in_range(rows) && in_range(cols) {
        return (rows, cols);
    }
    // PRD #104 RN3/AN1 (reviewer / auditor nit): one debug emission for
    // both fall-back branches — the original (rows, cols) pair is the
    // useful diagnostic regardless of which axis tripped the guard, and
    // the `reason` tag distinguishes the legacy-daemon case from the
    // out-of-range case without duplicating the message body.
    let reason = if rows == 0 && cols == 0 {
        "legacy-daemon-zero"
    } else {
        "out-of-range"
    };
    tracing::debug!(
        rows,
        cols,
        reason,
        "hydrate_from_daemon: daemon-supplied PTY dims unusable — falling back to 24×80 parser init"
    );
    (24, 80)
}

use crate::pane_input::{SUBMIT_DELAY, encode_pane_payload};

/// Embedded terminal pane controller. Spawns agents on the daemon at
/// [`Self::client`]'s socket path and renders their PTY output through a
/// local vt100 parser. PRD #93 Phase 2 collapsed the historical
/// dual-mode design (local in-process PTY + remote-deck attach) into the
/// single attach-protocol path — every pane is daemon-backed.
pub struct EmbeddedPaneController {
    panes: PaneRegistry,
    next_id: Arc<Mutex<u64>>,
    /// Daemon RPC client used by `create_pane`, `close_pane`,
    /// `hydrate_from_daemon`, and `rename_pane`. Carrying it on the
    /// controller (rather than reconstructing per call) lets the
    /// existing `block_on` paths reuse the same socket address resolution
    /// logic.
    client: DaemonClient,
    /// Tokio runtime handle used to drive the blocking `block_on` calls
    /// from the TUI's blocking render thread, plus the long-lived
    /// per-pane I/O and resize worker tasks.
    runtime: tokio::runtime::Handle,
}

impl EmbeddedPaneController {
    /// Build a controller whose panes are stream-backed against the daemon
    /// at `socket_path`. Caller is responsible for ensuring the daemon is
    /// actually running — `daemon_attach::ensure_external_daemon_or_die`
    /// is the canonical pre-flight from `main`.
    pub fn new(socket_path: PathBuf, runtime: tokio::runtime::Handle) -> Self {
        Self {
            panes: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(Mutex::new(1)),
            client: DaemonClient::new(socket_path),
            runtime,
        }
    }

    /// Test-only constructor for code paths that need a `PaneController`
    /// value but never actually exercise pane I/O — e.g. render-frame
    /// tests that build an empty controller just to satisfy a function
    /// signature. The daemon socket path is a tempdir placeholder; any
    /// attempt to spawn or attach against it will fail.
    #[cfg(test)]
    pub fn for_render_only_tests() -> Self {
        use std::sync::OnceLock;
        static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
        let rt = RT.get_or_init(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime")
        });
        let mut placeholder = std::env::temp_dir();
        placeholder.push(format!(
            "dot-agent-deck-render-only-{}.sock",
            std::process::id()
        ));
        Self::new(placeholder, rt.handle().clone())
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
            if pane
                .backend
                .input_tx
                .send(StreamCmd::Input(bytes.to_vec()))
                .is_err()
            {
                return Err(PaneError::CommandFailed(format!(
                    "Pane {pane_id} stream I/O task ended"
                )));
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
        // `send_replace` overwrites whatever value was pending and ignores
        // the no-receivers case (the worker would only be gone if the
        // pane was being torn down — losing the resize is the right
        // outcome there). The watch channel cannot block, so this returns
        // immediately and never holds the pane lock across daemon I/O.
        let _ = pane.backend.resize_tx.send_replace(Some((rows, cols)));
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

    /// Enqueue `payload` for the pane's I/O task to forward as one
    /// `KIND_STREAM_IN` frame. Held under the `panes` mutex only long
    /// enough to look up the sender — the actual write happens on the
    /// I/O task. A closed channel means the I/O task has already exited
    /// (e.g. socket close); surface that as `CommandFailed` so callers
    /// can decide whether to retry.
    fn queue_stream_input(&self, pane_id: &str, payload: Vec<u8>) -> Result<(), PaneError> {
        let panes = self.panes.lock().unwrap();
        let pane = panes
            .get(pane_id)
            .ok_or_else(|| PaneError::CommandFailed(format!("Pane {pane_id} not found")))?;
        pane.backend
            .input_tx
            .send(StreamCmd::Input(payload))
            .map_err(|_| PaneError::CommandFailed(format!("Pane {pane_id} stream I/O task ended")))
    }

    /// Build a stream-backed pane against the daemon. The PTY lives in
    /// the daemon; this side holds an
    /// [`crate::daemon_client::AttachConnection`] and feeds the shared
    /// vt100 parser from STREAM_OUT bytes.
    #[allow(clippy::too_many_arguments)]
    fn create_stream_pane(
        &self,
        pane_id: String,
        command: Option<&str>,
        cwd: Option<&str>,
        display_name: &str,
        tab_membership: Option<TabMembership>,
        agent_type: Option<AgentType>,
        rows: u16,
        cols: u16,
    ) -> Result<String, PaneError> {
        // Tag the spawned process so daemon-spawned agents see
        // DOT_AGENT_DECK_PANE_ID and can emit hook events back to this
        // UI's pane.
        let env = vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.clone())];

        // Already resolved by `create_pane_with_display_name` (single
        // source of truth via `resolve_display_name`). Sending it as-is
        // keeps the StartAgent payload identical to the local Pane.name
        // and the UI maps — fixing the divergence M2.11 fixup-3 reviewer
        // P2 and auditor LOW called out.
        let label = display_name.to_string();
        let client = self.client.clone();
        let runtime = self.runtime.clone();

        let opts = StartAgentOptions {
            command: command.map(|c| c.to_string()),
            cwd: cwd.map(|c| c.to_string()),
            display_name: Some(label.clone()),
            // PRD #76 M2.15: forward the TUI's real viewport-derived dims
            // so the daemon opens its PTY at the eventual size. Older
            // daemons fall back to the serde defaults (24/80) via
            // `default_rows` / `default_cols`, so this is forward + backward
            // compatible without a wire-format change.
            rows,
            cols,
            env,
            tab_membership,
            agent_type,
        };

        // Start-agent + attach happen on the daemon's runtime; we
        // `block_on` here because `create_pane` is called from the TUI's
        // blocking thread.
        //
        // CodeRabbit Fix D: if `start_agent` succeeds the daemon has
        // already spawned a live PTY + session. A subsequent `attach`
        // failure would otherwise leak that session — the user never gets
        // a pane to close it through. Capture the agent id immediately
        // after start, and on attach error issue a best-effort
        // `stop_agent` before propagating the original attach failure.
        //
        // Fix D fixup (reviewer + auditor P3): each RPC inside the
        // `block_on` is wrapped in `tokio::time::timeout`. Without these
        // a wedged same-UID daemon could:
        //   * hang `start_agent` (no agent created, no cleanup needed —
        //     surface a TimedOut error),
        //   * answer `attach` with Err promptly then *never* respond to
        //     the cleanup `stop_agent`, pinning `create_stream_pane`
        //     forever on the cleanup await (auditor's specific concern),
        //   * hang `attach` itself, never reaching the cleanup branch.
        // Cleanup on `attach` error OR `attach` timeout is best-effort
        // and bounded by `CREATE_PANE_STOP_TIMEOUT`; the original attach
        // error (or synthesized timeout error) is what propagates.
        let client_for_calls = client.clone();
        let (agent_id, conn) = runtime
            .block_on(async move {
                use crate::daemon_client::ClientError;

                let id = match tokio::time::timeout(
                    CREATE_PANE_START_TIMEOUT,
                    client_for_calls.start_agent(opts),
                )
                .await
                {
                    Ok(Ok(id)) => id,
                    Ok(Err(e)) => return Err(e),
                    Err(_) => {
                        return Err(ClientError::Io(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            format!(
                                "start_agent timed out after {}ms",
                                CREATE_PANE_START_TIMEOUT.as_millis()
                            ),
                        )));
                    }
                };

                // Run attach with a timeout. On Ok(conn) we're done. On
                // Err OR timeout we fall through to the bounded cleanup
                // path below.
                let attach_err: ClientError = match tokio::time::timeout(
                    CREATE_PANE_ATTACH_TIMEOUT,
                    client_for_calls.attach(&id),
                )
                .await
                {
                    Ok(Ok(conn)) => return Ok::<_, ClientError>((id, conn)),
                    Ok(Err(e)) => e,
                    Err(_) => ClientError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!(
                            "attach timed out after {}ms",
                            CREATE_PANE_ATTACH_TIMEOUT.as_millis()
                        ),
                    )),
                };

                // Best-effort, bounded cleanup. On failure OR timeout we
                // log at warn (the daemon-side agent may be leaked) but
                // always propagate the ORIGINAL attach error so callers
                // see the real cause, not a cleanup-stage symptom.
                match tokio::time::timeout(
                    CREATE_PANE_STOP_TIMEOUT,
                    client_for_calls.stop_agent(&id),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(stop_err)) => tracing::warn!(
                        agent_id = %id,
                        error = %stop_err,
                        "create_stream_pane: stop_agent during attach-failure cleanup failed; daemon-side agent may be leaked"
                    ),
                    Err(_) => tracing::warn!(
                        agent_id = %id,
                        timeout_ms = CREATE_PANE_STOP_TIMEOUT.as_millis() as u64,
                        "create_stream_pane: stop_agent during attach-failure cleanup timed out; daemon-side agent may be leaked"
                    ),
                }

                Err(attach_err)
            })
            .map_err(|e| PaneError::CommandFailed(format!("daemon: {e}")))?;

        let name = label;
        let command = command.map(|c| c.to_string());
        let cwd_stored = cwd.map(|c| c.to_string());
        self.wire_stream_pane(
            pane_id.clone(),
            agent_id,
            conn,
            name,
            command,
            cwd_stored,
            rows,
            cols,
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
        cwd: Option<String>,
        rows: u16,
        cols: u16,
    ) {
        let daemon_path = self.client.socket_path().to_path_buf();
        let runtime = self.runtime.clone();
        // PRD #76 M2.15: size the local vt100 parser to match the dims the
        // daemon's PTY was opened at (spawn) or last resized to (hydration).
        // A 24×80 parser receiving an already-correctly-sized frame would
        // clip it; resize-time keeps both sides in sync via the per-pane
        // resize worker + `resize_pane_pty`.
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 10_000)));
        let mouse_mode = Arc::new(AtomicBool::new(false));
        let hyperlinks = Arc::new(Mutex::new(HyperlinkMap::new()));

        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<StreamCmd>();
        let (resize_tx, resize_rx) = tokio::sync::watch::channel::<Option<(u16, u16)>>(None);

        // PRD #92 F12: the per-pane subscriber and the resize worker both
        // need to follow `agent_id` across daemon-side respawns (F9
        // clear=true delegate flow kills + replaces the agent under the
        // same pane_id_env). Share one `Arc<Mutex<String>>` between them
        // and `StreamBackend` so a single update in the io_task is visible
        // to the next `stop-agent` / `resize-agent` call without rewiring.
        let shared_agent_id = Arc::new(Mutex::new(agent_id));

        // Per-pane resize worker: at-most-one in-flight daemon Resize per
        // pane, with intermediate values coalesced via the watch channel.
        // Survives until either `resize_tx` drops (pane removed) or the
        // worker is aborted by `StreamBackend::drop`. See the comment on
        // `resize_pane_pty` for the full rationale.
        let resize_task = runtime.spawn(resize_worker(
            resize_rx,
            daemon_path.clone(),
            Arc::clone(&shared_agent_id),
        ));

        let parser_for_task = Arc::clone(&parser);
        let mouse_mode_for_task = Arc::clone(&mouse_mode);
        let hyperlinks_for_task = Arc::clone(&hyperlinks);
        let agent_id_for_task = Arc::clone(&shared_agent_id);
        let client_for_task = self.client.clone();
        let pane_id_for_task = pane_id.clone();

        let io_task = runtime.spawn(run_pane_io_task(
            pane_id_for_task,
            client_for_task,
            conn,
            agent_id_for_task,
            input_rx,
            parser_for_task,
            mouse_mode_for_task,
            hyperlinks_for_task,
        ));

        let pane = Pane {
            backend: StreamBackend {
                agent_id: shared_agent_id,
                input_tx,
                io_task: Some(io_task),
                runtime,
                daemon_path,
                resize_tx,
                resize_task: Some(resize_task),
            },
            screen: parser,
            name,
            is_focused: false,
            command,
            cwd,
            mouse_mode,
            hyperlinks,
        };

        self.panes.lock().unwrap().insert(pane_id, pane);
    }

    /// Reconnect to every daemon-side agent on TUI bootstrap (PRD #76
    /// M2.x). The agents the user spawned in a previous session are
    /// still alive in the daemon; without this step the dashboard would
    /// show "No active sessions" even though the daemon owns live PTYs.
    ///
    /// For each id returned by `list_agents`, builds a fresh
    /// `StreamBackend` and opens an `AttachStream` (no `start-agent` —
    /// the agent already exists). The daemon replays its scrollback
    /// snapshot before live bytes, so hydrated panes render the agent's
    /// current screen on first paint.
    ///
    /// Errors are absorbed rather than propagated:
    /// - `list_agents` failure (transient daemon hiccup): logged at debug,
    ///   treated as empty. The user can retry by reconnecting.
    /// - Per-agent `attach` failure (race: the agent terminated between
    ///   list and attach): logged at debug, that agent is skipped, others
    ///   continue.
    ///
    /// Returns one [`HydratedPane`] per successfully attached agent, in
    /// the order returned by the daemon. Callers register each pane id
    /// with [`crate::state::AppState`] and seed the UI's display-name
    /// maps from `HydratedPane::display_name` (falling back to `agent_id`
    /// when the daemon has no recorded label — M2.11 added persistence,
    /// older daemons or unlabelled agents still come back as `None`).
    pub fn hydrate_from_daemon(&self) -> Vec<HydratedPane> {
        let client = self.client.clone();
        let runtime = self.runtime.clone();

        // Bounded list_agents call: a parked or hostile same-user daemon
        // could otherwise hang TUI startup on the blocking `block_on`. On
        // timeout we treat the result as empty (the user can reconnect)
        // and emit a debug line so the cause is observable.
        let list_client = client.clone();
        let records = match runtime.block_on(async move {
            tokio::time::timeout(HYDRATE_LIST_TIMEOUT, list_client.list_agents()).await
        }) {
            Ok(Ok(a)) => a,
            Ok(Err(e)) => {
                tracing::debug!(
                    error = %e,
                    "hydrate_from_daemon: list_agents failed, treating as empty"
                );
                return Vec::new();
            }
            Err(_) => {
                tracing::debug!(
                    timeout_ms = HYDRATE_LIST_TIMEOUT.as_millis() as u64,
                    "hydrate_from_daemon: list_agents timed out, treating as empty"
                );
                return Vec::new();
            }
        };

        // Cap fan-out so a misbehaving daemon advertising thousands of ids
        // can't make us open thousands of attach sockets in series. Normal
        // interactive workloads stay well under this — hitting the cap is
        // itself a signal worth logging.
        let mut records = records;
        if records.len() > HYDRATE_MAX_PANES {
            tracing::debug!(
                received = records.len(),
                cap = HYDRATE_MAX_PANES,
                "hydrate_from_daemon: agent list exceeded cap, truncating"
            );
            records.truncate(HYDRATE_MAX_PANES);
        }

        let mut hydrated = Vec::new();
        // Dedup pane ids within this batch (PRD #76 M2.x audit follow-up).
        // Tracks both reused-from-`pane_id_env` *and* fresh `allocate_id`
        // outputs so a duplicate `DOT_AGENT_DECK_PANE_ID` from a stale or
        // hostile daemon (or a value that happens to collide with an id
        // we already allocated this pass) cannot HashMap::insert-overwrite
        // an earlier pane in `wire_stream_pane`.
        let mut used_ids: HashSet<String> = HashSet::new();
        for record in records {
            let agent_id = record.id.clone();
            let client_for_attach = client.clone();
            let id_for_attach = agent_id.clone();
            // Bounded per-agent attach: same rationale as the list-agents
            // timeout above, scaled down because there can be up to
            // HYDRATE_MAX_PANES of these in series.
            let attach_result = runtime.block_on(async move {
                tokio::time::timeout(
                    HYDRATE_ATTACH_TIMEOUT,
                    client_for_attach.attach(&id_for_attach),
                )
                .await
            });
            let conn = match attach_result {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
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
                Err(_) => {
                    tracing::debug!(
                        agent_id = %agent_id,
                        timeout_ms = HYDRATE_ATTACH_TIMEOUT.as_millis() as u64,
                        "hydrate_from_daemon: attach timed out, skipping"
                    );
                    continue;
                }
            };
            // Reuse the daemon-captured `DOT_AGENT_DECK_PANE_ID` when
            // present so the TUI's local pane id matches whatever the
            // agent's child process already carries in its env. This is
            // what lets hook events (delegate / work-done / status)
            // emitted by the agent route correctly after a reconnect —
            // see `state::AppState::apply_event`'s managed-pane check.
            // Older daemons omit this field (`pane_id_env: None`), so we
            // fall back to allocating a fresh id; that path keeps the
            // pane visible and the byte stream rendered, but hook
            // routing won't survive reconnect — same behavior as before
            // this fix.
            //
            // Defense in depth (audit follow-up): re-validate the
            // daemon-supplied value here too, so an older daemon that
            // doesn't yet scrub at capture can't poison this client's
            // pane registry. Same grammar as the daemon-side check.
            let pane_id = match record.pane_id_env.clone() {
                Some(id) if agent_pty::is_valid_pane_id_env(&id) && !used_ids.contains(&id) => {
                    // Bump `next_id` past any reused pane id so a later
                    // `allocate_id` for a freshly-created pane can't
                    // collide with one we just rehydrated. Without this,
                    // the new pane's `insert` would silently replace the
                    // hydrated one in the HashMap.
                    if let Ok(parsed) = id.parse::<u64>() {
                        let mut nxt = self.next_id.lock().unwrap();
                        if parsed >= *nxt {
                            *nxt = parsed + 1;
                        }
                    }
                    id
                }
                Some(id) => {
                    tracing::debug!(
                        agent_id = %agent_id,
                        pane_id_env_len = id.len(),
                        "hydrate_from_daemon: pane_id_env invalid or duplicate, falling back to allocate_id"
                    );
                    self.allocate_id()
                }
                None => self.allocate_id(),
            };
            used_ids.insert(pane_id.clone());
            // M2.11: prefer the daemon-stored display_name when present,
            // falling back to agent_id when older daemons omit it. Pane
            // metadata (cwd) is also lifted from the record so the
            // dashboard's cwd column survives a reconnect.
            let display_name = record.display_name.clone();
            let cwd_record = record.cwd.clone();
            let pane_name = display_name.clone().unwrap_or_else(|| agent_id.clone());
            // PRD #104: the daemon now echoes its current PTY dims via
            // `AgentRecord.rows/cols`. Size the local vt100 parser at
            // those dims before the snapshot bytes stream through — a
            // parser sized at 24×80 receiving bytes emitted at, say,
            // 200×60 clamps cursor sequences to col 79 and inserts
            // spurious wraps at col 80, baking permanent corruption
            // into the parser's scrollback. The post-hydration resize
            // sweep in `ui.rs` continues to run unchanged; its role
            // shifts from "wrong dims → correct dims" to "daemon's
            // dims → local viewport dims".
            //
            // Fall back to the historical 24×80 placeholder when the
            // daemon predates this PRD (the field serdes as 0) or when
            // the supplied value is outside the registry's own resize
            // bounds — vt100 has subtle edge cases at zero / huge
            // sizes, and a debug log keeps the fall-back observable.
            let (parser_rows, parser_cols) = parser_init_dims(record.rows, record.cols);
            self.wire_stream_pane(
                pane_id.clone(),
                agent_id.clone(),
                conn,
                pane_name,
                None,
                cwd_record.clone(),
                parser_rows,
                parser_cols,
            );
            hydrated.push(HydratedPane {
                pane_id,
                agent_id,
                display_name,
                cwd: cwd_record,
                tab_membership: record.tab_membership.clone(),
                agent_type: record.agent_type.clone(),
            });
        }
        hydrated
    }

    /// PRD #127 finding #2: wire a SINGLE daemon-side agent's pane on demand,
    /// keyed by its `DOT_AGENT_DECK_PANE_ID`. A scheduler-spawned agent surfaces
    /// its card to an already-attached TUI via a `SessionStart` broadcast (see
    /// [`crate::spawn`]), but that path creates only a placeholder session — the
    /// pane has no local [`StreamBackend`]. `focus_deck` calls this when
    /// `focus_pane` reports the pane missing, so focusing the card attaches the
    /// live daemon PTY (the same `AttachStream` + scrollback-replay wiring as
    /// [`Self::hydrate_from_daemon`]) instead of deleting the "stale" session.
    ///
    /// Returns `true` when the pane is present locally afterward — already wired
    /// (idempotent), or freshly attached — and `false` when no live daemon agent
    /// backs `pane_id` (a genuinely stale card the caller may then drop). Errors
    /// from `list_agents` / `attach` are absorbed into `false`, mirroring
    /// `hydrate_from_daemon`'s best-effort posture.
    pub fn hydrate_pane(&self, pane_id: &str) -> bool {
        if self.panes.lock().unwrap().contains_key(pane_id) {
            return true;
        }

        let client = self.client.clone();
        let runtime = self.runtime.clone();

        let list_client = client.clone();
        let records = match runtime.block_on(async move {
            tokio::time::timeout(HYDRATE_LIST_TIMEOUT, list_client.list_agents()).await
        }) {
            Ok(Ok(a)) => a,
            Ok(Err(e)) => {
                tracing::debug!(
                    pane_id,
                    error = %e,
                    "hydrate_pane: list_agents failed, treating as no-backing-agent"
                );
                return false;
            }
            Err(_) => {
                tracing::debug!(
                    pane_id,
                    timeout_ms = HYDRATE_LIST_TIMEOUT.as_millis() as u64,
                    "hydrate_pane: list_agents timed out, treating as no-backing-agent"
                );
                return false;
            }
        };

        // Match the daemon agent whose child carries this exact
        // `DOT_AGENT_DECK_PANE_ID` — the same value the placeholder session and
        // the agent's hook events route by.
        let Some(record) = records
            .into_iter()
            .find(|r| r.pane_id_env.as_deref() == Some(pane_id))
        else {
            return false;
        };

        let agent_id = record.id.clone();
        let id_for_attach = agent_id.clone();
        let client_for_attach = client.clone();
        let conn = match runtime.block_on(async move {
            tokio::time::timeout(
                HYDRATE_ATTACH_TIMEOUT,
                client_for_attach.attach(&id_for_attach),
            )
            .await
        }) {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                tracing::debug!(
                    pane_id,
                    agent_id = %agent_id,
                    error = %e,
                    "hydrate_pane: attach failed"
                );
                return false;
            }
            Err(_) => {
                tracing::debug!(
                    pane_id,
                    agent_id = %agent_id,
                    timeout_ms = HYDRATE_ATTACH_TIMEOUT.as_millis() as u64,
                    "hydrate_pane: attach timed out"
                );
                return false;
            }
        };

        let pane_name = record
            .display_name
            .clone()
            .unwrap_or_else(|| agent_id.clone());
        let (parser_rows, parser_cols) = parser_init_dims(record.rows, record.cols);
        self.wire_stream_pane(
            pane_id.to_string(),
            agent_id,
            conn,
            pane_name,
            None,
            record.cwd.clone(),
            parser_rows,
            parser_cols,
        );
        self.panes.lock().unwrap().contains_key(pane_id)
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
    /// An unknown `pane_id` is a soft error so callers iterating across
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
        let mut s = pane.backend;
        // Surface a closed channel as `CommandFailed` so callers
        // (e.g. `detach_all_streams`) can include it in their per-pane
        // error list. Survival is preserved either way: if the writer
        // task already exited, the socket has already closed and the
        // daemon has already observed EOF (implicit detach). The error
        // is purely observability — the user should know the explicit
        // signal didn't reach the wire.
        if s.input_tx.send(StreamCmd::Detach).is_err() {
            return Err(PaneError::CommandFailed(format!(
                "Pane {pane_id} stream I/O task ended"
            )));
        }
        if let Some(handle) = s.io_task.take() {
            // Hand the runtime a brief window to drain the queued
            // `Detach` and put the `KIND_DETACH` frame on the wire
            // before the socket goes away. Bound the wait at 200ms —
            // generous for a 5-byte frame on a local socket. On timeout
            // `tokio::time::timeout` drops the wrapped JoinHandle, which
            // only *detaches* the task; it does not cancel it. So we
            // capture an `AbortHandle` first and call `.abort()`
            // unconditionally afterward to terminate the writer
            // deterministically. `abort()` on a finished task is a
            // no-op, so this is safe regardless of which branch
            // (timeout vs. completion) fired.
            let abort = handle.abort_handle();
            let _ = s.runtime.block_on(async move {
                tokio::time::timeout(Duration::from_millis(200), handle).await
            });
            abort.abort();
        }
        // `s` drops here → channel sender drops. The socket halves
        // owned by the (now-aborted) task will be dropped on the next
        // runtime tick.
        Ok(())
    }

    /// Detach every pane. Used by the M2.5 "Detach (leave agents
    /// running)" option in the quit dialog: a single keystroke signals
    /// voluntary detach for all agents before the TUI exits. Returns
    /// the list of `(pane_id, error)` pairs for any panes that failed
    /// to detach — the caller can decide whether to surface them; a
    /// non-empty result does not block the quit.
    pub fn detach_all_streams(&self) -> Vec<(String, PaneError)> {
        let pane_ids: Vec<String> = {
            let panes = self.panes.lock().unwrap();
            panes.keys().cloned().collect()
        };
        let mut errors = Vec::new();
        for id in pane_ids {
            if let Err(e) = self.detach_pane(&id) {
                errors.push((id, e));
            }
        }
        errors
    }

    /// PRD #92 F1: send `KIND_SHUTDOWN` to the daemon, asking it to
    /// terminate every managed agent and exit. Used by the **Stop** option
    /// in the Ctrl+C dialog. Returns `Ok(())` once the daemon has
    /// acknowledged the shutdown (via socket close) or after the 1-second
    /// fallback inside [`DaemonClient::send_shutdown`]. The TUI proceeds to
    /// quit regardless of the result — Stop has already committed at the
    /// dialog level. A wrapped error is returned for observability only.
    pub fn shutdown_daemon(&self) -> Result<(), PaneError> {
        let client = self.client.clone();
        self.runtime
            .block_on(async move { client.send_shutdown().await })
            .map_err(|e| PaneError::CommandFailed(format!("send_shutdown: {e}")))
    }
}

/// Bounded wait for an in-flight daemon resize call. Two seconds is far
/// longer than a healthy local Unix-socket round-trip for a single Resize op
/// but short enough that a wedged daemon can't park the worker indefinitely.
/// On timeout the underlying `DaemonClient` connection drops, releasing the
/// FD and any per-connection daemon-side task.
const RESIZE_DAEMON_TIMEOUT: Duration = Duration::from_secs(2);

/// Hard cap on the number of agents the TUI will hydrate from the daemon
/// on bootstrap. Far above any realistic interactive workload (the TUI
/// only renders a handful of panes at once); the cap exists so a buggy or
/// hostile same-user daemon advertising thousands of fake ids can't fan
/// out unbounded sockets and tasks at startup. Hits in normal use should
/// never happen — if they do, the truncation log line is a signal that
/// something on the daemon side is misbehaving.
const HYDRATE_MAX_PANES: usize = 256;

/// Bounded wait for the `list_agents` round-trip during rehydration. A
/// healthy daemon answers in well under a millisecond; a daemon that
/// fails to respond within five seconds is treated as if it had no
/// agents (the user can reconnect). Without this bound, a parked daemon
/// would hang TUI startup indefinitely on the blocking `block_on` call
/// in `hydrate_from_daemon`.
const HYDRATE_LIST_TIMEOUT: Duration = Duration::from_secs(5);

/// Bounded wait for each per-agent `attach` during rehydration. Tighter
/// than the list timeout because there are up to [`HYDRATE_MAX_PANES`] of
/// these in series — the TUI shouldn't take HYDRATE_MAX_PANES × 5s on a
/// pathological daemon. On timeout the agent is skipped (logged at debug)
/// and rehydration continues with the rest.
const HYDRATE_ATTACH_TIMEOUT: Duration = Duration::from_secs(3);

/// Bounded wait for the `start_agent` RPC inside `create_stream_pane`. The
/// daemon allocates a PTY and spawns the child process before replying,
/// which is heavier than `list_agents` but should still complete within a
/// few seconds on a healthy host. Without this bound a wedged same-UID
/// daemon would pin the TUI's blocking `block_on` indefinitely.
const CREATE_PANE_START_TIMEOUT: Duration = Duration::from_secs(5);

/// Bounded wait for the `attach` RPC inside `create_stream_pane`. Same
/// rationale as [`HYDRATE_ATTACH_TIMEOUT`]: a single attach round-trip is
/// well under a millisecond on a healthy daemon; capping at three seconds
/// keeps a wedged daemon from blocking pane creation forever. On timeout
/// the cleanup [`CREATE_PANE_STOP_TIMEOUT`] path runs and the timeout is
/// surfaced as the propagated error.
const CREATE_PANE_ATTACH_TIMEOUT: Duration = Duration::from_secs(3);

/// Bounded wait for the best-effort `stop_agent` cleanup inside
/// `create_stream_pane` when `attach` fails or times out. Auditor P3 on
/// Fix D: a wedged daemon could answer `attach` with Err promptly then
/// never respond to the cleanup `stop_agent`, leaving the function pinned
/// on the cleanup await. Tight because cleanup is best-effort — on
/// timeout we log a warning and still propagate the original attach
/// error (the daemon-side agent may be leaked, same outcome as a stop_agent
/// that errored).
const CREATE_PANE_STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// PRD #92 F8 — bounded wait for the Ctrl+W `stop-agent` RPC. The
/// daemon's `close_agent` path now does a SIGTERM-with-grace before
/// SIGKILL (`AGENT_TERMINATE_GRACE = 3 s` in `src/agent_pty.rs`), so
/// the RPC can take up to ~3 s in the worst case (uncooperative agent
/// that ignores SIGTERM). Pre-F8 the Ctrl+W path reused
/// `CREATE_PANE_STOP_TIMEOUT` (2 s); that's now too tight — a SIGTERM-
/// ignoring agent would trip the controller timeout before the
/// daemon-side SIGKILL fallback fired. 5 s = 3 s F8 grace + 2 s
/// buffer for SIGKILL delivery, child reap, and RPC round-trip on a
/// loaded system. Anything longer is a real daemon hang and the user
/// gets the "stop-agent timed out" error message with a retry hint.
const CTRL_W_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// PRD #92 F12: initial wait between `list_agents` lookups when the
/// per-pane attach stream has ended and we're trying to find the
/// freshly-respawned agent for `pane_id_env`. The F9 clear=true delegate
/// path kills the OLD agent before spawning the NEW one; the daemon's
/// event-driven respawn dispatch (F9 followup-6) closes the timing window
/// in the happy case but real-world respawns can take much longer
/// (Claude Code via devbox: 0.5-3 s SIGTERM-to-exit + new-process
/// startup, up to ~5 s pathological when SIGTERM is trapped). The
/// exponential backoff below trades a few extra `list_agents` calls for
/// budget that actually covers the production gap.
const REATTACH_LOOKUP_INITIAL_DELAY: Duration = Duration::from_millis(200);

/// PRD #92 F12: cap on the per-iteration sleep. Backoff doubles each
/// miss until it hits this ceiling, then stays flat — keeps the retry
/// cadence under one lookup per second for the slow-respawn tail.
const REATTACH_LOOKUP_MAX_DELAY: Duration = Duration::from_millis(1000);

/// PRD #92 F12: total wall-clock budget for finding the respawned agent
/// before [`resolve_and_reattach`] gives up. Covers the SIGTERM grace
/// (up to [`AGENT_TERMINATE_GRACE`](crate::agent_pty) = 3 s) plus
/// new-process startup plus margin. With the 200 ms initial doubling to
/// a 1 s cap, the actual schedule is approximately
/// 200, 400, 800, 1000, 1000, 1000, 1000, 1000, 1000, 1000 (cumulative
/// ~9.4 s) — fast respawns succeed on the first one or two attempts;
/// slow ones get caught within the budget. On give-up the io_task
/// exits cleanly; the pane keeps its last-rendered screen and the user
/// can close it manually.
const REATTACH_LOOKUP_TOTAL_BUDGET: Duration = Duration::from_secs(10);

/// PRD #92 F12: bounds NEW agents that produce zero NEW bytes after the
/// initial snapshot replay before terminating. Reader-side any
/// `KIND_STREAM_OUT` byte — including the daemon's snapshot replay sent
/// on every attach — resets this counter, so a crash-on-start agent
/// whose snapshot replays before each crash is not caught by this bound
/// alone. The no-live-agent path via [`resolve_and_reattach`] is the
/// primary protection, giving up after [`REATTACH_LOOKUP_TOTAL_BUDGET`]
/// when `pane_id_env` has no matching live agent.
///
/// Assumes the daemon keeps the attach stream open while the agent is
/// alive but idle — i.e. the stream doesn't close just because the
/// agent isn't emitting bytes. A daemon change that closes idle
/// streams aggressively would cause healthy agents to be classified
/// as dead by this bound. See [`crate::agent_pty`] for the related
/// daemon-side respawn coordination this retry loop pairs with.
const REATTACH_MAX_EMPTY_SESSIONS: u32 = 3;

/// PRD #92 F12: per-pane I/O task body. Drives the attach-stream
/// reader/writer pair for a single pane; on STREAM_END from the daemon
/// (typically: OLD agent died as part of F9's clear=true respawn), look
/// up the pane's NEW agent via `list_agents` filtered by `pane_id_env`
/// and re-`attach` to it, with exponential backoff capped by
/// [`REATTACH_LOOKUP_TOTAL_BUDGET`]. Updates `agent_id` under the
/// shared mutex so a concurrent `close_pane` / `resize_pane_pty` targets
/// the NEW agent's id. Returns when:
/// - the input channel is closed or `KIND_DETACH` was sent (pane teardown
///   or explicit M2.5 detach — never re-attach),
/// - no live agent is found for `pane_id_env` within the retry window
///   (the pane was permanently closed on the daemon side),
/// - or [`REATTACH_MAX_EMPTY_SESSIONS`] consecutive re-attaches yield
///   zero bytes (the NEW agent crashes on every spawn).
#[allow(clippy::too_many_arguments)]
async fn run_pane_io_task(
    pane_id: String,
    client: DaemonClient,
    initial_conn: AttachConnection,
    agent_id: Arc<Mutex<String>>,
    mut input_rx: tokio::sync::mpsc::UnboundedReceiver<StreamCmd>,
    parser: Arc<Mutex<vt100::Parser>>,
    mouse_mode: Arc<AtomicBool>,
    hyperlinks: Arc<Mutex<HyperlinkMap>>,
) {
    let mut conn_opt: Option<AttachConnection> = Some(initial_conn);
    let mut consecutive_empty_sessions: u32 = 0;

    'outer: loop {
        let conn = match conn_opt.take() {
            Some(c) => c,
            None => break 'outer,
        };
        let (mut rd, mut wr) = conn.into_split();
        let mut bytes_received_this_session = false;
        let writer_won;
        {
            // Reader half: STREAM_OUT → process pipeline. Tracks whether
            // any STREAM_OUT frames arrived so the outer loop can detect
            // an "immediately Closed" session (Failure mode #1 in PRD #92
            // F12 context) and cap retries.
            let reader = async {
                let mut osc8 = Osc8Filter::new();
                loop {
                    match crate::daemon_protocol::read_frame(&mut rd).await {
                        Ok(None) => break,
                        Ok(Some((kind, bytes))) => match kind {
                            crate::daemon_protocol::KIND_STREAM_OUT => {
                                bytes_received_this_session = true;
                                process_agent_output_chunk(
                                    &bytes,
                                    &mut osc8,
                                    &parser,
                                    &mouse_mode,
                                    &hyperlinks,
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
            // an explicit detach before the socket closes. On write
            // failure we park forever so the reader's branch wins the
            // select! — write failure usually means the socket is gone,
            // and the reader's end-of-stream is what determines whether
            // to auto-reattach (F12).
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
                                // Park the writer branch so the reader's STREAM_END/EOF
                                // branch wins the surrounding `select!` and drives the
                                // reattach decision. The `Input` we just dequeued is
                                // lost (its bytes never made it onto the wire), but
                                // any subsequent items still buffered in `input_rx`
                                // remain in the channel and are drained on the next
                                // iteration's writer.
                                std::future::pending::<()>().await;
                                unreachable!();
                            }
                        }
                        StreamCmd::Detach => {
                            // Best-effort: even if the write errors,
                            // exiting here closes the socket and the
                            // daemon will observe EOF — the agent
                            // still survives.
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

            // `select!` lets us tell apart "reader exited" (STREAM_END /
            // EOF from the daemon — candidate for auto-reattach) from
            // "writer exited" (explicit detach, or `input_tx` dropped on
            // pane teardown — never reattach). The losing future is
            // dropped here, releasing its borrow of `rd` / `wr` so the
            // outer loop can rebind them on the next iteration.
            tokio::pin!(reader, writer);
            writer_won = tokio::select! {
                _ = &mut reader => false,
                _ = &mut writer => true,
            };
        }

        if writer_won {
            break 'outer;
        }

        // Reader exited. Decide: re-attach to the (likely-respawned)
        // agent for this pane, or give up. Zero-byte sessions guard
        // against an immediately-closing agent looping the io_task
        // forever; non-empty sessions reset the counter.
        if bytes_received_this_session {
            consecutive_empty_sessions = 0;
        } else {
            consecutive_empty_sessions += 1;
            if consecutive_empty_sessions >= REATTACH_MAX_EMPTY_SESSIONS {
                tracing::debug!(
                    pane_id = %pane_id,
                    "auto-reattach: too many consecutive empty sessions; giving up"
                );
                break 'outer;
            }
        }

        match resolve_and_reattach(&client, &pane_id).await {
            Some((new_agent_id, new_conn)) => {
                tracing::debug!(
                    pane_id = %pane_id,
                    new_agent_id = %new_agent_id,
                    "auto-reattach: subscribed to new agent for pane"
                );
                *agent_id.lock().unwrap() = new_agent_id;
                conn_opt = Some(new_conn);
            }
            None => {
                tracing::debug!(
                    pane_id = %pane_id,
                    "auto-reattach: no live agent for pane within retry window; giving up"
                );
                break 'outer;
            }
        }
    }
}

/// PRD #92 F12: resolve `pane_id_env` → current agent_id via `list_agents`
/// and open a fresh `AttachConnection`. Polls with exponential backoff
/// — [`REATTACH_LOOKUP_INITIAL_DELAY`] doubling up to
/// [`REATTACH_LOOKUP_MAX_DELAY`] — until the elapsed time exceeds
/// [`REATTACH_LOOKUP_TOTAL_BUDGET`]. This covers the F9 respawn-in-flight
/// gap, which spans from a few milliseconds (sh-based test agents) to
/// several seconds (Claude Code via devbox, especially when SIGTERM is
/// trapped). Returns `None` if no live agent matches the pane within
/// the budget, or if every `attach` attempt fails.
async fn resolve_and_reattach(
    client: &DaemonClient,
    pane_id_env: &str,
) -> Option<(String, AttachConnection)> {
    let start = tokio::time::Instant::now();
    let mut delay = REATTACH_LOOKUP_INITIAL_DELAY;
    loop {
        match client.list_agents().await {
            Ok(records) => {
                let new_id_opt = records
                    .into_iter()
                    .find(|r| r.pane_id_env.as_deref() == Some(pane_id_env))
                    .map(|r| r.id);
                if let Some(new_id) = new_id_opt {
                    match client.attach(&new_id).await {
                        Ok(conn) => return Some((new_id, conn)),
                        Err(e) => tracing::debug!(
                            agent_id = %new_id,
                            error = %e,
                            "auto-reattach: attach to new agent failed; retrying after backoff"
                        ),
                    }
                }
            }
            Err(e) => tracing::debug!(
                error = %e,
                "auto-reattach: list_agents failed; retrying after backoff"
            ),
        }

        if start.elapsed() >= REATTACH_LOOKUP_TOTAL_BUDGET {
            return None;
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(REATTACH_LOOKUP_MAX_DELAY);
    }
}

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
    agent_id: Arc<Mutex<String>>,
) {
    // Mark the initial `None` value as seen so the first `changed()` call
    // waits for an actual resize, not the channel's seed value.
    let _ = rx.borrow_and_update();
    while rx.changed().await.is_ok() {
        let dims = *rx.borrow_and_update();
        let Some((rows, cols)) = dims else { continue };

        // Snapshot the current agent id under the std::sync mutex (brief,
        // not held across `.await`). PRD #92 F12: this can change between
        // resize ops when the io_task auto-renews the per-pane subscription
        // to a freshly-respawned agent; the next resize naturally targets
        // the new agent.
        let id = agent_id.lock().unwrap().clone();

        let client = DaemonClient::new(daemon_path.clone());
        match tokio::time::timeout(RESIZE_DAEMON_TIMEOUT, client.resize_agent(&id, rows, cols))
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::debug!(
                    agent_id = %id,
                    rows, cols,
                    error = %e,
                    "resize-agent failed (transient — next resize will reconcile)"
                );
            }
            Err(_) => {
                tracing::debug!(
                    agent_id = %id,
                    rows, cols,
                    timeout_ms = RESIZE_DAEMON_TIMEOUT.as_millis() as u64,
                    "resize-agent timed out (transient — next resize will reconcile)"
                );
            }
        }
    }
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

    /// PRD #83: surface the inherent [`EmbeddedPaneController::focused_pane_id`]
    /// through the trait so `TabManager`'s tab-switch focus capture can
    /// read it via `Arc<dyn PaneController>`.
    fn focused_pane_id(&self) -> Option<String> {
        EmbeddedPaneController::focused_pane_id(self)
    }

    /// PRD #110 followup: snapshot the daemon-side `agent_id` currently
    /// bound to a pane. Brand-new pane creation sites call this right
    /// after `create_pane_with_options` returns so the local placeholder
    /// can be born with the correct `agent_id` and the strict-equality
    /// reuse guard in `AppState::apply_event` accepts the agent's first
    /// `SessionStart` event. The id is held under a `Mutex` because PRD
    /// #92 F12 rotates it on F9 clear=true respawns; we clone the latest
    /// value while the lock is held and never await across it.
    fn pane_agent_id(&self, pane_id: &str) -> Option<String> {
        let panes = self.panes.lock().unwrap();
        panes
            .get(pane_id)
            .map(|p| p.backend.agent_id.lock().unwrap().clone())
    }

    fn create_pane_with_options(
        &self,
        command: Option<&str>,
        cwd: Option<&str>,
        opts: AgentSpawnOptions<'_>,
    ) -> Result<(String, String), PaneError> {
        // The pane ID is allocated up front because it has to be injected into
        // the child's environment as DOT_AGENT_DECK_PANE_ID. If the spawn
        // below fails, the ID is intentionally consumed (a gap in the
        // sequence is harmless and avoids racing concurrent `create_pane`
        // calls to revert the counter).
        let pane_id = self.allocate_id();
        // Single source of truth for the in-session label, local Pane.name,
        // and the daemon's StartAgent.display_name. `resolve_display_name`
        // applies trim + `is_valid_display_name` + shell fallback, so all
        // downstream sites store the SAME string by construction — fixing
        // the divergence M2.11 fixup-3 reviewer P2 / auditor LOW called
        // out (bare `"   "`, surround whitespace, and control-byte
        // commands all converge here).
        let resolved = agent_pty::resolve_display_name(opts.display_name, command);

        let result = self.create_stream_pane(
            pane_id,
            command,
            cwd,
            &resolved,
            opts.tab_membership,
            opts.agent_type,
            opts.rows,
            opts.cols,
        );
        result.map(|id| (id, resolved))
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
        let s = pane.backend;
        // Ctrl+W is the explicit "kill the agent" path per PRD #76 line
        // 220 — it must send `stop-agent` over the protocol so the
        // daemon SIGKILLs the underlying child. Plain TUI exit takes a
        // different path: panes are dropped, `StreamBackend::drop`
        // aborts the I/O task, and the daemon sees the closed socket as
        // implicit detach. Order here matters: send `stop-agent` first
        // (over a fresh connection), then let the drop abort the I/O
        // task. If we aborted first, the daemon would treat the
        // dropped attach connection as a detach and the agent would
        // survive.
        let client = DaemonClient::new(s.daemon_path.clone());
        // Snapshot the latest agent id under the shared mutex; PRD #92 F12
        // can swap this to the respawned id while the pane is alive, and
        // Ctrl+W must target the currently-bound agent, not the one we
        // first attached to. Keep the Arc around so the retry below can
        // re-read the id after a mid-reattach swap.
        let shared_agent_id = Arc::clone(&s.agent_id);
        let initial_agent_id = shared_agent_id.lock().unwrap().clone();
        // CodeRabbit Fix E: bound the stop-agent RPC. Without this
        // timeout a wedged daemon would pin the TUI renderer
        // indefinitely (Ctrl+W happens on the render thread via
        // `block_on`) while the pane has already been removed from the
        // registry — the UI would freeze on a phantom-closed pane.
        //
        // PRD #92 F8: the daemon's `close_agent` path is now SIGTERM-
        // with-grace before SIGKILL (`AGENT_TERMINATE_GRACE = 3 s`),
        // so the worst-case RPC duration grew from "well under a
        // millisecond" to "up to ~3 s" for an uncooperative agent.
        // The Ctrl+W path therefore needs a generous budget —
        // `CTRL_W_STOP_TIMEOUT` (5 s = grace + 2 s buffer) — rather
        // than the 2 s `CREATE_PANE_STOP_TIMEOUT` it used to reuse.
        //
        // PRD #92 F12 followup (auditor #1): if Ctrl+W lands inside the
        // ~300 ms reattach window, `initial_agent_id` is the OLD
        // (just-killed) agent — the daemon answers stop-agent with an
        // "Agent <id> not found" error. Re-read the shared agent id once
        // and retry: the io_task may have already swapped in the NEW id
        // from the F9 respawn. If the retry also fails we fall through
        // to the existing log+restore path; we don't loop further.
        let (agent_id, stop_result) = s.runtime.block_on(async move {
            use crate::daemon_client::ClientError;
            let first = tokio::time::timeout(
                CTRL_W_STOP_TIMEOUT,
                client.stop_agent(&initial_agent_id),
            )
            .await;
            match first {
                Ok(Err(ClientError::Server(ref msg)))
                    if msg.to_lowercase().contains("not found") =>
                {
                    let retry_id = shared_agent_id.lock().unwrap().clone();
                    tracing::debug!(
                        first_agent_id = %initial_agent_id,
                        retry_agent_id = %retry_id,
                        "close_pane: stop-agent returned 'not found'; retrying once with currently-bound agent id"
                    );
                    let second =
                        tokio::time::timeout(CTRL_W_STOP_TIMEOUT, client.stop_agent(&retry_id))
                            .await;
                    (retry_id, second)
                }
                other => (initial_agent_id, other),
            }
        });
        match stop_result {
            Ok(Ok(())) => {
                // Drop `s` → io_task aborts. No explicit abort needed.
                Ok(())
            }
            Ok(Err(e)) => {
                // Don't silently degrade to detach: a swallowed
                // stop-agent error would close the socket, the daemon
                // would treat the close as implicit detach, and the
                // agent would survive on the remote with no signal to
                // the user. Re-insert the pane so a retry remains
                // possible (the io_task is still alive at this point —
                // `s` has not been dropped).
                tracing::error!(
                    agent_id = %agent_id,
                    error = %e,
                    "stop-agent failed during Ctrl+W close — pane retained for retry"
                );
                let restored = Pane {
                    backend: s,
                    screen: pane.screen,
                    name: pane.name,
                    is_focused: pane.is_focused,
                    command: pane.command,
                    cwd: pane.cwd,
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
            Err(_) => {
                // Timeout: daemon never answered. Same restore path as
                // the RPC-error branch — the io_task is still alive
                // (`s` not dropped), the daemon-side agent likely still
                // exists, and the user needs a visible pane to retry
                // against rather than a phantom-closed one.
                tracing::error!(
                    agent_id = %agent_id,
                    timeout_ms = CTRL_W_STOP_TIMEOUT.as_millis() as u64,
                    "stop-agent timed out during Ctrl+W close — pane retained for retry"
                );
                let restored = Pane {
                    backend: s,
                    screen: pane.screen,
                    name: pane.name,
                    is_focused: pane.is_focused,
                    command: pane.command,
                    cwd: pane.cwd,
                    mouse_mode: pane.mouse_mode,
                    hyperlinks: pane.hyperlinks,
                };
                self.panes
                    .lock()
                    .unwrap()
                    .insert(pane_id.to_string(), restored);
                Err(PaneError::CommandFailed(format!(
                    "stop-agent timed out for pane {pane_id}"
                )))
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

    fn rename_pane(&self, pane_id: &str, name: &str) -> Result<RenameOutcome, PaneError> {
        // M2.11 fixup 4 — single normalization rule shared with
        // `create_pane_with_display_name`: trim, then either
        //   * empty after trim → Cleared (None on daemon, "" locally)
        //   * trimmed value passes `is_valid_display_name` → Applied
        //     with that EXACT string on both local pane.name and the
        //     daemon record
        //   * non-empty but fails validation (control bytes, oversized,
        //     etc.) → Rejected — don't touch local or daemon,
        //     debug-log so the user can see why the label didn't update
        //
        // M2.11 fixup 5 — return the outcome so the dashboard rename
        // handler can mirror the controller-resolved label into the UI
        // display-name maps. Before this the UI inserted the raw
        // rename text verbatim and diverged from the controller (a
        // `"  newname  "` rename left the UI map padded; a
        // control-byte rename slipped escapes into the dashboard
        // title even though the controller rejected the change).
        //
        // Rejecting on invalid input (rather than silently falling back
        // to command/"shell") matches the user's intent: they typed
        // garbage, so the existing label stays put instead of being
        // replaced with an unrelated string they didn't ask for.
        //
        // M2.11 fixup 6 — route through `RenameOutcome::applied` so the
        // trim + `is_valid_display_name` invariant is enforced by a
        // single typed constructor instead of repeated inline in every
        // controller / mock. The constructor returns the same three
        // outcomes the production controller already maps to: empty
        // → Cleared, valid → Applied(trimmed), invalid → Rejected.
        let outcome = RenameOutcome::applied(name);
        let new_label: Option<String> = match &outcome {
            RenameOutcome::Applied(label) => Some(label.clone()),
            RenameOutcome::Cleared => None,
            RenameOutcome::Rejected => {
                tracing::debug!(
                    pane_id = %pane_id,
                    "rename_pane: rejected — name contains invalid bytes after trim"
                );
                return Ok(outcome);
            }
        };

        // M2.11: snapshot the stream-backed agent id + cached cwd under
        // the pane lock, then release the lock before the daemon RPC.
        // The cwd echo matters because `set_agent_label` uses "None to
        // clear" semantics; if we passed `cwd: None` here every rename
        // would erase the daemon-stored cwd captured at spawn time.
        //
        // An empty/whitespace-only `name` is the user's "clear" intent —
        // we map it to `display_name: None` so the daemon-side field is
        // cleared rather than stored as a blank label. On reconnect,
        // hydrate_from_daemon then falls back to the agent_id rather
        // than restoring a stale pre-clear name (PRD #76 M2.11 reviewer
        // P1 clear-rename case).
        let local_name = new_label.clone().unwrap_or_default();
        let (agent_id, cwd) = {
            let mut panes = self.panes.lock().unwrap();
            let pane = panes
                .get_mut(pane_id)
                .ok_or_else(|| PaneError::CommandFailed(format!("Pane {pane_id} not found")))?;
            pane.name = local_name;
            // Snapshot the currently-bound agent id (PRD #92 F12 can
            // swap this to a respawned id over the life of the pane).
            (
                pane.backend.agent_id.lock().unwrap().clone(),
                pane.cwd.clone(),
            )
        };
        // Daemon RPC is fire-and-forget on the controller's runtime.
        // The TUI thread must never block on `set_agent_label`:
        // `issue_command` awaits `read_response` with no timeout, so a
        // slow or wedged daemon would otherwise freeze the renderer
        // until the socket errors out (PRD #76 M2.11 reviewer P1
        // non-blocking rename). The local pane name has already been
        // updated above, which is the user-visible effect; a transient
        // daemon failure resyncs on the next reconnect.
        let client = self.client.clone();
        let agent_id_for_log = agent_id.clone();
        let daemon_label = new_label.clone();
        self.runtime.spawn(async move {
            if let Err(e) = client.set_agent_label(&agent_id, daemon_label, cwd).await {
                tracing::debug!(
                    agent_id = %agent_id_for_log,
                    error = %e,
                    "rename_pane: set_agent_label failed — local rename kept, daemon will resync on next reconnect"
                );
            }
        });
        Ok(outcome)
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
    ///
    /// PRD #93 round-8: an embedded bracketed-paste marker in a multi-line
    /// `text` causes [`encode_pane_payload`] to return Err — log at warn
    /// and drop the write, same handling as a missing pane below.
    fn write_to_pane(&self, pane_id: &str, text: &str) -> Result<(), PaneError> {
        let payload = match encode_pane_payload(text) {
            Ok(payload) => payload,
            Err(e) => {
                tracing::warn!(
                    pane_id = %pane_id,
                    error = %e,
                    "write_to_pane: dropping write — encode_pane_payload rejected the input"
                );
                return Ok(());
            }
        };
        // Write the payload (content, optionally bracketed-paste-wrapped), flush, then
        // pause briefly before sending the submit CR. Agent TUIs like claude treat a
        // CR that arrives fused to the preceding text as newline-in-input; only a CR
        // that arrives as a separate event after a pause is honored as Enter. The
        // pane lock is released during the sleep so the UI thread can keep drawing.
        self.queue_stream_input(pane_id, payload)?;
        std::thread::sleep(SUBMIT_DELAY);
        self.queue_stream_input(pane_id, b"\r".to_vec())?;
        Ok(())
    }

    /// PRD #100: atomic counterpart of [`Self::write_to_pane`]. Routes
    /// through the new `WriteAndSubmit` RPC so the daemon holds its
    /// per-agent writer mutex across `payload → SUBMIT_DELAY → CR`,
    /// matching the daemon-initiated `write_to_pane_and_submit` contract.
    /// Used at the orchestrator spawn-time role-prompt injection site
    /// in `ui.rs`, where a concurrent daemon-initiated write (e.g.
    /// work-done feedback for a sibling worker) could otherwise
    /// interleave into the legacy two-frame path's mid-sequence gap and
    /// submit the user's prompt with daemon bytes fused in.
    fn write_and_submit_to_pane(&self, pane_id: &str, text: &str) -> Result<(), PaneError> {
        let client = self.client.clone();
        let pane_id = pane_id.to_string();
        let text = text.to_string();
        self.runtime
            .block_on(async move { client.write_and_submit(&pane_id, &text).await })
            .map_err(|e| PaneError::CommandFailed(format!("write_and_submit: {e}")))
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

    // PRD #104 M2: hydration sizes the local vt100 parser from the
    // daemon-reported dims. Pin the small helper so its three branches
    // (sane / older-daemon zero / out-of-range) keep their documented
    // contracts — a regression that silently re-clamped every snapshot
    // to 24×80 would otherwise show up only as visual scrollback
    // corruption.

    #[test]
    fn parser_init_dims_uses_daemon_supplied_values_when_in_range() {
        assert_eq!(parser_init_dims(120, 40), (120, 40));
        assert_eq!(parser_init_dims(1, 1), (1, 1));
        assert_eq!(
            parser_init_dims(PTY_RESIZE_DIM_MAX, PTY_RESIZE_DIM_MAX),
            (PTY_RESIZE_DIM_MAX, PTY_RESIZE_DIM_MAX)
        );
    }

    #[test]
    fn parser_init_dims_falls_back_to_24x80_when_daemon_omits_field() {
        // Pre-PRD daemon: field absent on the wire → serde_default → 0.
        assert_eq!(parser_init_dims(0, 0), (24, 80));
    }

    #[test]
    fn parser_init_dims_falls_back_when_out_of_range() {
        // Defensive clamp: vt100 panics on zero rows/cols and has
        // subtle edge cases at huge sizes. Refuse anything outside
        // the registry's own resize bounds.
        assert_eq!(parser_init_dims(0, 80), (24, 80));
        assert_eq!(parser_init_dims(24, 0), (24, 80));
        assert_eq!(parser_init_dims(PTY_RESIZE_DIM_MAX + 1, 40), (24, 80));
        assert_eq!(parser_init_dims(40, PTY_RESIZE_DIM_MAX + 1), (24, 80));
        assert_eq!(parser_init_dims(u16::MAX, u16::MAX), (24, 80));
    }
}
