use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    /// PRD #162: the daemon's live, event-derived session snapshot for this
    /// agent, joined onto the `AgentRecord` in the `ListAgents` handler (M1.2).
    /// `Some(..)` carries the real `status` / event-derived `agent_type` /
    /// `active_tool` / `tool_count` / prompt context so the hydrated card
    /// restores the pre-disconnect view instead of a bare `Idle` / "No agent"
    /// placeholder. `None` — older daemon, the dummy-state attach path, or an
    /// agent that never emitted an event — falls back to today's placeholder
    /// seeding via [`crate::state::AppState::seed_hydrated_session`].
    pub live: Option<crate::state::SessionSnapshot>,
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

/// PRD #341 M5 — the child-input side of
/// [`EmbeddedPaneController::for_scroll_seam_with_focused_pane`]: everything the
/// pane queued for the agent, standing in for the I/O task that would have framed
/// it as `KIND_STREAM_IN`.
///
/// Opaque on purpose — [`StreamCmd`] is a private wire detail, and a test only
/// needs the flattened bytes.
#[doc(hidden)]
pub struct SeamChildInput {
    rx: tokio::sync::mpsc::UnboundedReceiver<StreamCmd>,
}

impl SeamChildInput {
    /// Take every byte queued for the child since the last call, in order.
    /// `Detach` carries no payload and contributes nothing.
    pub fn drain_bytes(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        while let Ok(cmd) = self.rx.try_recv() {
            if let StreamCmd::Input(bytes) = cmd {
                out.extend_from_slice(&bytes);
            }
        }
        out
    }
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
    /// PRD #241 F3b: what the per-pane I/O task is doing right now — one of
    /// [`IO_ATTACHED`], [`IO_REATTACHING`], [`IO_FINISHED`].
    ///
    /// `close_pane` reads it to decide how long to keep asking the daemon
    /// whether a *replacement* agent has taken over this pane's slot before it
    /// accepts an "agent not found" as proof the pane is gone. See
    /// [`resolve_pane_slot_after_not_found`] for how each state maps to a
    /// settle window.
    io_state: Arc<AtomicU8>,
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
    /// Why this pane's I/O task gave up, once it has.
    ///
    /// `None` while attached, and also after a *deliberate* end (explicit
    /// detach, or pane teardown dropping `input_tx`) — those are not failures
    /// and must not be reported as one. `Some(_)` only for the two give-up
    /// exits in [`run_pane_io_task`], after which the pane can never accept
    /// input again.
    ///
    /// Before this existed the pane simply went quiet: it kept rendering its
    /// last frame and looking alive, every keystroke was dropped, and the only
    /// hint was a transient `PTY write failed: … stream I/O task ended` naming
    /// an internal detail. Recording the reason lets the pane say what actually
    /// happened.
    lost: Arc<Mutex<Option<PaneLostReason>>>,
}

/// Why a pane's attach I/O task stopped trying to reach its agent.
///
/// Both variants are terminal: the reader/writer pair is gone and the pane's
/// input channel has no receiver. They are distinguished because the causes are
/// unrelated — one is an agent that will not stay up, the other an agent the
/// daemon no longer has at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneLostReason {
    /// The agent was respawned but produced no output across
    /// [`REATTACH_MAX_EMPTY_SESSIONS`] consecutive attaches — it crashes on
    /// every spawn.
    AgentKeptCrashing,
    /// No live agent claimed this `pane_id` within
    /// [`REATTACH_LOOKUP_TOTAL_BUDGET`] — the agent is gone daemon-side.
    AgentGone,
}

impl PaneLostReason {
    /// One-line, non-internal explanation for the status line.
    pub fn user_message(self) -> &'static str {
        match self {
            Self::AgentKeptCrashing => {
                "Agent exited on every restart — pane is disconnected. Close it to start over."
            }
            Self::AgentGone => {
                "Agent is no longer running — pane is disconnected. Close it to start over."
            }
        }
    }

    /// Short marker for the pane title.
    pub fn title_marker(self) -> &'static str {
        "disconnected"
    }
}

/// The error every pane-input path returns when its send finds no receiver.
///
/// There is more than one way for input to reach a pane — raw keystrokes
/// ([`EmbeddedPaneController::write_raw_bytes`]) and queued text
/// ([`EmbeddedPaneController::queue_stream_input`], behind `write_to_pane` and
/// so behind mode init, config prompts and permission responses). They all fail
/// for the same reason and must explain it the same way; the first cut of this
/// only fixed the keystroke path, so a config prompt into a dead pane still
/// reported `stream I/O task ended` (caught in review on #286).
///
/// A recorded loss reason means the I/O task gave up on the agent — say so in
/// the user's terms. No reason means a deliberate detach or a teardown still in
/// flight, which is not a failure to explain.
fn input_failure(pane_id: &str, backend: &StreamBackend) -> PaneError {
    match *backend.lost.lock().unwrap() {
        Some(reason) => PaneError::CommandFailed(reason.user_message().to_string()),
        None => PaneError::CommandFailed(format!("Pane {pane_id} is detached")),
    }
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
    /// PRD #611 M2 — every agent-output byte this pane's parser has been handed
    /// since the pane was created. Shared with the per-pane I/O task, which is
    /// the only thing that feeds the parser once the pane is live.
    ///
    /// The "has this agent actually been running?" half of the cannot-scroll
    /// trigger: a pane holding no retained lines means nothing on its own —
    /// a claude pane one second after spawn holds none either — so the notice
    /// needs a second signal that separates "nothing yet" from "never any".
    bytes_since_spawn: Arc<AtomicU64>,
    /// PRD #611 M2 — when a scroll attempt on this pane last found nothing to
    /// move, or `None` if no such attempt has happened (or the notice has since
    /// been dismissed by a keystroke).
    ///
    /// The expiry policy is deliberately NOT here: `ui.rs` owns the TTL it
    /// shares with the command banner, so this records only the arming instant
    /// and the renderer decides whether it is still worth drawing.
    scroll_notice_armed_at: Option<Instant>,
}

/// Thread-safe pane registry.
type PaneRegistry = Arc<Mutex<HashMap<String, Pane>>>;

/// PRD #611 M2 — the facts a scroll attempt needs about one pane, sampled
/// together by [`EmbeddedPaneController::scroll_facts`].
///
/// Grouped into one read because the cannot-scroll trigger is a conjunction:
/// "holds no retained lines" is meaningless without "and has been running long
/// enough to have some", and sampling those separately would let a pane look
/// mature at one instant and empty at another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneScrollFacts {
    /// Lines the parser actually retains right now, measured by asking vt100 to
    /// clamp an impossible offset — never the configured scrollback capacity.
    pub scrollback_depth: usize,
    /// Agent-output bytes handed to this pane's parser since the pane was made.
    pub bytes_since_spawn: u64,
    /// The parser's live geometry, which is what one "screenful" means here.
    pub rows: u16,
    /// See [`Self::rows`].
    pub cols: u16,
}

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

/// The single construction point for a pane's local vt100 parser (issue #363).
///
/// Every parser this module builds — spawn, hydration, the L1 render/scroll
/// seams, and the rebuild that follows a contained parser panic — goes through
/// here, so "what is a valid parser geometry?" has one definition
/// ([`parser_init_dims`]) and one place it is applied.
///
/// Not having that is what issue #363 was: `wire_stream_pane` reached
/// `vt100::Parser::new` raw on the **shipped spawn path**, while
/// [`EmbeddedPaneController::seam_pane`] a few hundred lines above already
/// clamped. The spawn caller forwards viewport-derived dims from `ui.rs`'s
/// layout helpers, every one of which ends in a `saturating_sub(2)` border
/// allowance, so a short or narrow terminal built a 0-row or 0-column parser in
/// the real binary.
///
/// A zero axis breaks the grid at *construction*, not at first byte: vt100
/// 0.16.2's `Grid::new` sets `scroll_bottom: size.rows - 1`, which under debug
/// overflow checks panics outright (measured — `grid.rs:26`) and in a release
/// build wraps to 65535, leaving a grid with no rows claiming a scroll region
/// that ends at row 65535. That is why [`guarded_parser_feed`] never covered
/// this: it wraps feeds, not construction. The only thing that did was the next
/// frame's resize sweep landing before any output arrived — incidental, not
/// designed. (`col_wrap`'s `prev_pos.row -= scrolled` underflow on a *1-row*
/// grid is a separate, feed-time hazard, and that one the feed guard does
/// contain.)
///
/// This guards *construction* only, which is the whole of the contract. A
/// parser's geometry can still move afterwards through `screen_mut().set_size`
/// in [`EmbeddedPaneController::resize_pane_pty`], which since issue #747
/// clamps the UPPER bound to [`PTY_RESIZE_DIM_MAX`] (matching the daemon) but
/// still relies on its one caller, `ui.rs`'s `resize_panes_to_layout`, to skip
/// a zero axis rather than rejecting one itself — and feeding stays separately
/// guarded by [`guarded_parser_feed`].
fn new_pane_parser(rows: u16, cols: u16) -> vt100::Parser {
    let (rows, cols) = parser_init_dims(rows, cols);
    vt100::Parser::new(rows, cols, PANE_SCROLLBACK_LINES)
}

use crate::pane_input::{SUBMIT_DELAY, encode_pane_payload};

/// Placeholder daemon socket path for the render-only constructors below. It
/// intentionally points at nothing: any spawn/attach against it fails, which is
/// exactly what a render seam wants.
fn render_only_socket_path() -> PathBuf {
    let mut placeholder = std::env::temp_dir();
    placeholder.push(format!(
        "dot-agent-deck-render-only-{}.sock",
        std::process::id()
    ));
    placeholder
}

/// One lazily-built current-thread runtime shared by the render-only
/// constructors. A [`StreamBackend`] holds a `Handle` even when it owns no task,
/// and `Handle::current` panics outside a runtime — so a render seam that never
/// performs I/O still needs one to exist. Built on first use, so a normal run
/// (which always has a real runtime) never creates it.
fn render_only_runtime() -> tokio::runtime::Handle {
    use std::sync::OnceLock;
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("render-only runtime")
    })
    .handle()
    .clone()
}

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
    /// PRD #20 R20-007 (finding #10): typed stream rejections the daemon pushed
    /// on the attach stream (a `KIND_STREAM_REJECT` frame). Each per-pane I/O
    /// task appends `(pane_id, reason)` here when the daemon refuses a key/paste
    /// frame because the target went non-live / exited / rebound. The render loop
    /// drains this each frame via [`Self::take_stream_rejections`] and surfaces
    /// honest feedback + leaves PaneInput — closing the server/UI race where the
    /// UI's pre-forward liveness snapshot was stale.
    stream_rejections: Arc<Mutex<Vec<(String, String)>>>,
    /// PRD #241 F3b (review finding G2): warnings from closes that COMPLETED
    /// without being able to confirm the daemon side (see
    /// [`StopOutcome::DoneUnverified`]). `close_pane` returns `Ok(())` on that
    /// path — the card is gone and the caller has nothing to retry — so the
    /// message cannot ride the `Result`. The render loop drains this each frame
    /// (exactly like [`Self::stream_rejections`]) into `ui.status_message`, the
    /// same status line every other close outcome already reports through.
    close_warnings: Arc<Mutex<Vec<String>>>,
    /// PRD #611 M2 (review finding 4b): "is ANY pane's cannot-scroll notice
    /// armed?", so the dismissal that runs on every single keystroke can answer
    /// "no" without taking the pane-registry mutex and walking every pane.
    ///
    /// Set by [`Self::arm_scroll_notice`] and swapped back by
    /// [`Self::clear_scroll_notices`]. It is an over-approximation on purpose:
    /// it stays set while an armed notice merely ages out of its TTL, which
    /// costs one already-clear sweep and never a missed dismissal. Both writers
    /// run on the TUI thread, so the swap-then-sweep cannot lose an arm.
    any_scroll_notice_armed: Arc<AtomicBool>,
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
            stream_rejections: Arc::new(Mutex::new(Vec::new())),
            close_warnings: Arc::new(Mutex::new(Vec::new())),
            any_scroll_notice_armed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// PRD #20 R20-007 (finding #10): drain and return the typed stream
    /// rejections the daemon has pushed since the last call — `(pane_id, reason)`
    /// pairs. The render loop consumes these each frame to surface honest input
    /// feedback and leave PaneInput.
    pub fn take_stream_rejections(&self) -> Vec<(String, String)> {
        std::mem::take(&mut *self.stream_rejections.lock().unwrap())
    }

    /// PRD #241 F3b (review finding G2): drain and return the warnings queued by
    /// closes that completed WITHOUT being able to confirm the daemon side. The
    /// render loop consumes these each frame and shows them on the status line —
    /// a possibly-orphaned agent must never be a silent outcome. Empty on every
    /// ordinary close, successful or failed.
    pub fn take_close_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.close_warnings.lock().unwrap())
    }

    /// Test-only constructor for code paths that need a `PaneController`
    /// value but never actually exercise pane I/O — e.g. render-frame
    /// tests that build an empty controller just to satisfy a function
    /// signature. The daemon socket path is a tempdir placeholder; any
    /// attempt to spawn or attach against it will fail.
    #[cfg(test)]
    pub fn for_render_only_tests() -> Self {
        Self::new(render_only_socket_path(), render_only_runtime())
    }

    /// PRD #341 M1 — L1 render-seam constructor: a controller carrying exactly
    /// ONE focused pane whose vt100 screen has already consumed `bytes`, with no
    /// daemon behind it.
    ///
    /// [`Self::for_render_only_tests`] builds an EMPTY controller, and
    /// `render_terminal_panes` returns before it touches a cursor when there is
    /// no pane — so a seam that renders the real pane path needs a pane that
    /// exists. It is also `#[cfg(test)]`, hence unreachable from the integration
    /// tests that drive the `pub` L1 seams in `ui.rs`.
    ///
    /// The backend is inert by construction: no I/O task, no resize task, and
    /// the input channel's receiver is dropped, so nothing is spawned, nothing
    /// reaches a socket, and every input path fails as "detached". Rendering
    /// only ever reads `screen` / `is_focused` / `name`, which is the whole
    /// point of the seam. `#[doc(hidden)]`: `pub` because integration tests
    /// cannot enable a crate feature on demand, not because it is API.
    #[doc(hidden)]
    pub fn for_render_seam_with_focused_pane(
        pane_id: &str,
        rows: u16,
        cols: u16,
        bytes: &[u8],
    ) -> Self {
        // The receiver is dropped immediately: an inert backend must not be able
        // to queue input at an agent that does not exist.
        let (controller, _child_input) =
            Self::seam_with_focused_pane(pane_id, rows, cols, bytes, false);
        controller
    }

    /// PRD #341 M5 — L1 scroll-seam constructor: the same single-focused-pane
    /// controller as [`Self::for_render_seam_with_focused_pane`], but with the
    /// child-input channel KEPT so a test can see exactly which bytes (if any) the
    /// pane queued for the agent, and with the child's mouse-reporting flag
    /// settable.
    ///
    /// Those two are the whole point: the M5 safety property is "in command mode
    /// the wheel never reaches the agent's mouse protocol", and the only honest way
    /// to assert it is to record what the child would have received. Dropping the
    /// receiver (as the render seam does) would make every write fail as "detached"
    /// and a forwarding regression would look identical to correct behaviour.
    #[doc(hidden)]
    pub fn for_scroll_seam_with_focused_pane(
        pane_id: &str,
        rows: u16,
        cols: u16,
        bytes: &[u8],
        mouse_mode_enabled: bool,
    ) -> (Self, SeamChildInput) {
        let (controller, rx) =
            Self::seam_with_focused_pane(pane_id, rows, cols, bytes, mouse_mode_enabled);
        (controller, SeamChildInput { rx })
    }

    /// PRD #341 M6 — add ONE more inert seam pane, UNFOCUSED, to a controller
    /// already built by [`Self::for_scroll_seam_with_focused_pane`].
    ///
    /// The M6 scrollback reconcile keys on the `(mode, focused pane id)` PAIR, so
    /// its most interesting case — focus moving to an already-scrolled OTHER pane
    /// while `PaneInput` never lifts — cannot be posed against a one-pane
    /// controller at all. This adds the second pane through the same
    /// [`Self::seam_pane`] body the constructors use, so there is still exactly one
    /// place an inert pane is built.
    ///
    /// Child mouse reporting is left off: the reconcile never consults
    /// `mouse_mode`, only `screen`, `is_focused` and `name`. The child-input channel
    /// comes back for symmetry with
    /// [`Self::for_scroll_seam_with_focused_pane`] — a caller that wants to prove
    /// nothing at all reached THIS pane's agent can, and holding it keeps the pane's
    /// writes from failing as "detached" for the wrong reason.
    #[doc(hidden)]
    pub fn add_scroll_seam_pane(
        &self,
        pane_id: &str,
        rows: u16,
        cols: u16,
        bytes: &[u8],
    ) -> SeamChildInput {
        let (pane, rx) = Self::seam_pane(pane_id, rows, cols, bytes, false, false);
        self.panes.lock().unwrap().insert(pane_id.to_string(), pane);
        SeamChildInput { rx }
    }

    /// PRD #341 (code-review finding 3) — L1 seam constructor: an inert controller
    /// with **no panes at all**, so [`Self::focused_pane_id`] answers `None`.
    ///
    /// That is the state the finding is about — `UiMode::PaneInput` with nothing
    /// focused, which a vanished reactive pane with no successor really does
    /// produce — and it cannot be posed against either
    /// [`Self::for_render_seam_with_focused_pane`] or
    /// [`Self::for_scroll_seam_with_focused_pane`], both of which focus their pane
    /// by construction. `for_render_only_tests` builds exactly this controller but
    /// is `#[cfg(test)]`, hence unreachable from the integration tests that drive
    /// the `pub` L1 seams in `ui.rs`.
    #[doc(hidden)]
    pub fn for_render_seam_without_panes() -> Self {
        Self::new(render_only_socket_path(), render_only_runtime())
    }

    /// Shared body of the two L1 seam constructors: one focused pane whose vt100
    /// screen has already consumed `bytes`, with no daemon behind it.
    ///
    /// [`Self::for_render_only_tests`] builds an EMPTY controller, and
    /// `render_terminal_panes` returns before it touches a cursor when there is
    /// no pane — so a seam that renders the real pane path needs a pane that
    /// exists. It is also `#[cfg(test)]`, hence unreachable from the integration
    /// tests that drive the `pub` L1 seams in `ui.rs`.
    fn seam_with_focused_pane(
        pane_id: &str,
        rows: u16,
        cols: u16,
        bytes: &[u8],
        mouse_mode_enabled: bool,
    ) -> (Self, tokio::sync::mpsc::UnboundedReceiver<StreamCmd>) {
        let controller = Self::new(render_only_socket_path(), render_only_runtime());
        let (pane, input_rx) =
            Self::seam_pane(pane_id, rows, cols, bytes, mouse_mode_enabled, true);
        controller
            .panes
            .lock()
            .unwrap()
            .insert(pane_id.to_string(), pane);
        (controller, input_rx)
    }

    /// One inert seam pane: a real vt100 parser that has already consumed `bytes`,
    /// behind a backend with nothing live in it.
    ///
    /// The backend is inert apart from the returned input channel: no I/O task and
    /// no resize task are spawned, and nothing reaches a socket. Rendering only
    /// ever reads `screen` / `is_focused` / `name`, and scrolling only ever reads
    /// `screen` / `mouse_mode`, which is the whole point of the seams.
    ///
    /// `rows` / `cols` are caller-controlled on every seam above, and these are
    /// ordinary `pub` entry points of the release library — so they go through
    /// [`new_pane_parser`], the one guarded constructor the spawn and hydration
    /// paths also use, rather than reaching `vt100::Parser::new` raw. A zero axis
    /// would otherwise build a parser whose grid is already inconsistent before
    /// the first byte (`Grid::new` computes `scroll_bottom` as `rows - 1`), and
    /// `u16::MAX` square would ask for ~4.3 billion cells.
    ///
    /// Valid dims are not enough on their own: `parser_init_dims` admits a 1-row /
    /// 1-col parser, and vt100 0.16.2 underflows in `col_wrap` the moment text
    /// wraps in one that short. The live output path already contains that exact
    /// bug with [`guarded_parser_feed`], so the seam feeds through the same guard
    /// and rebuilds the parser at the same geometry on a contained panic — the
    /// pane then renders blank instead of taking the process down.
    fn seam_pane(
        pane_id: &str,
        rows: u16,
        cols: u16,
        bytes: &[u8],
        mouse_mode_enabled: bool,
        focused: bool,
    ) -> (Pane, tokio::sync::mpsc::UnboundedReceiver<StreamCmd>) {
        let mut parser = new_pane_parser(rows, cols);
        // Post-guard geometry, read back off the parser itself rather than
        // re-derived: what the warning below should name, and what the rebuild
        // has to reproduce. Re-deriving it is how two copies of a rule drift.
        let (rows, cols) = parser.screen().size();
        if guarded_parser_feed(|| parser.process(bytes)).is_err() {
            tracing::warn!(
                rows,
                cols,
                "vt100 parser panicked seeding an inert seam pane; rebuilding it empty at the \
                 same geometry. Known vt100 0.16.2 edge case in a very short pane."
            );
            parser = new_pane_parser(rows, cols);
        }

        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<StreamCmd>();
        // Dropped immediately: an inert backend must not be able to resize an
        // agent that does not exist.
        let (resize_tx, _resize_rx) = tokio::sync::watch::channel::<Option<(u16, u16)>>(None);

        let pane = Pane {
            backend: StreamBackend {
                agent_id: Arc::new(Mutex::new(String::new())),
                input_tx,
                io_task: None,
                io_state: Arc::new(AtomicU8::new(IO_FINISHED)),
                runtime: render_only_runtime(),
                daemon_path: render_only_socket_path(),
                resize_tx,
                resize_task: None,
                lost: Arc::new(Mutex::new(None)),
            },
            screen: Arc::new(Mutex::new(parser)),
            name: pane_id.to_string(),
            is_focused: focused,
            command: None,
            cwd: None,
            mouse_mode: Arc::new(AtomicBool::new(mouse_mode_enabled)),
            hyperlinks: Arc::new(Mutex::new(HyperlinkMap::new())),
            // The seam feeds `bytes` straight into the parser above rather than
            // through `process_agent_output_chunk`, so the counter is seeded
            // here — otherwise a seam pane would look freshly spawned no matter
            // how much output it had consumed.
            bytes_since_spawn: Arc::new(AtomicU64::new(bytes.len() as u64)),
            scroll_notice_armed_at: None,
        };
        (pane, input_rx)
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
                return Err(input_failure(pane_id, &pane.backend));
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

    /// Why this pane's agent connection was given up on, or `None` if the pane
    /// is still attached (or ended deliberately via detach / teardown).
    ///
    /// Read by the renderer so a disconnected pane is labelled as such instead
    /// of showing a frozen frame that looks live.
    pub fn pane_lost_reason(&self, pane_id: &str) -> Option<PaneLostReason> {
        let panes = self.panes.lock().unwrap();
        let pane = panes.get(pane_id)?;
        *pane.backend.lost.lock().unwrap()
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
    ///
    /// Issue #747: the request is normalized through
    /// [`agent_pty::clamp_pty_dims`] FIRST, so the parser and the wire carry
    /// the geometry the daemon will actually apply to the child rather than
    /// one the daemon would quietly narrow. Both sides of this function must
    /// use the same clamped pair — resizing the parser to 4198 columns while
    /// the child is told 4096 is exactly the divergence #747 is about. The
    /// production caller (`ui.rs`'s `resize_panes_to_layout`) already clamps
    /// its layout target through the same helper, which is what keeps its
    /// "only commit a real delta" check honest: comparing an unclamped target
    /// against a clamped parser would never match and would re-send a resize
    /// on every frame.
    pub fn resize_pane_pty(&self, pane_id: &str, rows: u16, cols: u16) -> Result<(), PaneError> {
        let (rows, cols) = agent_pty::clamp_pty_dims(rows, cols);
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

    /// PRD #611 M2 — everything a scroll attempt needs to know about whether
    /// this pane has anything to scroll, read in one pass under the registry
    /// lock so the three numbers cannot disagree about the same instant.
    ///
    /// `None` for an unknown pane, or for one whose parser mutex is poisoned —
    /// neither is a state the notice should guess about.
    pub fn scroll_facts(&self, pane_id: &str) -> Option<PaneScrollFacts> {
        let panes = self.panes.lock().unwrap();
        let pane = panes.get(pane_id)?;
        let bytes_since_spawn = pane.bytes_since_spawn.load(Ordering::Relaxed);
        let mut parser = pane.screen.lock().ok()?;
        let (rows, cols) = parser.screen().size();
        // vt100 0.16 clamps a requested offset to the lines it actually holds,
        // so asking for an impossible one and reading it back IS the depth.
        // Deliberately not `PANE_SCROLLBACK_LINES`: that is the capacity the
        // parser was built with, which every pane shares and which says nothing
        // about whether this one retained a single line. Restored immediately so
        // the measurement cannot move the view the user is looking at.
        let view = parser.screen().scrollback();
        parser.screen_mut().set_scrollback(usize::MAX);
        let scrollback_depth = parser.screen().scrollback();
        parser.screen_mut().set_scrollback(view);
        Some(PaneScrollFacts {
            scrollback_depth,
            bytes_since_spawn,
            rows,
            cols,
        })
    }

    /// PRD #611 M2 — record that a scroll attempt on this pane found nothing to
    /// move, at `at`.
    ///
    /// Idempotent by construction: re-arming an already-armed pane overwrites
    /// the instant and nothing else, so a user swiping repeatedly at an
    /// unscrollable pane refreshes one notice rather than stacking or blinking
    /// several.
    pub fn arm_scroll_notice(&self, pane_id: &str, at: Instant) {
        let mut panes = self.panes.lock().unwrap();
        if let Some(pane) = panes.get_mut(pane_id) {
            pane.scroll_notice_armed_at = Some(at);
            // Published while the registry lock is still held, so no reader can
            // observe an armed pane behind an unset bit.
            self.any_scroll_notice_armed.store(true, Ordering::Relaxed);
        }
    }

    /// PRD #611 M2 — when this pane's cannot-scroll notice was armed, if it is
    /// armed at all. The caller owns the expiry policy.
    pub fn scroll_notice_armed_at(&self, pane_id: &str) -> Option<Instant> {
        let panes = self.panes.lock().unwrap();
        panes.get(pane_id)?.scroll_notice_armed_at
    }

    /// PRD #611 M2 — disarm every pane's cannot-scroll notice.
    ///
    /// Called on the way IN to key handling, before the key is resolved, so
    /// dismissal can never depend on what the key turned out to mean and can
    /// never swallow it. Every pane rather than the focused one because focus
    /// may have moved since the notice was armed, and a notice that outlives
    /// the keystroke that was meant to clear it is the bug users report.
    ///
    /// O(1) in the overwhelmingly common case (review finding 4b): this runs on
    /// every character typed into a pane, and taking the registry mutex to write
    /// `None` over `None` on every pane is a per-keystroke cost paid for a state
    /// that is almost never armed. [`Self::any_scroll_notice_armed`] answers
    /// that without the lock; only a swap that finds it set walks the registry.
    pub fn clear_scroll_notices(&self) {
        if !self.any_scroll_notice_armed.swap(false, Ordering::Relaxed) {
            return;
        }
        let mut panes = self.panes.lock().unwrap();
        for pane in panes.values_mut() {
            pane.scroll_notice_armed_at = None;
        }
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
            .map_err(|_| input_failure(pane_id, &pane.backend))
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
        // PRD #201: seed/prompt to stash daemon-side for native pull (Pi
        // orchestrator panes only); `None` keeps the unchanged inject path.
        seed: Option<String>,
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
            seed,
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
        //
        // Issue #363: through [`new_pane_parser`], never `vt100::Parser::new`
        // raw. The two hydration callers hand over dims the daemon vouched for,
        // but the spawn caller (`create_stream_pane`) forwards viewport-derived
        // dims straight from `ui.rs`'s layout helpers — `right_column_pane_dims`,
        // `mode_side_pane_dims` and `mode_agent_pane_dims` all end in a
        // `saturating_sub(2)` border allowance — so a short or narrow terminal
        // arrives here with a zero axis and no caller in between rejects it, the
        // way `resize_panes_to_layout` does on the resize path.
        let parser = Arc::new(Mutex::new(new_pane_parser(rows, cols)));
        let mouse_mode = Arc::new(AtomicBool::new(false));
        let hyperlinks = Arc::new(Mutex::new(HyperlinkMap::new()));
        // PRD #611 M2: born at zero even on the hydration path. The daemon
        // replays its scrollback snapshot as ordinary `KIND_STREAM_OUT` before
        // live bytes, so a rehydrated pane counts exactly what its parser
        // consumed — which is the honest answer to "has this agent produced
        // substantial output into this parser?".
        let bytes_since_spawn = Arc::new(AtomicU64::new(0));

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
        let bytes_for_task = Arc::clone(&bytes_since_spawn);
        let agent_id_for_task = Arc::clone(&shared_agent_id);
        let client_for_task = self.client.clone();
        let pane_id_for_task = pane_id.clone();
        let rejections_for_task = Arc::clone(&self.stream_rejections);

        // PRD #241 F3b: published state for the I/O task. Seeded here rather
        // than inside `run_pane_io_task` so it already reads `IO_ATTACHED`
        // before the task is first polled — a `close_pane` racing the spawn
        // must never see a not-yet-started `IO_FINISHED`.
        let io_state = Arc::new(AtomicU8::new(IO_ATTACHED));
        let io_state_for_task = Arc::clone(&io_state);
        let io_state_for_exit = Arc::clone(&io_state);

        // Distinct from `io_state`, which answers "can this pane still adopt a
        // respawned agent?" for the close path. `lost` answers "why did it
        // stop?" for the user, and is only ever set on the two give-up exits —
        // `IO_FINISHED` is also reached by a deliberate detach, which is not a
        // failure and must not be reported as one.
        let lost = Arc::new(Mutex::new(None));
        let lost_for_task = Arc::clone(&lost);

        let io_task = runtime.spawn(async move {
            run_pane_io_task(
                pane_id_for_task,
                client_for_task,
                conn,
                agent_id_for_task,
                input_rx,
                parser_for_task,
                mouse_mode_for_task,
                hyperlinks_for_task,
                bytes_for_task,
                rejections_for_task,
                io_state_for_task,
                lost_for_task,
            )
            .await;
            // The task gave up: no respawned agent will be adopted for this
            // pane any more, so a later "agent not found" needs no settle wait.
            // An `abort()` skips this store, leaving the last in-flight state —
            // the conservative direction, and only reachable from
            // `StreamBackend::drop`, i.e. after any close has already finished.
            io_state_for_exit.store(IO_FINISHED, Ordering::SeqCst);
        });

        let pane = Pane {
            backend: StreamBackend {
                agent_id: shared_agent_id,
                input_tx,
                io_task: Some(io_task),
                io_state,
                runtime,
                daemon_path,
                resize_tx,
                resize_task: Some(resize_task),
                lost,
            },
            screen: parser,
            name,
            is_focused: false,
            command,
            cwd,
            mouse_mode,
            hyperlinks,
            bytes_since_spawn,
            scroll_notice_armed_at: None,
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
                live: record.live.clone(),
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
    /// Stop the daemon agent backing `pane_id` when this TUI has no local pane
    /// for it — the daemon-spawned-card case `close_pane` falls through to.
    ///
    /// Resolves the agent by `pane_id_env` through `list-agents` (the same lookup
    /// [`Self::hydrate_pane`] uses) and issues the ordinary `stop-agent`, so the
    /// daemon side of the close is byte-identical to the attached path. There is
    /// no local pane to tear down or to restore on failure.
    ///
    /// An empty match is reported as SUCCESS: the card is backed by nothing, so
    /// "close it" has already been achieved and the caller should drop the card
    /// rather than preserve an unclosable one.
    fn stop_unattached_pane(&self, pane_id: &str) -> Result<(), PaneError> {
        let list_client = self.client.clone();
        let records = match self.runtime.block_on(async move {
            tokio::time::timeout(HYDRATE_LIST_TIMEOUT, list_client.list_agents()).await
        }) {
            Ok(Ok(records)) => records,
            Ok(Err(e)) => {
                return Err(PaneError::CommandFailed(format!(
                    "Pane {pane_id} is not attached here and the daemon could not be \
                     queried for it: {e}"
                )));
            }
            Err(_) => {
                return Err(PaneError::CommandFailed(format!(
                    "Pane {pane_id} is not attached here and the daemon did not answer \
                     within {}s",
                    HYDRATE_LIST_TIMEOUT.as_secs()
                )));
            }
        };
        let Some(agent_id) = records
            .into_iter()
            .find(|r| r.pane_id_env.as_deref() == Some(pane_id))
            .map(|r| r.id)
        else {
            tracing::debug!(
                pane_id,
                "close_pane: no local pane and no daemon agent for this pane id — \
                 nothing to stop, treating the close as complete"
            );
            return Ok(());
        };
        let stop_client = self.client.clone();
        let stop_id = agent_id.clone();
        match self.runtime.block_on(async move {
            tokio::time::timeout(CTRL_W_STOP_TIMEOUT, stop_client.stop_agent(&stop_id)).await
        }) {
            Ok(Ok(())) => {
                tracing::info!(
                    pane_id,
                    agent_id,
                    "close_pane: stopped a daemon-spawned agent this TUI had not attached"
                );
                Ok(())
            }
            // Already gone between the list and the stop — the close is done.
            Ok(Err(e)) if is_agent_not_found(&e, &agent_id) => Ok(()),
            Ok(Err(e)) => Err(PaneError::CommandFailed(format!(
                "Failed to stop agent {agent_id} behind pane {pane_id}: {e}"
            ))),
            Err(_) => Err(PaneError::CommandFailed(format!(
                "stop-agent for {agent_id} (pane {pane_id}) timed out after {}s",
                CTRL_W_STOP_TIMEOUT.as_secs()
            ))),
        }
    }

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

/// PRD #241 M2: is this `stop-agent` failure the daemon telling us that
/// **this exact agent id** is already gone?
///
/// The daemon's only agent-scoped not-found condition is
/// [`AgentPtyError::NotFound`](crate::agent_pty::AgentPtyError), whose `Display`
/// is `"Agent {id} not found"`. `handle_request`'s `StopAgent` arm passes that
/// string through verbatim into `AttachResponse::err`, and `stop_agent` wraps it
/// verbatim into `ClientError::Server` — so the whole message, for the id we
/// actually sent, is what this predicate matches.
///
/// It is deliberately an EXACT match on that rendering rather than a
/// `contains("not found")` substring test (PRD #241 review F3a). The loose form
/// also swallowed unrelated server errors — `"Pane 3 not found"`,
/// `"session not found"`, a wrapped `"file not found"` from anything the stop
/// path might grow — and classifying one of those as "already stopped" silently
/// discards a **live** pane. Binding the match to the requested id also means a
/// not-found reported for some *other* agent can never authorize dropping this
/// one.
///
/// Kept as the ONE place the string is sniffed. A typed protocol error would be
/// better still, but it does not remove the string match: a newer TUI must keep
/// understanding an older daemon's message, so the sniff would survive as the
/// compatibility path while the typed variant moved the wire shape and forced a
/// `PROTOCOL_VERSION` bump this PRD deliberately does not take. Narrowing the
/// predicate buys the safety; the wire change would only add surface.
fn is_agent_not_found(err: &crate::daemon_client::ClientError, agent_id: &str) -> bool {
    match err {
        crate::daemon_client::ClientError::Server(msg) => msg
            .trim()
            .eq_ignore_ascii_case(&format!("Agent {agent_id} not found")),
        _ => false,
    }
}

/// PRD #241 F3b: the per-pane I/O task is streaming from a live attach session.
const IO_ATTACHED: u8 = 0;
/// PRD #241 F3b: the attach session ended and the task is inside
/// [`resolve_and_reattach`], hunting for the agent that replaced this pane's
/// (typically respawned) one.
const IO_REATTACHING: u8 = 1;
/// PRD #241 F3b: the task has exited — it will never adopt another agent for
/// this pane.
const IO_FINISHED: u8 = 2;

/// PRD #241 F3b: worst-case wall clock for one F9 `clear = true` respawn to hand
/// the pane slot over to its replacement — the old child's SIGTERM-to-exit (up to
/// [`AGENT_TERMINATE_GRACE`](crate::agent_pty::AGENT_TERMINATE_GRACE) = 3 s, and
/// the longer pathological case where SIGTERM is trapped) plus the replacement
/// process's startup. Observed at up to ~5 s for Claude Code under devbox; see
/// [`REATTACH_LOOKUP_TOTAL_BUDGET`], which was already sized against this same
/// measurement.
///
/// **Both windows that guard this one race derive from this constant** —
/// [`CLOSE_SLOT_SETTLE_BUDGET`] (how long a close waits for the replacement to
/// show up before declaring the slot empty) and [`REATTACH_LOOKUP_TOTAL_BUDGET`]
/// (how long the pane's I/O task hunts for that same replacement). Review finding
/// G1: they used to be independent magic numbers — 3.5 s here against a
/// documented ~5 s worst case there — so a slow respawn could outlive the close's
/// window, get declared "slot empty", and keep running with its card gone. Two
/// constants guarding one race must not contradict each other, so the ordering is
/// pinned at compile time below.
const RESPAWN_SLOT_HANDOVER_WORST_CASE: Duration = Duration::from_secs(5);

/// PRD #241 F3b: margin added on top of [`RESPAWN_SLOT_HANDOVER_WORST_CASE`] for
/// [`CLOSE_SLOT_SETTLE_BUDGET`], covering the `list-agents` round-trip that
/// observes the replacement plus one [`CLOSE_SLOT_POLL_INTERVAL`] of poll
/// granularity. Being generous costs a slightly longer wait on a rare path;
/// being stingy orphans an agent.
const CLOSE_SLOT_SETTLE_MARGIN_MS: u64 = 500;

/// PRD #241 F3b: how long [`EmbeddedPaneController::close_pane`] keeps asking
/// the daemon "who owns this pane slot now?" after both `stop-agent` attempts
/// answered *agent not found* and the pane's I/O task is mid-reattach.
///
/// The window exists because `respawn_agent_for_pane` (the F9 `clear = true`
/// delegate flow) removes the old agent from the registry and drops its PTY
/// master — which is what ends the attach stream and puts the I/O task into
/// [`IO_REATTACHING`] — then SIGTERMs the child with up to
/// `AGENT_TERMINATE_GRACE` (3 s) of grace *before* spawning the replacement.
/// For that whole stretch the daemon truthfully reports both "the id you asked
/// about does not exist" and "no agent occupies this pane", so a single
/// snapshot is not evidence the slot is empty: declaring the close complete
/// there drops the card while the replacement comes up behind it and keeps
/// running unattended.
///
/// Sized as [`RESPAWN_SLOT_HANDOVER_WORST_CASE`] plus
/// [`CLOSE_SLOT_SETTLE_MARGIN_MS`] rather than as its own number, so it can never
/// again fall short of the respawn the reattach loop is simultaneously waiting
/// out.
const CLOSE_SLOT_SETTLE_BUDGET: Duration = Duration::from_millis(
    RESPAWN_SLOT_HANDOVER_WORST_CASE.as_millis() as u64 + CLOSE_SLOT_SETTLE_MARGIN_MS,
);

/// PRD #241 F3b: the much shorter settle window used while the I/O task still
/// reports [`IO_ATTACHED`].
///
/// A respawn ends the attach stream at its very start, so an attached task
/// means no respawn is in flight — except for the propagation gap between the
/// daemon dropping the PTY master and our reader observing the end, which is
/// local-socket latency. Two `stop-agent` round-trips and a `list-agents` have
/// already elapsed by the time we get here; 300 ms of re-checking is several
/// more round-trips of margin, without making the ordinary ghost-card close
/// (agent long gone, attach socket still nominally open) pay the full respawn
/// budget.
const CLOSE_SLOT_ATTACHED_GRACE: Duration = Duration::from_millis(300);

/// PRD #241 F3b: gap between slot-occupancy polls inside
/// [`CLOSE_SLOT_SETTLE_BUDGET`].
const CLOSE_SLOT_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// PRD #241 F3b: bound on the single `list_agents` round-trip used to resolve
/// the pane slot's current occupant. Same reasoning as
/// [`HYDRATE_LIST_TIMEOUT`], but much tighter: `close_pane` runs on the render
/// thread via `block_on`, and the daemon has just answered two `stop-agent`
/// RPCs, so it is demonstrably alive.
const CLOSE_SLOT_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

/// PRD #241 F3b (review finding G3): worst-case wall clock for one *resolution
/// round* — the `list-agents` that names the pane slot's current occupant plus
/// the `stop-agent` sent to that occupant.
const CLOSE_SLOT_RESOLVE_ROUND_WORST_CASE: Duration = Duration::from_millis(
    CLOSE_SLOT_LOOKUP_TIMEOUT.as_millis() as u64 + CTRL_W_STOP_TIMEOUT.as_millis() as u64,
);

/// PRD #241 F3b (review finding G3): hard cap on how many *replacement* agents
/// one close will chase before it stops guessing and says so.
///
/// One round is the pre-G3 behaviour (find the replacement, stop it); the extra
/// rounds cover a respawn chain where each replacement is itself replaced before
/// our `stop-agent` lands. Past that the slot is changing owners faster than a
/// client can follow it, and chasing further only trades a longer render-thread
/// block for the same uncertainty — so the close completes and is *announced*
/// instead (see [`slot_churn_outcome`]).
const CLOSE_SLOT_RESOLVE_MAX_ROUNDS: u32 = 3;

/// PRD #241 F3b (review finding G3): total wall-clock budget for resolving who
/// owns the pane slot, covering **all** rounds together — the iteration cap
/// alone would not bound the work, because each round can spend a
/// `list-agents` timeout plus a `stop-agent` timeout.
///
/// Sized as the longest path a close could already take before G3 existed: a
/// full [`CLOSE_SLOT_SETTLE_BUDGET`] of polling, one more
/// [`CLOSE_SLOT_POLL_INTERVAL`] of granularity, then one
/// [`CLOSE_SLOT_RESOLVE_ROUND_WORST_CASE`] to find and stop the single
/// replacement that appears at the very end of the window (the
/// `lifecycle/stop/009` shape). So the unchurned case always fits and is never
/// cut short into a false "unverified" — the bound only ever bites on genuine
/// churn, and the worst-case block on this path is unchanged by G3; it is
/// merely named and enforced now instead of being an emergent sum.
const CLOSE_SLOT_RESOLVE_TOTAL_BUDGET: Duration = Duration::from_millis(
    CLOSE_SLOT_SETTLE_BUDGET.as_millis() as u64
        + CLOSE_SLOT_POLL_INTERVAL.as_millis() as u64
        + CLOSE_SLOT_RESOLVE_ROUND_WORST_CASE.as_millis() as u64,
);

/// PRD #241 F3b: what the close path decided to do about the daemon-side agent.
/// Replaces the previous `Result<Result<(), ClientError>, Elapsed>` pair, which
/// could no longer express the extra outcomes the slot check produces (a
/// *replacement* agent was stopped instead; the slot was proven empty).
enum StopOutcome {
    /// The daemon-side agent for this pane is gone — either `stop-agent`
    /// succeeded, or the daemon proved nothing occupies the pane slot. Teardown
    /// may complete.
    Done,
    /// PRD #241 F3b (review finding G2): teardown completes, but nothing *proved*
    /// the pane slot empty — `list-agents` was unusable during the respawn window,
    /// or (finding G3) the slot kept changing owners until the bounded resolution
    /// loop ran out of budget (see [`resolve_pane_slot_after_not_found`] for why
    /// retaining the pane here would be worse). Complete the close AND surface
    /// the carried message: a close that could not determine whether an agent is
    /// still running must never be silent, which is the same reason
    /// [`Self::Failed`] retains the pane rather than degrading to a detach.
    DoneUnverified(String),
    /// A genuine failure: the agent may still be alive. Retain the pane and
    /// surface this message.
    Failed(String),
    /// The stop RPC never answered.
    TimedOut,
}

/// PRD #241 F3: what one `stop-agent` attempt means. Split out from
/// [`StopOutcome`] because `NotFound` is not (yet) an outcome — it is the
/// signal to keep looking.
enum StopClass {
    /// The daemon acknowledged the stop.
    Stopped,
    /// The daemon reports THIS id does not exist (see [`is_agent_not_found`]).
    NotFound,
    /// Any other server/transport error.
    Failed(String),
    /// The RPC did not answer within [`CTRL_W_STOP_TIMEOUT`].
    TimedOut,
}

impl StopClass {
    /// Collapse to the caller-visible outcome, or `None` when the daemon says
    /// this id does not exist.
    ///
    /// PRD #241 F3b (review finding G3): `NotFound` deliberately has **no**
    /// outcome. It used to collapse to [`StopOutcome::Done`], which is what let
    /// a stop that lost a race to yet another respawn report the close as
    /// complete while the successor kept running — the same silent-orphan bug
    /// as G1/G2, one level deeper. Returning `None` makes "the id I asked about
    /// is gone" un-representable as a finished close: every call site must
    /// either keep resolving the slot or announce that it could not.
    fn resolved(self) -> Option<StopOutcome> {
        match self {
            StopClass::Stopped => Some(StopOutcome::Done),
            StopClass::NotFound => None,
            StopClass::Failed(msg) => Some(StopOutcome::Failed(msg)),
            StopClass::TimedOut => Some(StopOutcome::TimedOut),
        }
    }
}

/// PRD #241 F3: classify one bounded `stop_agent` attempt against the id it was
/// sent for. The id matters — [`is_agent_not_found`] only accepts the daemon's
/// agent-scoped not-found for exactly this agent.
fn classify_stop(
    result: Result<Result<(), crate::daemon_client::ClientError>, tokio::time::error::Elapsed>,
    agent_id: &str,
) -> StopClass {
    match result {
        Ok(Ok(())) => StopClass::Stopped,
        Ok(Err(e)) if is_agent_not_found(&e, agent_id) => StopClass::NotFound,
        Ok(Err(e)) => StopClass::Failed(e.to_string()),
        Err(_) => StopClass::TimedOut,
    }
}

/// PRD #241 F3b (review finding G2): the user-visible text for a close that
/// COMPLETED without being able to verify the daemon side.
///
/// Says three things, in the order a user needs them: the pane *is* closed (so
/// the vanished card is not itself a bug), why we could not check, and what may
/// still be running plus what to do about it. Restarting the deck re-hydrates
/// every daemon-side agent into a card (`hydrate_from_daemon`), so a survivor
/// becomes visible and closable again; stopping the daemon is the blunt option.
///
/// Kept in one place so every arm that cannot verify a close words it
/// identically and a test can pin the behaviour rather than N copies of a
/// format string. `reason` is the middle clause — what stopped us from
/// verifying — and reads directly after "Closed pane N but".
fn unverified_close_warning(pane_id_env: &str, reason: &str) -> String {
    format!(
        "Closed pane {pane_id_env} but {reason} — an agent may still be running unattended; \
         restart the deck to reattach it, or stop the daemon"
    )
}

/// PRD #241 F3b (review finding G3): the close ran out of budget while chasing
/// a pane slot that kept changing owners.
///
/// The daemon answered every question we asked — this is not the G2
/// "cannot query the daemon" case — but the answer kept changing under us:
/// each agent we were told owns the slot had already been replaced by the time
/// our `stop-agent` arrived. Teardown still completes (retaining the pane would
/// re-wedge issue #218's ghost card), so the user is told instead, exactly as on
/// the G2 paths.
fn slot_churn_warning(pane_id_env: &str) -> String {
    unverified_close_warning(
        pane_id_env,
        "the pane slot kept changing owners, so the close could not be verified (each replacement \
         was itself replaced before the close could stop it)",
    )
}

/// PRD #241 F3b (review finding G3): log the exhausted bound with enough detail
/// to tell the two bounds apart in a trace, and return the one announced
/// outcome the user sees for either.
fn slot_churn_outcome(pane_id_env: &str, replacement_stops: u32, elapsed: Duration) -> StopOutcome {
    tracing::warn!(
        pane_id = %pane_id_env,
        replacement_stops,
        elapsed_ms = elapsed.as_millis() as u64,
        max_rounds = CLOSE_SLOT_RESOLVE_MAX_ROUNDS,
        budget_ms = CLOSE_SLOT_RESOLVE_TOTAL_BUDGET.as_millis() as u64,
        "close_pane: the pane slot kept changing owners — completing the close without confirming \
         the slot is empty"
    );
    StopOutcome::DoneUnverified(slot_churn_warning(pane_id_env))
}

/// PRD #241 F3b: both `stop-agent` attempts said the agent does not exist —
/// decide whether the pane is genuinely agent-less or whether a *replacement*
/// has taken over its slot.
///
/// The daemon is authoritative about which agent (if any) currently carries
/// this `pane_id_env`, so ask it:
///
/// * **an occupant we have not already stopped** → that is the replacement the
///   F9 respawn produced. Stop *it*; its result is the close's result. This is
///   the orphan the old code created by returning `Ok(())` and dropping the
///   pane out from under a live agent. Review finding G3: unless that stop
///   *also* comes back id-scoped not-found, which means the replacement was
///   itself replaced between our `list-agents` and our `stop-agent` — so the
///   answer is to resolve the slot **again**, not to call the close done.
/// * **an occupant we already stopped** → the daemon contradicts itself
///   (`stop` says gone, `list` says present). Nothing further to kill; treat
///   the stop as done.
/// * **no occupant** → the slot is empty; how long we keep re-checking before
///   believing it depends on what the pane's I/O task is doing.
///   [`IO_FINISHED`] is immediate (it can no longer adopt anything, so nothing
///   can be orphaned), [`IO_REATTACHING`] gets the full
///   [`CLOSE_SLOT_SETTLE_BUDGET`] (a respawn inside its SIGTERM grace shows an
///   empty slot for up to `AGENT_TERMINATE_GRACE`), and [`IO_ATTACHED`] gets
///   only [`CLOSE_SLOT_ATTACHED_GRACE`]. The window is recomputed every pass,
///   so a task that transitions attached → reattaching mid-poll extends it, and
///   it is measured from the last handover we witnessed rather than from the
///   start of the close — "the slot has looked empty for a whole handover" is
///   the claim, and a stop that loses to a further respawn restarts it.
/// * **`list_agents` unusable** → no positive evidence of a replacement exists.
///   Fall back to the plain already-stopped reading, which is exactly the
///   behaviour without this check; the alternative — retaining the pane — would
///   re-wedge the ghost card that issue #218 reported, in exchange for guarding
///   a replacement we have no reason to believe exists. Review finding G2: the
///   close still completes, but it returns [`StopOutcome::DoneUnverified`] so the
///   user is *told* it completed blind — a replacement that starts after the
///   failed lookup would otherwise run unattended with no signal at all, the very
///   silence the pane-retaining failure path exists to prevent.
///
/// Review finding G3: because the third bullet feeds back into the first, this
/// is a **bounded re-resolution loop** rather than a fixed number of steps —
/// TOCTOU against a remote daemon has no depth limit a client can assume, so
/// nesting depth is handled by one code path instead of by special-casing each
/// newly-noticed level. Two explicit bounds keep the *total* work finite:
/// [`CLOSE_SLOT_RESOLVE_MAX_ROUNDS`] replacement stops, and
/// [`CLOSE_SLOT_RESOLVE_TOTAL_BUDGET`] of wall clock across all rounds
/// (enforced both by the checks in the loop and, structurally, by the timeout
/// wrapped around it here). Exhausting either completes the teardown and
/// returns [`slot_churn_outcome`] — never a silent `Done`.
async fn resolve_pane_slot_after_not_found(
    client: &DaemonClient,
    pane_id_env: &str,
    already_stopped: [String; 2],
    io_state: &AtomicU8,
) -> (String, StopOutcome) {
    // Owned out here so the id survives the hard-deadline arm below, which
    // cancels the loop mid-round and therefore cannot return it.
    let mut last_tried = already_stopped[1].clone();
    let chased = tokio::time::timeout(
        CLOSE_SLOT_RESOLVE_TOTAL_BUDGET,
        chase_pane_slot_owner(
            client,
            pane_id_env,
            already_stopped,
            io_state,
            &mut last_tried,
        ),
    )
    .await;
    match chased {
        Ok(outcome) => (last_tried, outcome),
        // Reachable only once a churn round has re-armed the settle window past
        // the budget: a replacement discovered near the deadline can start a
        // `stop-agent` that would run beyond it. Cancelling that stop mid-flight
        // is the point — the budget is a ceiling on how long Ctrl+W blocks the
        // render thread — and the announced outcome is the honest one, because
        // we no longer know whether the stop landed. Keeping the bound here
        // rather than only in the loop's arithmetic also means a later edit to
        // any single arm cannot quietly unbound the whole path.
        Err(_) => {
            tracing::warn!(
                pane_id = %pane_id_env,
                budget_ms = CLOSE_SLOT_RESOLVE_TOTAL_BUDGET.as_millis() as u64,
                "close_pane: pane-slot resolution hit its total budget mid-round — completing the \
                 close without confirming the slot is empty"
            );
            (
                last_tried,
                StopOutcome::DoneUnverified(slot_churn_warning(pane_id_env)),
            )
        }
    }
}

/// PRD #241 F3b: the re-resolution loop behind
/// [`resolve_pane_slot_after_not_found`], which owns its bounds and its docs.
///
/// Writes the last id it sent a `stop-agent` to into `last_tried` as it goes,
/// so the caller can still name it after cancelling this future at the deadline.
async fn chase_pane_slot_owner(
    client: &DaemonClient,
    pane_id_env: &str,
    already_stopped: [String; 2],
    io_state: &AtomicU8,
    last_tried: &mut String,
) -> StopOutcome {
    let mut already_stopped: Vec<String> = already_stopped.into();
    let started = tokio::time::Instant::now();
    // The settle window asks "has the slot looked empty for a whole respawn
    // handover?", so it is measured from the last handover we witnessed — not
    // from the start of the close. A stop that loses to a further respawn is
    // fresh evidence that a handover is in flight, so it re-arms this.
    let mut handover_at = started;
    let mut replacement_stops: u32 = 0;
    loop {
        let listed = tokio::time::timeout(CLOSE_SLOT_LOOKUP_TIMEOUT, client.list_agents()).await;
        let occupant = match listed {
            Ok(Ok(records)) => records
                .into_iter()
                .find(|r| r.pane_id_env.as_deref() == Some(pane_id_env))
                .map(|r| r.id),
            Ok(Err(e)) => {
                tracing::warn!(
                    pane_id = %pane_id_env,
                    error = %e,
                    "close_pane: cannot confirm the pane slot is empty (list-agents failed); \
                     accepting the daemon's 'agent not found' as an already-stopped close"
                );
                return StopOutcome::DoneUnverified(unverified_close_warning(
                    pane_id_env,
                    &format!("could not query the daemon (list-agents: {e})"),
                ));
            }
            Err(_) => {
                tracing::warn!(
                    pane_id = %pane_id_env,
                    timeout_ms = CLOSE_SLOT_LOOKUP_TIMEOUT.as_millis() as u64,
                    "close_pane: cannot confirm the pane slot is empty (list-agents timed out); \
                     accepting the daemon's 'agent not found' as an already-stopped close"
                );
                return StopOutcome::DoneUnverified(unverified_close_warning(
                    pane_id_env,
                    &format!(
                        "could not query the daemon (list-agents timed out after {}s)",
                        CLOSE_SLOT_LOOKUP_TIMEOUT.as_secs()
                    ),
                ));
            }
        };

        match occupant {
            Some(id) if !already_stopped.contains(&id) => {
                replacement_stops += 1;
                tracing::info!(
                    pane_id = %pane_id_env,
                    replacement_agent_id = %id,
                    replacement_stops,
                    "close_pane: a replacement agent now owns this pane slot — stopping it \
                     instead of orphaning it"
                );
                let stop = tokio::time::timeout(CTRL_W_STOP_TIMEOUT, client.stop_agent(&id)).await;
                let class = classify_stop(stop, &id);
                last_tried.clone_from(&id);
                if let Some(outcome) = class.resolved() {
                    return outcome;
                }
                // PRD #241 F3b (review finding G3): id-scoped not-found for the
                // *replacement* — a further respawn took the slot between the
                // `list-agents` above and this stop. That is not a finished
                // close, it is the same question one level down, so ask it
                // again with this id added to the set we have already tried.
                // The handover clock re-arms because a fresh handover is
                // demonstrably in flight.
                already_stopped.push(id);
                handover_at = tokio::time::Instant::now();
                if replacement_stops >= CLOSE_SLOT_RESOLVE_MAX_ROUNDS
                    || started.elapsed() + CLOSE_SLOT_RESOLVE_ROUND_WORST_CASE
                        > CLOSE_SLOT_RESOLVE_TOTAL_BUDGET
                {
                    return slot_churn_outcome(pane_id_env, replacement_stops, started.elapsed());
                }
            }
            Some(_) => return StopOutcome::Done,
            None => {
                let settle = match io_state.load(Ordering::SeqCst) {
                    // No respawned agent will ever be adopted for this pane, so
                    // an empty slot is final.
                    IO_FINISHED => Duration::ZERO,
                    IO_REATTACHING => CLOSE_SLOT_SETTLE_BUDGET,
                    _ => CLOSE_SLOT_ATTACHED_GRACE,
                };
                if handover_at.elapsed() >= settle {
                    tracing::debug!(
                        pane_id = %pane_id_env,
                        settle_ms = settle.as_millis() as u64,
                        replacement_stops,
                        "close_pane: pane slot stayed empty for the whole settle window — \
                         treating the close as complete"
                    );
                    return StopOutcome::Done;
                }
                // Only reachable once a churn round has re-armed the settle
                // window past the total budget: with `replacement_stops == 0`
                // the check above returns first, because the budget is a full
                // settle window plus a whole round (pinned below).
                if started.elapsed() + CLOSE_SLOT_POLL_INTERVAL + CLOSE_SLOT_LOOKUP_TIMEOUT
                    > CLOSE_SLOT_RESOLVE_TOTAL_BUDGET
                {
                    return slot_churn_outcome(pane_id_env, replacement_stops, started.elapsed());
                }
                tokio::time::sleep(CLOSE_SLOT_POLL_INTERVAL).await;
            }
        }
    }
}

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
///
/// PRD #241 F3b (review finding G1): expressed as twice
/// [`RESPAWN_SLOT_HANDOVER_WORST_CASE`] — the same worst case
/// [`CLOSE_SLOT_SETTLE_BUDGET`] is derived from, doubled for retry margin. The
/// value is unchanged (10 s); what changed is that the close path's window and
/// this one now move together, because they wait out the *same* respawn.
const REATTACH_LOOKUP_TOTAL_BUDGET: Duration =
    Duration::from_millis(2 * RESPAWN_SLOT_HANDOVER_WORST_CASE.as_millis() as u64);

// PRD #241 F3b (review finding G1): the ordering of these three windows is
// load-bearing, not stylistic — pin it at compile time so a future edit to any
// one of them cannot silently re-open the orphaned-replacement race.
const _: () = assert!(
    CLOSE_SLOT_SETTLE_BUDGET.as_millis() >= RESPAWN_SLOT_HANDOVER_WORST_CASE.as_millis(),
    "close_pane must keep polling the pane slot for at least as long as a respawn can take to \
     hand it over, or a replacement appearing later is orphaned with its card gone"
);
const _: () = assert!(
    REATTACH_LOOKUP_TOTAL_BUDGET.as_millis() >= CLOSE_SLOT_SETTLE_BUDGET.as_millis(),
    "the pane I/O task must still be willing to adopt a replacement for at least as long as \
     close_pane waits for one, or the close outlasts the only task that could attach to it"
);

// PRD #241 F3b (review finding G3): the re-resolution loop's bounds. Both are
// pinned so shrinking the budget can never turn the ordinary single-respawn
// close into a false "could not verify", and so the loop always gets at least
// the one round that was the pre-G3 behaviour.
const _: () = assert!(
    CLOSE_SLOT_RESOLVE_TOTAL_BUDGET.as_millis()
        >= CLOSE_SLOT_SETTLE_BUDGET.as_millis()
            + CLOSE_SLOT_POLL_INTERVAL.as_millis()
            + CLOSE_SLOT_RESOLVE_ROUND_WORST_CASE.as_millis(),
    "pane-slot resolution must be able to wait out a whole respawn handover AND still afford one \
     worst-case stop of the replacement it finds, or a close that verified fine before would now \
     report itself unverified"
);
const _: () = assert!(
    CLOSE_SLOT_RESOLVE_MAX_ROUNDS >= 1,
    "a close must be allowed to stop at least one replacement agent, or the replacement-aware \
     close path is disabled and every respawn during a close orphans its agent"
);

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

/// PRD #611 (audit finding): does this reattach hand the pane a **different
/// child process** than the one the pane's terminal state was derived from?
///
/// Each attach session gets a fresh [`MouseModeScanner`], but the pane's mouse
/// flag is an `AtomicBool` that outlives the session — so without this a
/// replacement agent inherits its predecessor's answer and can be sent wheel
/// reports it never requested.
///
/// The two cases genuinely differ and a blanket clear would get one of them
/// wrong. A **same-PTY reconnect** (the daemon closed the stream, the child is
/// the same process) must PRESERVE the flag: the child's DECSET is long past,
/// and a snapshot replay that does not re-emit it would silently stop the wheel
/// working on a live claude pane — a common regression traded for a rare
/// injection. A **replacement attach** (F9's `clear=true` respawn: the old agent
/// died and the daemon spawned a new one for this pane) must CLEAR it: that
/// child has requested nothing yet, and whatever it does request arrives on the
/// new stream.
///
/// The daemon issues one id per spawned agent, so the ids are exactly that
/// distinction and it is free at the reattach site. An empty previous id (only
/// an inert seam backend, which runs no I/O task) is not evidence of anything,
/// so it preserves — the conservative direction.
fn reattach_replaces_the_child(previous_agent_id: &str, new_agent_id: &str) -> bool {
    !previous_agent_id.is_empty() && previous_agent_id != new_agent_id
}

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
    bytes_since_spawn: Arc<AtomicU64>,
    stream_rejections: Arc<Mutex<Vec<(String, String)>>>,
    io_state: Arc<AtomicU8>,
    lost: Arc<Mutex<Option<PaneLostReason>>>,
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
                // PRD #611 M3: per-stream, alongside `osc8` and for the same
                // reason — both hold the tail of a sequence a PTY read boundary
                // cut in half, and neither means anything to a different stream.
                let mut mouse_scan = MouseModeScanner::default();
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
                                    &mut mouse_scan,
                                    &hyperlinks,
                                    &bytes_since_spawn,
                                );
                            }
                            crate::daemon_protocol::KIND_STREAM_END => break,
                            // PRD #20 R20-007 (finding #10): a typed, NON-terminal
                            // input rejection — the daemon refused a key/paste
                            // frame because the target went non-live / exited /
                            // rebound. Record `(pane_id, reason)` for the render
                            // loop to surface + leave PaneInput; DO NOT break, the
                            // stream stays open (output keeps flowing).
                            crate::daemon_protocol::KIND_STREAM_REJECT => {
                                let reason = String::from_utf8_lossy(&bytes).into_owned();
                                stream_rejections
                                    .lock()
                                    .unwrap()
                                    .push((pane_id.clone(), reason));
                            }
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
                // `warn!`, not `debug!`: this is a terminal, user-visible
                // outcome — the pane stops accepting input for the rest of the
                // session. At `debug!` (and with file logging off unless
                // `DOT_AGENT_DECK_LOG` is set) a report of "the pane died" left
                // no evidence of WHICH give-up fired, and the two have
                // completely different causes.
                tracing::warn!(
                    pane_id = %pane_id,
                    reason = "empty-sessions",
                    consecutive_empty_sessions,
                    "auto-reattach: agent respawned but produced no output; giving up on this pane"
                );
                *lost.lock().unwrap() = Some(PaneLostReason::AgentKeptCrashing);
                break 'outer;
            }
        }

        // PRD #241 F3b: publish "a replacement may be coming" for the whole
        // lookup. A concurrent `close_pane` that gets `agent not found` for the
        // id it holds must wait out this window instead of dropping the pane
        // on top of the agent the daemon is about to hand us.
        io_state.store(IO_REATTACHING, Ordering::SeqCst);
        match resolve_and_reattach(&client, &pane_id).await {
            Ok((new_agent_id, new_conn)) => {
                tracing::debug!(
                    pane_id = %pane_id,
                    new_agent_id = %new_agent_id,
                    "auto-reattach: subscribed to new agent for pane"
                );
                {
                    let mut held = agent_id.lock().unwrap();
                    // PRD #611: the mouse flag describes a CHILD's requested
                    // reporting state, and this loop covers two situations the
                    // flag must be treated differently in. See
                    // `reattach_replaces_the_child`.
                    if reattach_replaces_the_child(&held, &new_agent_id) {
                        mouse_mode.store(false, Ordering::Relaxed);
                    }
                    *held = new_agent_id;
                }
                conn_opt = Some(new_conn);
                io_state.store(IO_ATTACHED, Ordering::SeqCst);
            }
            Err(gave_up) => {
                // See the `warn!` rationale above: terminal and user-visible.
                // `reason` now separates "the daemon stopped answering" from
                // "the daemon answered and this pane has no agent" — see
                // `ReattachGiveUp`. The counts ride along so a reader can tell a
                // clean single miss from a whole window of failed round-trips.
                tracing::warn!(
                    pane_id = %pane_id,
                    reason = gave_up.reason(),
                    list_attempts = gave_up.attempts,
                    list_errors = gave_up.list_errors,
                    trailing_list_errors = gave_up.trailing_list_errors,
                    attach_errors = gave_up.attach_errors,
                    trailing_attach_errors = gave_up.trailing_attach_errors,
                    // Selected by the same branch that picked `reason`, so the
                    // two always describe the same fault.
                    last_error = gave_up.reported_error(),
                    budget_secs = REATTACH_LOOKUP_TOTAL_BUDGET.as_secs(),
                    "auto-reattach: no live agent for pane within the retry window; giving up on this pane"
                );
                // NOTE: still `AgentGone` — the user-facing pane text is
                // deliberately unchanged here. A distinct daemon-unreachable
                // `PaneLostReason` would be a better message, but it changes
                // rendered pane/status strings and belongs with its own L1
                // snapshot coverage rather than riding along on a logging fix.
                *lost.lock().unwrap() = Some(PaneLostReason::AgentGone);
                break 'outer;
            }
        }
    }
}

/// Why a [`resolve_and_reattach`] lookup ran out its budget, so the caller's
/// terminal `warn!` can name the actual cause.
///
/// The give-up used to report a flat `reason = "no-live-agent"` for three
/// completely different failures, because the distinguishing detail was logged
/// only at `debug!` (off unless `DOT_AGENT_DECK_LOG` is set):
///
/// * every `list_agents` call FAILED — the daemon is unreachable/gone, and the
///   agents may well still be alive;
/// * `list_agents` answered and named a live agent for this pane, but every
///   `attach` to it failed — the agent exists and the daemon is reachable, so
///   the fault is in the attach/stream path; or
/// * `list_agents` answered fine and simply had no record for this pane — the
///   agent really is gone daemon-side.
///
/// All three have different remedies (reconnect the daemon vs. investigate the
/// attach path vs. reopen the pane), and telling them apart after the fact
/// mattered in a real incident: a daemon was terminated under seven live panes
/// and every pane reported the agent-gone wording, so the logs alone could not
/// show that ONE shared cause — not seven dying agents — was responsible.
///
/// The attach-failure case was a Greptile P1 on the first draft of this struct,
/// which counted only `list_agents` errors: a pane disconnected by a persistent
/// attach failure was reported as `no-live-agent`, i.e. blamed on an agent that
/// was demonstrably still registered.
struct ReattachGiveUp {
    /// Total `list_agents` round-trips attempted within the budget.
    attempts: u32,
    /// How many of those returned an error rather than a record set.
    list_errors: u32,
    /// CONSECUTIVE `list_agents` failures at the END of the window — reset to 0
    /// by every success. This, not [`Self::list_errors`], decides whether the
    /// daemon is the culprit: what matters is whether the daemon was answering
    /// when we gave up, not whether it ever answered.
    ///
    /// A second Greptile P1 caught the aggregate version: requiring
    /// `list_errors == attempts` meant a single early success — e.g. the very
    /// first lookup, before the respawning agent had registered — followed by the
    /// daemon dying for the rest of the window fell through to `no-live-agent`.
    /// That is the exact shape of the incident this diagnostic exists for, so the
    /// aggregate got the one case it most needed to get right backwards.
    trailing_list_errors: u32,
    /// How many times a matching agent WAS found but `attach` to it failed.
    attach_errors: u32,
    /// Attach failures that are still the LIVE story — cleared the moment a
    /// successful lookup reports no agent for this pane, because that answer is
    /// authoritative and supersedes an attach failure against an agent that has
    /// since disappeared.
    ///
    /// The aggregate version was a third Greptile P1 in the same family as
    /// [`Self::trailing_list_errors`]: one early attach failure outranked nine
    /// later authoritative "no such agent" answers and reported `attach-failing`
    /// for a pane whose agent had demonstrably gone.
    trailing_attach_errors: u32,
    /// Last `list_agents` transport error, cleared the moment the daemon answers.
    last_list_error: Option<String>,
    /// Last `attach` error, cleared by an authoritative no-agent answer.
    last_attach_error: Option<String>,
}

impl ReattachGiveUp {
    // THE RULE, since FOUR of the five review findings on this struct were the
    // same mistake in different clothes: everything reported reads ONLY the
    // trailing state — the most recent authoritative evidence — never the
    // aggregate counters. A fault that has since been superseded is history and
    // must not outrank what the last round-trip actually established. The
    // aggregates exist solely to enrich the log line.
    //
    // Concretely, each iteration updates the trailing state like this:
    //   * `list_agents` errored          → daemon is the live fault
    //   * agent found, `attach` errored   → attach path is the live fault
    //   * lookup fine, agent NOT found    → AUTHORITATIVE: the agent is gone,
    //                                       so both trailing faults are cleared
    //
    // "Everything reported" deliberately includes the error STRING, not just the
    // reason. Scoping this rule to the two predicates and leaving a single sticky
    // `last_error` was the fifth finding: a `no-live-agent` give-up could still
    // carry "attach refused", re-creating the exact ambiguity this struct exists
    // to remove. So the error is no longer stored once and hoped to match — it is
    // kept per-fault and selected by [`Self::reported_error`] from the SAME branch
    // that picks [`Self::reason`], which makes a mismatched pair unrepresentable
    // rather than merely discouraged.
    //
    // A new failure mode added here must extend that table and that selection, not
    // add another aggregate test alongside them.

    /// `true` when the window ENDED with the daemon not answering, i.e. the
    /// daemon — not the agent — is what went missing. Requires at least one
    /// attempt so an empty run can never masquerade as a daemon fault.
    ///
    /// A single trailing failure is deliberately enough. At give-up time the last
    /// thing known is that the daemon did not answer, and the alternative —
    /// blaming an agent whose absence was never actually observed — is the worse
    /// error. The exact counts ride along in the log for anyone who needs to tell
    /// a one-off blip from a sustained outage.
    fn daemon_unreachable(&self) -> bool {
        self.attempts > 0 && self.trailing_list_errors > 0
    }

    /// `true` when an attach failure is still the live story at give-up time.
    /// Checked AFTER [`Self::daemon_unreachable`], so a window that ended with the
    /// daemon silent is reported as the daemon's fault.
    fn attach_failing(&self) -> bool {
        self.trailing_attach_errors > 0
    }

    /// The `reason` field for the terminal give-up log.
    fn reason(&self) -> &'static str {
        if self.daemon_unreachable() {
            "daemon-unreachable"
        } else if self.attach_failing() {
            "attach-failing"
        } else {
            "no-live-agent"
        }
    }

    /// The error string that BELONGS to [`Self::reason`] — chosen by the same
    /// branch order, so the pair can never disagree. `no-live-agent` reports no
    /// error at all: the daemon answered and simply had no agent, which is an
    /// absence of failure, not a failure.
    fn reported_error(&self) -> &str {
        if self.daemon_unreachable() {
            self.last_list_error.as_deref().unwrap_or("none")
        } else if self.attach_failing() {
            self.last_attach_error.as_deref().unwrap_or("none")
        } else {
            "none"
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
/// trapped). Returns `Err(`[`ReattachGiveUp`]`)` — carrying WHY — if no live
/// agent matches the pane within the budget, or if every `attach` attempt fails.
async fn resolve_and_reattach(
    client: &DaemonClient,
    pane_id_env: &str,
) -> Result<(String, AttachConnection), ReattachGiveUp> {
    let start = tokio::time::Instant::now();
    let mut delay = REATTACH_LOOKUP_INITIAL_DELAY;
    let mut attempts: u32 = 0;
    let mut list_errors: u32 = 0;
    let mut trailing_list_errors: u32 = 0;
    let mut attach_errors: u32 = 0;
    let mut trailing_attach_errors: u32 = 0;
    // No initializers: every loop iteration assigns both before the `return Err`
    // below can read them (a successful lookup clears, a failure sets), so an
    // initial `None` would be provably dead — `-D warnings` rejects it.
    let mut last_list_error: Option<String>;
    let mut last_attach_error: Option<String> = None;
    loop {
        attempts = attempts.saturating_add(1);
        match client.list_agents().await {
            Ok(records) => {
                // The daemon answered, so it is reachable AS OF NOW. Anything it
                // failed to answer earlier is history and must not outrank the
                // current state when the reason is chosen — the error string goes
                // with the count, so neither can survive as stale evidence.
                trailing_list_errors = 0;
                last_list_error = None;
                let new_id_opt = records
                    .into_iter()
                    .find(|r| r.pane_id_env.as_deref() == Some(pane_id_env))
                    .map(|r| r.id);
                match new_id_opt {
                    None => {
                        // AUTHORITATIVE: the daemon answered and has no agent for
                        // this pane. That settles it — any earlier attach failure
                        // was against an agent that has since gone, so neither it
                        // nor its message may keep claiming the diagnosis.
                        trailing_attach_errors = 0;
                        last_attach_error = None;
                    }
                    Some(new_id) => match client.attach(&new_id).await {
                        Ok(conn) => return Ok((new_id, conn)),
                        Err(e) => {
                            // Counted, not just logged: the agent demonstrably
                            // EXISTS, so a give-up here must not be reported as
                            // `no-live-agent` and blamed on a missing agent.
                            attach_errors = attach_errors.saturating_add(1);
                            trailing_attach_errors = trailing_attach_errors.saturating_add(1);
                            last_attach_error = Some(e.to_string());
                            tracing::debug!(
                                agent_id = %new_id,
                                error = %e,
                                "auto-reattach: attach to new agent failed; retrying after backoff"
                            );
                        }
                    },
                }
            }
            Err(e) => {
                list_errors = list_errors.saturating_add(1);
                trailing_list_errors = trailing_list_errors.saturating_add(1);
                last_list_error = Some(e.to_string());
                tracing::debug!(
                    error = %e,
                    "auto-reattach: list_agents failed; retrying after backoff"
                );
            }
        }

        if start.elapsed() >= REATTACH_LOOKUP_TOTAL_BUDGET {
            return Err(ReattachGiveUp {
                attempts,
                list_errors,
                trailing_list_errors,
                attach_errors,
                trailing_attach_errors,
                last_list_error,
                last_attach_error,
            });
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

/// Which mouse-reporting protocol the child has selected, if any.
///
/// These four DEC private modes are **one mutually exclusive field**, not four
/// independent switches — exactly as the repo's own `vt100` 0.16.2 models them
/// (`Screen::set_mouse_mode` assigns `mouse_protocol_mode`, it does not or-in a
/// bit). Setting 1003 after 1000 leaves the child reporting any-motion and
/// nothing else, and a DECRST clears reporting only when it names the mode
/// currently in force (`Screen::clear_mouse_mode`).
///
/// 1004 is deliberately absent, and is not a protocol at all: it is focus
/// reporting, which codex sets on its own (PRD #611). Treating it as mouse would
/// break the exact case this PRD exists for.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum MouseProtocol {
    /// No reporting: nothing the child asked for will be sent to it.
    #[default]
    None,
    /// `9` — X10 compatibility mode, press only.
    Press,
    /// `1000` — normal tracking (VT200): press and release.
    PressRelease,
    /// `1002` — button-event tracking: press, release and drag.
    ButtonMotion,
    /// `1003` — any-event tracking: every motion, button or not.
    AnyMotion,
}

/// How the child expects a mouse report to be **encoded** — a separate field
/// from [`MouseProtocol`], and one that enables no reporting on its own.
///
/// This is the second half of `vt100`'s model (`set_mouse_encoding` /
/// `clear_mouse_encoding`), and the half a scanner that treats 1006 as "mouse is
/// on" gets wrong: `ESC[?1006h` by itself asks for SGR-encoded reports of a
/// protocol nobody has selected, which means no reports at all.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum MouseEncoding {
    /// The original X10 encoding: `ESC[M` plus three offset-by-32 bytes.
    #[default]
    Default,
    /// `1005` — UTF-8 extended coordinates.
    Utf8,
    /// `1006` — SGR extended: `ESC[<b;col;rowM`, the only encoding
    /// [`EmbeddedPaneController::forward_mouse_scroll`] emits.
    Sgr,
}

/// The protocol a DEC private mode number selects, if it selects one.
fn mouse_protocol_for_mode(mode: u32) -> Option<MouseProtocol> {
    match mode {
        9 => Some(MouseProtocol::Press),
        1000 => Some(MouseProtocol::PressRelease),
        1002 => Some(MouseProtocol::ButtonMotion),
        1003 => Some(MouseProtocol::AnyMotion),
        _ => None,
    }
}

/// The encoding a DEC private mode number selects, if it selects one.
fn mouse_encoding_for_mode(mode: u32) -> Option<MouseEncoding> {
    match mode {
        1005 => Some(MouseEncoding::Utf8),
        1006 => Some(MouseEncoding::Sgr),
        _ => None,
    }
}

/// Upper bound on the bytes [`MouseModeScanner`] carries between chunks. A
/// real private-mode sequence is a handful of bytes — `ESC[?1000;1002;1003;1006h`,
/// the longest shape any of these agents emits, is 24 — so 64 leaves room for
/// roughly a dozen parameters while keeping the ceiling nowhere near a PTY read.
/// It exists because the carry is driven by the child: without it, a stream that
/// opens `ESC[?` and never terminates it would grow the buffer for as long as
/// the agent kept talking.
const MOUSE_SCAN_CARRY_MAX: usize = 64;

/// Cross-chunk state for [`scan_mouse_mode`] (PRD #611 M3). Lives beside the
/// stream's [`Osc8Filter`] and for the same reason: both hold the tail of a
/// sequence a PTY read boundary cut in half, and neither means anything to a
/// different stream.
#[derive(Debug, Default)]
struct MouseModeScanner {
    /// The trailing bytes of the previous chunk that could still be the prefix
    /// of a private-mode sequence — never more than [`MOUSE_SCAN_CARRY_MAX`].
    carry: Vec<u8>,
    /// The one reporting protocol currently in force, if any.
    protocol: MouseProtocol,
    /// The encoding the child expects those reports in.
    encoding: MouseEncoding,
}

/// Derive `mouse_mode_enabled` from a chunk of PTY output (PRD #611 M3).
///
/// Scans `state`'s carry-over followed by `data` for DEC private-mode
/// sequences — `ESC [ ? <params> h` (set) and `ESC [ ? <params> l` (reset) —
/// and applies every directive naming a mouse protocol or encoding, **in byte
/// order**, exactly the way the repo's own `vt100` 0.16.2 does. `flag` is
/// written only when the chunk carried at least one such directive.
///
/// **The published flag is `protocol != None && encoding == Sgr`**, and the SGR
/// half is deliberate rather than incidental.
/// [`EmbeddedPaneController::forward_mouse_scroll`] only ever emits SGR bytes
/// (`ESC[<64;col;rowM`), so forwarding to a child that selected the default or
/// UTF-8 encoding injects bytes it will mis-parse. Not forwarding merely leaves
/// the deck scrolling its own buffer, which is harmless. The asymmetry decides
/// it: being too generous injects unrequested input into somebody else's
/// process, being too stingy costs a scroll.
///
/// Four things this replaced a substring search over four fixed patterns to
/// get right, each of which produced the same user-visible symptom — a pane
/// the deck refuses to scroll for an agent that would have scrolled fine, or
/// (worse) one it injects wheel reports into that never asked for them:
///
/// 1. The **combined** form. `ESC[?1000;1002;1006h` contains none of
///    `ESC[?1000h`, `ESC[?1002h`, `ESC[?1003h` or `ESC[?1006h` as a substring,
///    so an app that sets its modes in one sequence went entirely undetected.
///    Parameters are parsed as whole numbers, so `ESC[?11000h` still does not
///    match 1000, and 1004 — focus reporting — never counts.
/// 2. **Order.** The old scan tested every enable pattern before it considered
///    any disable, so an enable anywhere in a chunk outranked a disable later
///    in the same chunk. Position in the byte stream decides now.
/// 3. **Split sequences.** A PTY read boundary falls wherever the kernel had
///    bytes ready, so `ESC[?100` + `2h` is an ordinary pair of reads and was
///    invisible to a scan with no memory. The trailing bytes that could still
///    open a sequence are carried into the next chunk.
/// 4. **The shape of the state itself.** An intermediate version of this
///    function tracked one independent latch per mode and published `any()`
///    over them, which is not what a terminal does and is strictly worse than
///    the substring scan it replaced on a real sequence: `ESC[?1000h`
///    `ESC[?1002h` `ESC[?1002l` ends with reporting OFF (1002 superseded 1000,
///    and resetting the mode in force clears it), while four latches keep the
///    1000 latch set and hold the flag true forever. The protocol modes are one
///    mutually exclusive field and 1006 is an encoding selector that enables no
///    reporting at all — see [`MouseProtocol`] and [`MouseEncoding`].
fn scan_mouse_mode(data: &[u8], flag: &AtomicBool, state: &mut MouseModeScanner) {
    const ESC: u8 = 0x1b;

    // Taken rather than borrowed so `state` stays free for the latch writes
    // below; a new carry is stored at the end.
    let carry = std::mem::take(&mut state.carry);
    let joined: Vec<u8>;
    let buf: &[u8] = if carry.is_empty() {
        // The overwhelmingly common case, and the hot path: no copy at all.
        data
    } else {
        let mut v = Vec::with_capacity(carry.len() + data.len());
        v.extend_from_slice(&carry);
        v.extend_from_slice(data);
        joined = v;
        &joined
    };

    let mut touched = false;
    // Where a sequence that is still open when the buffer runs out began. Only
    // that suffix is worth carrying — everything before it has been decided.
    let mut partial_from: Option<usize> = None;
    let mut i = 0usize;

    while i < buf.len() {
        if buf[i] != ESC {
            i += 1;
            continue;
        }
        let seq_start = i;
        // `ESC [ ?` — the private-mode introducer. Running out mid-introducer
        // is a partial, not a miss; anything else here is some other escape
        // sequence, so resume scanning after the ESC.
        if i + 1 >= buf.len() {
            partial_from = Some(seq_start);
            break;
        }
        if buf[i + 1] != b'[' {
            i += 1;
            continue;
        }
        if i + 2 >= buf.len() {
            partial_from = Some(seq_start);
            break;
        }
        if buf[i + 2] != b'?' {
            i += 1;
            continue;
        }

        // Parameter list: `;`-separated decimal numbers, then a final byte.
        let params_start = i + 3;
        let mut j = params_start;
        while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
            j += 1;
        }
        if j >= buf.len() {
            partial_from = Some(seq_start);
            break;
        }
        if buf[j] == ESC {
            // A fresh introducer aborted this one (malformed output). Resync ON
            // it rather than consuming it, so a run of truncated sequences does
            // not swallow every other one.
            i = j;
            continue;
        }

        let set = match buf[j] {
            b'h' => Some(true),
            b'l' => Some(false),
            // Some other final byte: a private mode we do not care about, or a
            // request/report (`ESC[?1000$p`). Skip past it.
            _ => None,
        };
        if let Some(set) = set {
            for param in buf[params_start..j].split(|&b| b == b';') {
                // All-digit by construction; an empty or absurdly long
                // parameter simply names no mode we track.
                let mode = std::str::from_utf8(param)
                    .ok()
                    .and_then(|text| text.parse::<u32>().ok());
                let Some(mode) = mode else { continue };
                if let Some(protocol) = mouse_protocol_for_mode(mode) {
                    // One field, overwritten by a SET. A RESET clears reporting
                    // only when it names the protocol actually in force — an app
                    // withdrawing 1000 after it moved on to 1002 has withdrawn
                    // nothing (`vt100::Screen::clear_mouse_mode`).
                    if set {
                        state.protocol = protocol;
                    } else if state.protocol == protocol {
                        state.protocol = MouseProtocol::None;
                    }
                    touched = true;
                } else if let Some(encoding) = mouse_encoding_for_mode(mode) {
                    // Same shape, separate field: selecting an encoding turns no
                    // reporting on, and withdrawing one that is not in force
                    // turns none off.
                    if set {
                        state.encoding = encoding;
                    } else if state.encoding == encoding {
                        state.encoding = MouseEncoding::Default;
                    }
                    touched = true;
                }
            }
        }
        i = j + 1;
    }

    if let Some(from) = partial_from {
        let tail = &buf[from..];
        if tail.len() <= MOUSE_SCAN_CARRY_MAX {
            state.carry = tail.to_vec();
        }
        // Over the cap the carry is simply dropped. No real private-mode
        // sequence is anywhere near this long, so what is open is malformed or
        // hostile, and refusing it costs at most one missed directive — never a
        // mouse mode invented from bytes that were never seen whole.
    }

    if touched {
        // The question this flag answers is not "did any mouse mode ever appear"
        // but "will the SGR report `forward_mouse_scroll` emits be understood by
        // this child?" — which needs a protocol in force AND SGR selected to
        // carry it. See the SGR paragraph on this function.
        flag.store(
            state.protocol != MouseProtocol::None && state.encoding == MouseEncoding::Sgr,
            Ordering::Relaxed,
        );
    }
}

/// Scrollback depth (lines) for a pane's local vt100 parser. Used both when a
/// pane is created and when a contained parser panic forces a rebuild.
const PANE_SCROLLBACK_LINES: usize = 10_000;

thread_local! {
    /// Set while a `catch_unwind`-guarded vt100 feed is running on this thread
    /// (see [`guarded_parser_feed`]). The `run_tui` panic hook (`src/ui.rs`)
    /// reads it via [`in_guarded_parser_feed`] so a *contained* parser panic is
    /// not treated like a genuine fatal panic — i.e. it does not restore/tear
    /// down the live terminal and exit the TUI.
    static IN_GUARDED_PARSER_FEED: Cell<bool> = const { Cell::new(false) };
}

/// True while this thread is inside a guarded vt100 feed. The `run_tui` panic
/// hook uses this to suppress terminal teardown for a contained pane-processing
/// panic (a bug in the third-party `vt100` parser) rather than crashing the
/// whole TUI.
///
/// PRD #227 audit item C: that hook check is `#[cfg(panic = "unwind")]`, because
/// under `panic = "abort"` the panic is not contained — nothing can catch it and
/// the process dies, so skipping teardown would only leak the enhanced keyboard
/// mode. That leaves this getter with no caller in an abort build, hence the
/// conditional `dead_code` allowance.
#[cfg_attr(panic = "abort", allow(dead_code))]
pub(crate) fn in_guarded_parser_feed() -> bool {
    IN_GUARDED_PARSER_FEED.with(Cell::get)
}

/// Run `f` under [`catch_unwind`], flagging the thread for the duration so the
/// `run_tui` panic hook treats any panic as *contained*. Used to isolate
/// panics originating inside the `vt100` crate — notably the `col_wrap` row
/// underflow / out-of-bounds cell `unwrap()` (grid.rs) that fires on wide
/// characters in a very short (e.g. 1-row) pane. Callers MUST keep any held
/// `MutexGuard` *outside* `f` so a caught panic does not drop a guard
/// mid-unwind and poison the lock.
///
/// [`catch_unwind`]: std::panic::catch_unwind
fn guarded_parser_feed<T>(f: impl FnOnce() -> T) -> std::thread::Result<T> {
    IN_GUARDED_PARSER_FEED.with(|flag| flag.set(true));
    let result = std::panic::catch_unwind(AssertUnwindSafe(f));
    IN_GUARDED_PARSER_FEED.with(|flag| flag.set(false));
    result
}

/// Feed one OSC 8 segment into the vt100 parser, returning
/// `(rows_scrolled_off, optional (row, url) link)`. Split out from
/// [`process_agent_output_chunk`] so its whole body — every call that touches
/// the third-party parser — runs inside [`guarded_parser_feed`]'s
/// `catch_unwind`.
fn feed_segment(p: &mut vt100::Parser, segment: &Osc8Segment) -> (u16, Option<(u16, String)>) {
    // Recomputed per segment: only a resize changes it, and no resize happens
    // within a single chunk, so this matches the previous once-per-chunk value.
    let max_row = p.screen().size().0.saturating_sub(1);
    match segment {
        Osc8Segment::Text(bytes) => {
            let rb = p.screen().cursor_position().0;
            p.process(bytes);
            let ra = p.screen().cursor_position().0;
            let scrolled = if rb >= max_row && ra >= max_row {
                bytes.iter().filter(|&&b| b == b'\n').count() as u16
            } else {
                0
            };
            (scrolled, None)
        }
        Osc8Segment::LinkedText { url, bytes } => {
            let row = p.screen().cursor_position().0;
            p.process(bytes);
            let ra = p.screen().cursor_position().0;
            let scrolled = if row >= max_row && ra >= max_row {
                bytes.iter().filter(|&&b| b == b'\n').count() as u16
            } else {
                0
            };
            (scrolled, Some((row, url.clone())))
        }
    }
}

/// Feed a chunk of agent-output bytes through the OSC 8 filter, the vt100
/// parser, the mouse-mode scanner and the hyperlink map: the ONE place raw
/// agent bytes become pane render state.
///
/// It was written when two backends had to agree — a local-PTY reader thread
/// and the stream-backed I/O task — and it is worth being accurate that the
/// daemon-backed architecture has since removed the local reader, so
/// [`run_pane_io_task`] is now its only production caller. What that changes is
/// the *argument*, not the placement: a single chokepoint is still where a
/// derivation over the byte stream belongs, because every such derivation
/// (mouse mode, the OSC 8 link map, the since-spawn byte count) then has exactly
/// one implementation that a second reader path could not fork from, and any
/// backend added later inherits all of them by construction rather than by
/// somebody remembering.
///
/// `mouse_scan` carries whatever a PTY read boundary cut in half, and lives here
/// rather than on the pane for that reason (PRD #611 M3): parked on the pane it
/// is state each reader has to remember separately, and the one that forgot
/// would silently reintroduce the split-sequence defect.
///
/// The vt100 feed is wrapped in [`guarded_parser_feed`]: `vt100` 0.16.2 can
/// panic on malformed/edge-case output (e.g. wide characters in a 1-row pane),
/// and a panic on the stream-IO task must not crash the whole TUI. A panicking
/// chunk is dropped (that pane may render briefly stale) and processing
/// continues.
fn process_agent_output_chunk(
    data: &[u8],
    osc8: &mut Osc8Filter,
    parser: &Mutex<vt100::Parser>,
    mouse_mode: &AtomicBool,
    mouse_scan: &mut MouseModeScanner,
    hyperlinks: &Mutex<HyperlinkMap>,
    bytes_since_spawn: &AtomicU64,
) {
    // PRD #611 M2: counted here rather than at the caller for the same reason
    // this function is shared at all — both backends must derive identical
    // state from identical bytes, and "how much output has this parser been
    // given?" is exactly such a derivation. Counted before the feed, because a
    // panicking chunk's bytes are still bytes this parser was handed — and then
    // zeroed with the parser if that feed does panic, see the rebuild below.
    bytes_since_spawn.fetch_add(data.len() as u64, Ordering::Relaxed);

    scan_mouse_mode(data, mouse_mode, mouse_scan);

    let segments = osc8.process(data);
    let mut new_links: Vec<(u16, String)> = Vec::new();
    let mut scroll_amount: u16 = 0;

    let mut parser_reset = false;
    if let Ok(mut p) = parser.lock() {
        // Captured while the parser is known-good so a contained panic can
        // rebuild it at the same geometry. Rebuilt through `new_pane_parser`
        // (issue #363), so a parser that somehow holds a degenerate geometry is
        // replaced by a valid one instead of reproducing the degenerate grid.
        let (rows, cols) = p.screen().size();
        for segment in &segments {
            // The MutexGuard `p` is borrowed into the closure but lives in this
            // scope, so a caught panic does not poison the lock.
            match guarded_parser_feed(|| feed_segment(&mut p, segment)) {
                Ok((scrolled, link)) => {
                    scroll_amount += scrolled;
                    if let Some(link) = link {
                        new_links.push(link);
                    }
                }
                Err(_) => {
                    // The panic left the parser's cursor/screen partially
                    // advanced. Its state is now inconsistent, so link-row and
                    // scroll reads — for the rest of THIS chunk AND for LATER
                    // chunks that keep using this parser — could be wrong (e.g.
                    // hyperlinks attached to the wrong row). Rebuild it at the
                    // same geometry for a clean baseline. The pane loses its
                    // current screen/scrollback, which is acceptable after a
                    // contained crash and self-heals on the agent's next redraw;
                    // drop this chunk's accumulated link/scroll work too.
                    tracing::warn!(
                        "vt100 parser panicked on an agent-output chunk; resetting the \
                         pane parser to a clean state (screen/scrollback cleared). Known \
                         vt100 0.16.2 edge case with wide characters in a very short pane."
                    );
                    *p = new_pane_parser(rows, cols);
                    new_links.clear();
                    scroll_amount = 0;
                    parser_reset = true;
                    break;
                }
            }
        }
    }

    // A reset cleared the screen, so state recorded against the old screen is
    // stale and must be dropped alongside the rebuilt parser:
    //   - the OSC 8 filter may hold an open link (`current_url`) or a partial
    //     escape from the panicking chunk; left as-is it would wrap the next
    //     chunk's plain text as a stale hyperlink, inserted at rows from the
    //     fresh parser — Ctrl-click would open the wrong link. Rebuild it.
    //   - hyperlink rows recorded against the old screen no longer map to
    //     anything; clear the map.
    // (The hyperlinks lock is taken after releasing the parser lock, preserving
    // the existing parser-then-hyperlinks ordering.)
    if parser_reset {
        *osc8 = Osc8Filter::new();
        if let Ok(mut hmap) = hyperlinks.lock() {
            hmap.clear();
        }
        // PRD #611 (review finding 3): the byte evidence goes with the parser
        // it was evidence about. The cannot-scroll notice arms on "this parser
        // was handed eight screenfuls and retained no lines", which is a claim
        // about the AGENT's output — and a rebuild has just thrown away every
        // line THIS parser retained, for reasons that are the deck's own
        // (`vt100` 0.16.2 panicking on a wide character in a short pane). Left
        // counting, the very next scroll would explain Agent Deck's own history
        // loss as a property of the agent. The counter measures the current
        // parser epoch, so it restarts with the parser.
        bytes_since_spawn.store(0, Ordering::Relaxed);
        return;
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
    /// The production implementation of the on-demand attach: resolve
    /// `pane_id` through `list_agents` and wire the daemon's pane. Delegates to
    /// the inherent [`EmbeddedPaneController::hydrate_pane`], which is a no-op
    /// returning `true` when the pane is already wired.
    fn try_hydrate_pane(&self, pane_id: &str) -> bool {
        self.hydrate_pane(pane_id)
    }

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
            opts.seed,
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
                    // No LOCAL pane — but the card the user pressed Ctrl+W on is
                    // real, so the daemon has an agent for this pane id. That is
                    // the ordinary state for a DAEMON-SPAWNED card (a dispatch, or
                    // a scheduled fire): it surfaces via a synthetic `SessionStart`,
                    // which paints a card without attaching a pane — attaching
                    // happens only when the user focuses it.
                    //
                    // Returning "not found" here made such a card unclosable on the
                    // FIRST Ctrl+W: the F4 policy preserves a card whose close
                    // failed, so the card stayed and its agent kept running.
                    // Selecting the card attaches it, which is why a second Ctrl+W
                    // then worked — the "I have to close it twice" report
                    // (`dispatch/close/001`).
                    return self.stop_unattached_pane(pane_id);
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
        // PRD #241 F3b: the pane slot's identity (`pane_id` IS the
        // `DOT_AGENT_DECK_PANE_ID` the daemon records as `pane_id_env`) plus the
        // I/O task's liveness, so the not-found path below can ask the daemon
        // who owns the slot NOW rather than trusting a possibly-stale id.
        let pane_id_env = pane_id.to_string();
        let io_state = Arc::clone(&s.io_state);
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
        // from the F9 respawn.
        //
        // PRD #241 F3b: if BOTH attempts come back agent-not-found, that used
        // to end the story — `Ok(())`, pane dropped. It no longer does, because
        // "the id I hold is gone" and "this pane has no agent" are different
        // claims during a respawn. `resolve_pane_slot_after_not_found` asks the
        // daemon which agent owns the pane slot now and stops THAT one, so a
        // replacement can never be left running with its card gone.
        let (agent_id, stop_outcome) = s.runtime.block_on(async move {
            // Attempt 1: the id this pane was bound to when the close started.
            let first = tokio::time::timeout(
                CTRL_W_STOP_TIMEOUT,
                client.stop_agent(&initial_agent_id),
            )
            .await;
            if let Some(outcome) = classify_stop(first, &initial_agent_id).resolved() {
                return (initial_agent_id, outcome);
            }

            // Attempt 2 (PRD #92 F12): re-read the shared id — the io_task may
            // already have swapped in the respawned agent — and try that.
            // Unconditional even when the id is unchanged: for a ghost card
            // this second identical answer is what proves the id is dead
            // rather than merely stale.
            let retry_id = shared_agent_id.lock().unwrap().clone();
            tracing::debug!(
                first_agent_id = %initial_agent_id,
                retry_agent_id = %retry_id,
                "close_pane: stop-agent returned 'not found'; retrying once with currently-bound agent id"
            );
            let second =
                tokio::time::timeout(CTRL_W_STOP_TIMEOUT, client.stop_agent(&retry_id)).await;
            if let Some(outcome) = classify_stop(second, &retry_id).resolved() {
                return (retry_id, outcome);
            }

            // PRD #241 F3b: both ids are gone as far as the daemon is
            // concerned. That is NOT yet proof this pane has no agent: the F9
            // `clear = true` respawn removes the old agent and spawns a
            // replacement under the SAME `pane_id_env`, and until the io_task
            // adopts it the shared id we just asked about is the dead one. If
            // we returned success here, the card would vanish while the
            // replacement kept running on the daemon with nothing attached to
            // it. Ask the daemon who owns the slot instead.
            resolve_pane_slot_after_not_found(
                &client,
                &pane_id_env,
                [initial_agent_id, retry_id],
                &io_state,
            )
            .await
        });
        match stop_outcome {
            StopOutcome::Done => {
                // Drop `s` → io_task aborts. No explicit abort needed.
                Ok(())
            }
            StopOutcome::DoneUnverified(warning) => {
                // PRD #241 F3b (review finding G2): the close completes for the
                // reason documented on `resolve_pane_slot_after_not_found` —
                // with `list-agents` unusable there is no evidence a replacement
                // exists, and retaining the pane would re-wedge #218's ghost
                // card. But "completed without being able to check" is exactly
                // the silent degradation the `Failed` arm below refuses to allow:
                // an agent could still be alive on the daemon with its card gone.
                // So finish the teardown (drop `s` → io_task aborts, same as
                // `Done`) and queue the warning for the render loop's per-frame
                // drain, which puts it on the status line.
                tracing::warn!(
                    pane_id = %pane_id,
                    agent_id = %agent_id,
                    "close completed without confirming the daemon side — surfacing a \
                     possibly-unattended-agent warning"
                );
                self.close_warnings.lock().unwrap().push(warning);
                Ok(())
            }
            StopOutcome::Failed(e) => {
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
                    // Restored, not reset: this is the SAME pane and the same
                    // agent behind it, so its output history and any notice it
                    // was showing survive a close that failed.
                    bytes_since_spawn: pane.bytes_since_spawn,
                    scroll_notice_armed_at: pane.scroll_notice_armed_at,
                };
                self.panes
                    .lock()
                    .unwrap()
                    .insert(pane_id.to_string(), restored);
                Err(PaneError::CommandFailed(format!(
                    "stop-agent failed for pane {pane_id}: {e}"
                )))
            }
            StopOutcome::TimedOut => {
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
                    // Restored, not reset: this is the SAME pane and the same
                    // agent behind it, so its output history and any notice it
                    // was showing survive a close that failed.
                    bytes_since_spawn: pane.bytes_since_spawn,
                    scroll_notice_armed_at: pane.scroll_notice_armed_at,
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
    fn write_and_submit_to_pane(
        &self,
        pane_id: &str,
        text: &str,
    ) -> Result<crate::event::SendResult, PaneError> {
        // Issue #608: the daemon now REFUSES a paned write that names no agent
        // (`no-live-target`, nothing written) — a pane id is a recycled handle,
        // so "deliver to whoever holds this pane now" is the accidental
        // mis-delivery the guarded-send machinery exists to prevent. This
        // controller is the one non-mock implementor of this identity-less
        // door, and it already knows which agent occupies the pane, so it names
        // it instead of sending a shape the daemon declines. The SESSION
        // generation is daemon-side state this controller does not track; a
        // caller that knows one calls
        // [`Self::write_and_submit_to_pane_with_identity`] directly, which both
        // production UI delivery paths do.
        //
        // The daemon client is called directly rather than through the trait
        // sibling so this can never recurse through `PaneController`'s default
        // `write_and_submit_to_pane_with_identity` (which forwards back here).
        let expected_agent_id = self.pane_agent_id(pane_id).filter(|id| !id.is_empty());
        let client = self.client.clone();
        let pane_id = pane_id.to_string();
        let text = text.to_string();
        self.runtime
            .block_on(async move {
                client
                    .write_and_submit_with_identity(
                        &pane_id,
                        &text,
                        expected_agent_id.as_deref(),
                        None,
                        None,
                    )
                    .await
            })
            .map_err(|e| PaneError::CommandFailed(format!("write_and_submit: {e}")))
    }

    /// PRD #20 R20-003/R20-004: carry the queued-for agent identity + session and
    /// a stable delivery id to the daemon's atomic write-and-submit RPC, so a
    /// respawn/rebind between enqueue and delivery yields `stale`/`wrong-session`
    /// (no write) and a retry after a lost response replays the first result
    /// instead of double-submitting.
    fn write_and_submit_to_pane_with_identity(
        &self,
        pane_id: &str,
        text: &str,
        expected_agent_id: Option<&str>,
        expected_session_id: Option<&str>,
        delivery_id: Option<&str>,
    ) -> Result<crate::event::SendResult, PaneError> {
        let client = self.client.clone();
        let pane_id = pane_id.to_string();
        let text = text.to_string();
        let expected_agent_id = expected_agent_id.map(str::to_string);
        let expected_session_id = expected_session_id.map(str::to_string);
        let delivery_id = delivery_id.map(str::to_string);
        self.runtime
            .block_on(async move {
                client
                    .write_and_submit_with_identity(
                        &pane_id,
                        &text,
                        expected_agent_id.as_deref(),
                        expected_session_id.as_deref(),
                        delivery_id.as_deref(),
                    )
                    .await
            })
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

    /// The whole point of `PaneLostReason` is that the user reads it, so guard
    /// the strings against regressing back into internal vocabulary. The old
    /// message was `Pane <id> stream I/O task ended` — it named a task the user
    /// has no concept of and said nothing about what to do.
    #[test]
    fn pane_lost_messages_are_user_facing_and_actionable() {
        for reason in [PaneLostReason::AgentKeptCrashing, PaneLostReason::AgentGone] {
            let msg = reason.user_message();
            assert!(
                msg.contains("disconnected"),
                "{reason:?} must name the pane's state: {msg}"
            );
            assert!(
                msg.contains("Close it"),
                "{reason:?} must tell the user what they can do: {msg}"
            );
            for leak in ["I/O task", "io_task", "stream", "reattach", "pane_id"] {
                assert!(
                    !msg.contains(leak),
                    "{reason:?} leaks the internal term {leak:?}: {msg}"
                );
            }
        }
    }

    /// The two give-up causes are unrelated (an agent that will not stay up vs.
    /// one the daemon no longer has), so they must stay distinguishable — that
    /// distinction is what makes a user report diagnosable.
    #[test]
    fn pane_lost_reasons_are_distinguishable() {
        assert_ne!(
            PaneLostReason::AgentKeptCrashing.user_message(),
            PaneLostReason::AgentGone.user_message()
        );
    }

    /// A give-up in which EVERY `list_agents` round-trip failed is the daemon
    /// going missing, not the agent — the remedy differs, so the log must not
    /// call it `no-live-agent`.
    #[test]
    fn reattach_give_up_reports_daemon_unreachable_when_every_lookup_errored() {
        let all_failed = ReattachGiveUp {
            attempts: 4,
            list_errors: 4,
            trailing_list_errors: 4,
            trailing_attach_errors: 0,
            attach_errors: 0,
            last_list_error: Some("connection refused".into()),
            last_attach_error: None,
        };
        assert!(all_failed.daemon_unreachable());
        assert_eq!(all_failed.reason(), "daemon-unreachable");
    }

    /// Second Greptile P1: the aggregate `list_errors == attempts` test meant one
    /// early success — the very first lookup, before a respawning agent had
    /// registered — followed by the daemon dying for the rest of the window was
    /// reported as `no-live-agent`. That is the exact shape of the incident this
    /// diagnostic exists for, so classification keys off the TAIL of the window.
    #[test]
    fn reattach_give_up_reports_daemon_unreachable_when_the_daemon_died_mid_window() {
        let died_after_first_success = ReattachGiveUp {
            attempts: 10,
            // Not all 10 — the first lookup answered, it simply had no record yet.
            list_errors: 9,
            trailing_list_errors: 9,
            trailing_attach_errors: 0,
            attach_errors: 0,
            last_list_error: Some("connection refused".into()),
            last_attach_error: None,
        };
        assert!(
            died_after_first_success.daemon_unreachable(),
            "a daemon that answered once and then died for the rest of the window \
             is still the cause; the agent's absence was never actually observed"
        );
        assert_eq!(
            died_after_first_success.reason(),
            "daemon-unreachable",
            "aggregate counting reported this as no-live-agent — the one case the \
             classifier most needed to get right"
        );
    }

    /// Fourth Greptile P1, the same mistake as the third but on the attach
    /// counter: an early attach failure was kept for the whole window, so it
    /// outranked later AUTHORITATIVE "no agent for this pane" answers and reported
    /// `attach-failing` for a pane whose agent had demonstrably disappeared. A
    /// successful lookup that finds no agent now clears the trailing attach fault.
    #[test]
    fn reattach_give_up_lets_an_authoritative_no_agent_answer_supersede_an_attach_fault() {
        let agent_vanished_after_an_attach_failure = ReattachGiveUp {
            attempts: 10,
            list_errors: 0,
            trailing_list_errors: 0,
            // It happened, and stays in the log…
            attach_errors: 1,
            // …but nine later lookups answered "no such agent", which settles it.
            trailing_attach_errors: 0,
            last_list_error: None,
            last_attach_error: Some("attach refused".into()),
        };
        assert!(!agent_vanished_after_an_attach_failure.attach_failing());
        assert_eq!(
            agent_vanished_after_an_attach_failure.reason(),
            "no-live-agent",
            "an attach failure against an agent that has since gone must not \
             outrank the daemon's authoritative answer that it is gone"
        );
        // Fifth Greptile P1: the reason was right but the logged error string was
        // not cleared with it, so this give-up read `no-live-agent` while still
        // carrying "attach refused". Note the literal above deliberately LEAVES
        // that message set — the guarantee is that reporting refuses to pair it
        // with this reason, not merely that the loop happens to clear it.
        assert_eq!(
            agent_vanished_after_an_attach_failure.reported_error(),
            "none",
            "a no-live-agent give-up must report no error: the daemon answered \
             and had no agent, which is an absence of failure, not a failure"
        );
    }

    /// The pairing invariant itself: `reason()` and `reported_error()` are chosen
    /// by the same branch order, so a give-up carrying BOTH a daemon error and an
    /// attach error reports the daemon's — the one that matches its reason.
    #[test]
    fn reattach_give_up_reports_the_error_belonging_to_its_reason() {
        let both_faults_recorded = ReattachGiveUp {
            attempts: 4,
            list_errors: 3,
            trailing_list_errors: 3,
            attach_errors: 1,
            trailing_attach_errors: 1,
            last_list_error: Some("connection refused".into()),
            last_attach_error: Some("attach refused".into()),
        };
        assert_eq!(both_faults_recorded.reason(), "daemon-unreachable");
        assert_eq!(
            both_faults_recorded.reported_error(),
            "connection refused",
            "the reported error must come from the same branch as the reason, \
             never from a different fault that also happened to be recorded"
        );
    }

    /// The converse: failures EARLY that recovered must not be blamed on the
    /// daemon. The window ended with the daemon answering and no record for the
    /// pane, so the agent really is gone.
    #[test]
    fn reattach_give_up_ignores_transient_failures_that_recovered() {
        let recovered = ReattachGiveUp {
            attempts: 8,
            list_errors: 5,
            // Every trailing lookup succeeded — the blip is history.
            trailing_list_errors: 0,
            trailing_attach_errors: 0,
            attach_errors: 0,
            last_list_error: Some("earlier blip".into()),
            last_attach_error: None,
        };
        assert!(!recovered.daemon_unreachable());
        assert_eq!(recovered.reason(), "no-live-agent");
    }

    /// Greptile P1 on the first draft: a pane whose agent WAS listed but could
    /// never be attached to was reported as `no-live-agent`, blaming an agent
    /// that was demonstrably still registered.
    #[test]
    fn reattach_give_up_reports_attach_failing_when_the_agent_was_found_but_unattachable() {
        let attach_broken = ReattachGiveUp {
            attempts: 5,
            list_errors: 0,
            trailing_list_errors: 0,
            attach_errors: 5,
            // Every attach failed and none was ever superseded by a no-agent
            // answer, so the attach fault is still the live story.
            trailing_attach_errors: 5,
            last_list_error: None,
            last_attach_error: Some("attach refused".into()),
        };
        assert!(!attach_broken.daemon_unreachable());
        assert!(attach_broken.attach_failing());
        assert_eq!(attach_broken.reason(), "attach-failing");
    }

    /// Precedence: a run that lost the daemon partway (some list errors AND an
    /// earlier attach failure) is the daemon's fault, not the attach path's —
    /// `daemon_unreachable` is only true when EVERY lookup failed, so a mixed
    /// run falls through to `attach-failing` rather than silently outranking it.
    #[test]
    fn reattach_give_up_precedence_between_daemon_and_attach_faults() {
        // An attach failed early, then the daemon went away and never came back.
        // The daemon outranks the attach fault: it is the live problem, and the
        // attach can't even be retried while the daemon is unreachable.
        let daemon_died_after_an_attach_failure = ReattachGiveUp {
            attempts: 4,
            list_errors: 3,
            trailing_list_errors: 3,
            attach_errors: 1,
            // Still 1: only a SUCCESSFUL lookup with no match clears this, and
            // every lookup after the attach failure errored out.
            trailing_attach_errors: 1,
            last_list_error: Some("connection refused".into()),
            last_attach_error: Some("attach refused".into()),
        };
        assert_eq!(
            daemon_died_after_an_attach_failure.reason(),
            "daemon-unreachable",
            "the window ended with the daemon silent, so that is the cause to \
             report even though an attach also failed earlier"
        );

        // The reverse tail: the daemon is answering at give-up time and the only
        // standing fault is the attach path.
        let attach_fault_with_a_recovered_blip = ReattachGiveUp {
            attempts: 4,
            list_errors: 2,
            trailing_list_errors: 0,
            attach_errors: 1,
            // The agent is still being found, so the attach failure was never
            // superseded — it is the standing fault.
            trailing_attach_errors: 1,
            last_list_error: None,
            last_attach_error: Some("attach refused".into()),
        };
        assert_eq!(
            attach_fault_with_a_recovered_blip.reason(),
            "attach-failing"
        );
    }

    /// The complements: a lookup that answered at least once genuinely has no
    /// agent for the pane, and a zero-attempt give-up must never be reported as
    /// a daemon fault (nothing was ever asked).
    #[test]
    fn reattach_give_up_reports_no_live_agent_when_any_lookup_succeeded() {
        let answered = ReattachGiveUp {
            attempts: 4,
            trailing_list_errors: 0,
            list_errors: 3,
            trailing_attach_errors: 0,
            attach_errors: 0,
            last_list_error: Some("transient".into()),
            last_attach_error: None,
        };
        assert!(!answered.daemon_unreachable());
        assert_eq!(answered.reason(), "no-live-agent");

        let clean = ReattachGiveUp {
            attempts: 2,
            trailing_list_errors: 0,
            list_errors: 0,
            trailing_attach_errors: 0,
            attach_errors: 0,
            last_list_error: None,
            last_attach_error: None,
        };
        assert_eq!(clean.reason(), "no-live-agent");

        let never_asked = ReattachGiveUp {
            attempts: 0,
            trailing_list_errors: 0,
            list_errors: 0,
            trailing_attach_errors: 0,
            attach_errors: 0,
            last_list_error: None,
            last_attach_error: None,
        };
        assert!(
            !never_asked.daemon_unreachable(),
            "0 attempts / 0 errors must not read as 'every lookup failed'"
        );
    }

    /// Regression: `vt100` 0.16.2 panics (`col_wrap` row-underflow / an
    /// out-of-bounds cell `unwrap()` in grid.rs) when a wide character wraps in
    /// a 1-row pane — the geometry a pane collapses to when many are stacked
    /// into one column (the crash observed over `connect`). Feeding that chunk
    /// must be contained by `process_agent_output_chunk` so a single malformed
    /// chunk from any agent can never crash the whole TUI: the call returns
    /// normally, the guard flag clears, the parser mutex is not poisoned, and a
    /// later chunk still processes.
    #[test]
    fn wide_char_in_one_row_pane_does_not_crash_the_tui() {
        // Suppress the *contained* panic's default report so test output stays
        // clean; still surface any uncontained panic by delegating to the prior
        // hook. Not restoring is harmless — it only alters behavior while
        // `in_guarded_parser_feed()` is set, which only our guarded feed sets.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !in_guarded_parser_feed() {
                prev(info);
            }
        }));

        // Mirrors real agent output (CJK) landing in a 1-row pane. Repeated so
        // the chunk on its own clears the notice's byte threshold for this
        // geometry — the point of the counter assertions below.
        let crasher_unit = "hello 世界 world 世界 more 世界";
        let crasher_owned = crasher_unit.repeat(8);
        let crasher = crasher_owned.as_bytes();

        // Guard against bit-rot: prove the raw crate still panics on this input,
        // so a future vt100 fix/upgrade doesn't turn this into a silent no-op.
        let unguarded = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let mut p = vt100::Parser::new(1, 10, 1000);
            p.process(crasher);
        }));
        assert!(
            unguarded.is_err(),
            "precondition: raw vt100 must still panic on this input; if the crate \
             was fixed/upgraded, retarget or retire this regression test"
        );

        let parser = Mutex::new(vt100::Parser::new(1, 10, 1000));
        let mut osc8 = Osc8Filter::new();
        let mouse = AtomicBool::new(false);
        let mut mouse_scan = MouseModeScanner::default();
        let links = Mutex::new(HyperlinkMap::new());
        let bytes_since_spawn = AtomicU64::new(0);

        // The real path must NOT panic on the crashing chunk.
        process_agent_output_chunk(
            crasher,
            &mut osc8,
            &parser,
            &mouse,
            &mut mouse_scan,
            &links,
            &bytes_since_spawn,
        );

        assert!(
            !in_guarded_parser_feed(),
            "guard flag must be cleared once the feed returns"
        );
        assert!(
            parser.lock().is_ok(),
            "parser mutex must not be poisoned by a contained panic"
        );

        // A subsequent chunk still flows through the same parser. Deliberately
        // short enough not to wrap in this 1x10 pane: a longer one panics on the
        // same vt100 edge case and would rebuild the parser again, so "a later
        // chunk still processes" would be asserting nothing.
        process_agent_output_chunk(
            b"ok\r\n",
            &mut osc8,
            &parser,
            &mouse,
            &mut mouse_scan,
            &links,
            &bytes_since_spawn,
        );
        assert!(parser.lock().is_ok());
        assert_eq!(
            bytes_since_spawn.load(Ordering::Relaxed),
            b"ok\r\n".len() as u64,
            "PRD #611 (review finding 3): the rebuild resets the byte evidence, \
             so the counter measures only what the CURRENT parser was handed"
        );

        // The property that matters, stated directly: a parser rebuild cannot
        // arm the cannot-scroll notice. Its trigger is "at least
        // SCROLL_NOTICE_MIN_SCREENFULS screenfuls fed, and zero retained lines"
        // — and a rebuild leaves a pane with zero retained lines for reasons
        // that are Agent Deck's own, not the agent's. Without the reset the
        // discarded parser's bytes would still be sitting in the counter and the
        // next scroll would explain the deck's own history loss as a property of
        // the agent.
        // The pane is 1 row x 10 cols, so one screenful is 10 bytes.
        let threshold = crate::ui::SCROLL_NOTICE_MIN_SCREENFULS * 10;
        assert!(
            crasher.len() as u64 >= threshold,
            "precondition: the panicking chunk alone must exceed the notice \
             threshold ({} bytes vs {threshold}), or this proves nothing",
            crasher.len()
        );
        assert!(
            bytes_since_spawn.load(Ordering::Relaxed) < threshold,
            "a parser the deck rebuilt must not immediately look like an agent \
             that produced screenfuls of output and retained nothing"
        );
    }

    /// PRD #611 (audit finding): the pane's mouse flag outlives an attach
    /// session, so the reattach loop has to decide whether the child it is now
    /// talking to is the same one that requested mouse reporting.
    ///
    /// The rule is deliberately narrow. A blanket clear-on-reattach would stop
    /// forwarding the wheel to a live claude pane after an ordinary reconnect
    /// whose snapshot did not re-emit DECSET — trading a rare injection for a
    /// common regression.
    #[test]
    fn a_reattach_only_replaces_the_child_when_the_agent_id_changes() {
        assert!(
            reattach_replaces_the_child("agent-1", "agent-2"),
            "a different daemon-side agent is a different child process, and it \
             has requested nothing yet"
        );
        assert!(
            !reattach_replaces_the_child("agent-1", "agent-1"),
            "the same agent is the same PTY: its DECSET is long past and its \
             terminal state must survive the reconnect"
        );
        assert!(
            !reattach_replaces_the_child("", "agent-1"),
            "no previous id is no evidence — preserve, the conservative direction"
        );
    }

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

    // ---- Issue #363: every parser is built through one guarded constructor --

    /// Regression for issue #363, on the **shipped spawn path**.
    ///
    /// `create_stream_pane` forwards viewport-derived dims into
    /// `wire_stream_pane`, and every `ui.rs` layout helper that produces them
    /// ends in a `saturating_sub(2)` border allowance — so a terminal too short
    /// or too narrow reaches the wiring with a zero axis. Before the fix that
    /// went straight into `vt100::Parser::new`, which breaks at construction:
    /// `Grid::new` computes `scroll_bottom` as `rows - 1`. Written against the
    /// unfixed code this test does not merely assert a wrong size — it panics in
    /// `grid.rs:26`, which is what a debug build of the shipped binary would do
    /// too.
    ///
    /// L1 by construction, and deliberately so: what is under test is parser
    /// *construction* from caller-supplied dims. Nothing here needs the spawned
    /// binary, a real PTY, or a daemon — the connection is an in-process socket
    /// pair — so an L2 `e2e_*` test would cost a process spawn to observe the
    /// same `screen().size()`.
    #[cfg(unix)]
    #[test]
    fn wire_stream_pane_never_builds_a_degenerate_parser() {
        let controller = EmbeddedPaneController::for_render_only_tests();
        // `UnixStream::pair` registers with the reactor, so it needs a runtime
        // in scope. The same one the controller will spawn its (never-polled)
        // I/O and resize tasks onto.
        let rt = render_only_runtime();
        let _enter = rt.enter();

        // Every way a border allowance can collapse an axis, plus the
        // hostile/oversized direction `parser_init_dims` also refuses.
        for (i, (rows, cols)) in [
            (0u16, 0u16),
            (0, 80),
            (24, 0),
            (u16::MAX, u16::MAX),
            (PTY_RESIZE_DIM_MAX + 1, 40),
        ]
        .into_iter()
        .enumerate()
        {
            let pane_id = format!("{i}");
            // Held for the duration: dropping it would EOF the reader half.
            let (conn, _peer) = AttachConnection::connected_pair_for_test();
            controller.wire_stream_pane(
                pane_id.clone(),
                format!("agent-{i}"),
                conn,
                format!("pane-{i}"),
                None,
                None,
                rows,
                cols,
            );

            let screen = controller
                .get_screen(&pane_id)
                .expect("wire_stream_pane must have registered the pane");
            let size = screen.lock().unwrap().screen().size();
            assert_eq!(
                size,
                (24, 80),
                "wire_stream_pane({rows}, {cols}) must fall back to the guarded 24x80 \
                 parser, not build a {size:?} grid"
            );
        }
    }

    /// Valid dims must still reach the parser untouched — the guard is a floor
    /// and a ceiling, not a rewrite. A pane sized to its real viewport has to
    /// parse the daemon's already-correctly-sized frames without clipping.
    #[cfg(unix)]
    #[test]
    fn wire_stream_pane_preserves_usable_dimensions() {
        let controller = EmbeddedPaneController::for_render_only_tests();
        let rt = render_only_runtime();
        let _enter = rt.enter();

        let (conn, _peer) = AttachConnection::connected_pair_for_test();
        controller.wire_stream_pane(
            "1".to_string(),
            "agent-1".to_string(),
            conn,
            "pane-1".to_string(),
            None,
            None,
            21,
            38,
        );

        let screen = controller.get_screen("1").expect("pane registered");
        let size = screen.lock().unwrap().screen().size();
        assert_eq!(size, (21, 38), "usable dims must pass through unchanged");
    }

    /// The shared constructor is the whole point of the fix: one definition of
    /// "valid parser geometry", applied in one place. Pin both halves.
    #[test]
    fn new_pane_parser_clamps_and_passes_through() {
        assert_eq!(new_pane_parser(0, 0).screen().size(), (24, 80));
        assert_eq!(new_pane_parser(0, 80).screen().size(), (24, 80));
        assert_eq!(new_pane_parser(24, 0).screen().size(), (24, 80));
        assert_eq!(
            new_pane_parser(u16::MAX, u16::MAX).screen().size(),
            (24, 80)
        );
        assert_eq!(new_pane_parser(21, 38).screen().size(), (21, 38));
        assert_eq!(new_pane_parser(1, 1).screen().size(), (1, 1));
    }

    /// The failure mode issue #363 warns about is a *fourth* call site added
    /// later that quietly skips the guard — the same drift that produced this
    /// bug, and the same class as any rule kept in two hand-maintained copies.
    ///
    /// So pin the structural invariant rather than only the behaviour: in this
    /// file's non-test code there is exactly ONE `vt100::Parser::new`, and it is
    /// inside `new_pane_parser`. `include_str!` is compile-time, so this reads
    /// no file at runtime and stays inside the fast tier's budget.
    #[test]
    fn parser_is_constructed_in_exactly_one_place() {
        const SRC: &str = include_str!("embedded_pane.rs");
        // Split off this very test module; parsers built inside it are fixtures
        // (`wide_char_in_one_row_pane_does_not_crash_the_tui` deliberately
        // constructs a raw 1x10 parser to prove the vt100 bug still exists).
        // `include_str!` yields the file exactly as checked out, and Windows
        // checks it out with CRLF endings — so every `\n`-anchored marker below
        // (this split, and the `\n}\n` that ends the helper) silently fails to
        // match there. Normalize once so the guard reads the same source on
        // every platform.
        let src = SRC.replace("\r\n", "\n");
        let (prod, _tests) = src
            .split_once("\n#[cfg(test)]\nmod tests {")
            .expect("test module marker moved; update this guard");

        let needle = "vt100::Parser::new(";
        let hits: Vec<usize> = prod.match_indices(needle).map(|(i, _)| i).collect();
        assert_eq!(
            hits.len(),
            1,
            "non-test code in embedded_pane.rs must construct vt100 parsers only \
             through `new_pane_parser` (issue #363); found {} raw call sites at \
             byte offsets {hits:?}",
            hits.len()
        );

        let helper_start = prod
            .find("fn new_pane_parser(")
            .expect("`new_pane_parser` must exist");
        let helper_end = prod[helper_start..]
            .find("\n}\n")
            .map(|rel| helper_start + rel)
            .expect("`new_pane_parser` must end at a top-level brace");
        assert!(
            (helper_start..helper_end).contains(&hits[0]),
            "the one raw `vt100::Parser::new` must live inside `new_pane_parser`"
        );
    }

    // ---- PRD #611 M3: `scan_mouse_mode` -----------------------------------
    //
    // This function derives `mouse_mode_enabled`, which is the exact input to
    // the branch `scroll_focused_agent_pane` takes — and, since M2, to the
    // on-screen notice that explains a scroll that cannot land. Mis-deriving it
    // makes the deck take the scrollback branch for an agent that would have
    // scrolled perfectly well, and then explain that outcome with a sentence
    // that is not true of that agent. It had no tests at all before these.

    /// Feed one chunk through the scanner and read the flag back. The scanner
    /// is the caller's, so a sequence cut in half by a chunk boundary is seen
    /// the way it is on the real stream.
    fn feed_mouse(chunk: &[u8], flag: &AtomicBool, state: &mut MouseModeScanner) -> bool {
        scan_mouse_mode(chunk, flag, state);
        flag.load(Ordering::Relaxed)
    }

    /// Fresh scanner over a flag seeded the way the real spawn path seeds it.
    fn mouse_scanner() -> (AtomicBool, MouseModeScanner) {
        (AtomicBool::new(false), MouseModeScanner::default())
    }

    /// The protocol modes and the encoding modes, named the way `vt100` 0.16.2
    /// names them, so a test reads as the model rather than as four numbers.
    const PROTOCOL_MODES: [(u32, MouseProtocol); 4] = [
        (9, MouseProtocol::Press),
        (1000, MouseProtocol::PressRelease),
        (1002, MouseProtocol::ButtonMotion),
        (1003, MouseProtocol::AnyMotion),
    ];

    /// `ESC[?<mode><final>` as bytes.
    fn private_mode(mode: u32, set: bool) -> Vec<u8> {
        format!("\x1b[?{mode}{}", if set { 'h' } else { 'l' }).into_bytes()
    }

    /// The flag's rule, stated once: a reporting protocol must be in force AND
    /// SGR must be the selected encoding.
    ///
    /// The SGR half is not a detail. `forward_mouse_scroll` emits SGR bytes and
    /// only SGR bytes, so forwarding to a child that selected the default or the
    /// UTF-8 encoding pushes input it will mis-parse into somebody else's
    /// process; declining merely leaves the deck scrolling its own buffer.
    #[test]
    fn scan_mouse_mode_enables_only_with_both_a_protocol_and_sgr() {
        for (mode, _) in PROTOCOL_MODES {
            let (flag, mut state) = mouse_scanner();
            assert!(
                !feed_mouse(&private_mode(mode, true), &flag, &mut state),
                "{mode} selects a protocol but no encoding — the SGR reports the \
                 deck forwards would not be understood, so it must not forward"
            );
            assert!(
                feed_mouse(b"\x1b[?1006h", &flag, &mut state),
                "{mode} plus SGR is a child that can receive what the deck sends"
            );
        }

        let (flag, mut state) = mouse_scanner();
        assert!(
            !feed_mouse(b"\x1b[?1006h", &flag, &mut state),
            "1006 selects an ENCODING and enables no reporting on its own; a \
             scanner that latches it reports mouse mode for a child that asked \
             for none"
        );
        assert!(!feed_mouse(b"\x1b[?1005h", &flag, &mut state));
        assert!(
            !feed_mouse(b"\x1b[?1000h", &flag, &mut state),
            "1005 is UTF-8 encoding, so a protocol under it is still not SGR"
        );
        assert!(
            feed_mouse(b"\x1b[?1006h", &flag, &mut state),
            "switching the encoding to SGR is what makes the standing protocol \
             forwardable"
        );
    }

    /// A reset withdraws whichever half it names, and either half is enough to
    /// stop the deck forwarding.
    #[test]
    fn scan_mouse_mode_detects_each_single_mode_disable_form() {
        for (mode, _) in PROTOCOL_MODES {
            let (flag, mut state) = mouse_scanner();
            let mut enable = private_mode(mode, true);
            enable.extend_from_slice(b"\x1b[?1006h");
            assert!(feed_mouse(&enable, &flag, &mut state));
            assert!(
                !feed_mouse(&private_mode(mode, false), &flag, &mut state),
                "{mode} is the protocol in force, so withdrawing it stops reporting"
            );
        }

        let (flag, mut state) = mouse_scanner();
        assert!(feed_mouse(b"\x1b[?1002h\x1b[?1006h", &flag, &mut state));
        assert!(
            !feed_mouse(b"\x1b[?1006l", &flag, &mut state),
            "withdrawing SGR leaves reporting on in an encoding the deck does \
             not speak, which is the same thing as not forwarding"
        );
    }

    /// The defect this model replaced, and the one case where the intermediate
    /// four-latch version was strictly WORSE than the substring scan before it.
    ///
    /// 1000, 1002, 1003 (and 9) are one mutually exclusive field —
    /// `vt100::Screen::set_mouse_mode` assigns it, it does not or-in a bit — so a
    /// later SET supersedes an earlier one and there is only ever one protocol
    /// to withdraw. With four independent latches, `1000h 1002h 1002l` leaves the
    /// 1000 latch set and holds the flag true forever, where a real terminal ends
    /// with reporting off.
    #[test]
    fn scan_mouse_mode_treats_the_protocol_modes_as_one_mutually_exclusive_field() {
        let (flag, mut state) = mouse_scanner();
        assert!(feed_mouse(b"\x1b[?1006h\x1b[?1000h", &flag, &mut state));
        assert!(feed_mouse(b"\x1b[?1002h", &flag, &mut state));
        assert!(
            !feed_mouse(b"\x1b[?1002l", &flag, &mut state),
            "1002 superseded 1000, so withdrawing 1002 withdraws the only \
             protocol in force — reporting is off"
        );

        let (flag, mut state) = mouse_scanner();
        assert!(feed_mouse(b"\x1b[?1006h\x1b[?1000h", &flag, &mut state));
        assert!(feed_mouse(b"\x1b[?1002h", &flag, &mut state));
        assert!(
            feed_mouse(b"\x1b[?1000l", &flag, &mut state),
            "a reset that does NOT name the protocol in force withdraws nothing \
             (`vt100::Screen::clear_mouse_mode`)"
        );
        assert_eq!(state.protocol, MouseProtocol::ButtonMotion);

        let (flag, mut state) = mouse_scanner();
        assert!(!feed_mouse(b"\x1b[?1000h", &flag, &mut state));
        assert!(!feed_mouse(b"\x1b[?1003h", &flag, &mut state));
        assert_eq!(
            state.protocol,
            MouseProtocol::AnyMotion,
            "the later SET supersedes: exactly one protocol is ever in force"
        );
    }

    /// The two agents PRD #611 measured, end to end, as the sanity check on the
    /// whole model: claude sets all four modes and must be forwarded to; codex
    /// sets only focus reporting and must not be.
    #[test]
    fn scan_mouse_mode_forwards_to_the_app_managed_agent_and_not_the_other() {
        let (flag, mut state) = mouse_scanner();
        assert!(
            feed_mouse(
                b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h",
                &flag,
                &mut state
            ),
            "claude requests a protocol and SGR — the wheel reaches it"
        );

        let (flag, mut state) = mouse_scanner();
        assert!(
            !feed_mouse(b"\x1b[?1004h", &flag, &mut state),
            "codex requests focus reporting and nothing else — the wheel must \
             not be forwarded to it"
        );
    }

    /// Defect 1. Apps routinely set several private modes in one sequence, and
    /// none of the four single-mode patterns the old scanner looked for is a
    /// substring of the combined form — so mouse mode went entirely undetected
    /// for an agent that emits it.
    #[test]
    fn scan_mouse_mode_detects_the_combined_set_form() {
        let (flag, mut state) = mouse_scanner();
        assert!(
            feed_mouse(b"\x1b[?1000;1002;1006h", &flag, &mut state),
            "a combined SET must enable — the modes are in the parameter list"
        );
        assert_eq!(
            state.protocol,
            MouseProtocol::ButtonMotion,
            "parameters apply left to right, so the last protocol named wins"
        );
    }

    #[test]
    fn scan_mouse_mode_detects_the_combined_reset_form() {
        let (flag, mut state) = mouse_scanner();
        assert!(feed_mouse(b"\x1b[?1000;1002;1006h", &flag, &mut state));
        assert!(
            !feed_mouse(b"\x1b[?1000;1002;1006l", &flag, &mut state),
            "a combined RESET naming both the protocol in force and the \
             encoding leaves nothing reporting"
        );
        assert_eq!(state.protocol, MouseProtocol::None);
        assert_eq!(state.encoding, MouseEncoding::Default);
    }

    /// A parameter list may mix tracked and untracked modes. What matters is
    /// that the ones it does name are applied.
    #[test]
    fn scan_mouse_mode_enables_on_a_combined_form_mixing_tracked_and_untracked_modes() {
        let (flag, mut state) = mouse_scanner();
        assert!(
            feed_mouse(b"\x1b[?1004;1002;1006h", &flag, &mut state),
            "1002 is button-motion tracking and 1006 is SGR; 1004 alongside \
             them changes nothing"
        );
    }

    /// The codex case, and the one a careless "does the parameter list contain
    /// 100…?" check breaks: 1004 is focus reporting, not mouse reporting. An
    /// agent that sets only 1004 receives no mouse events, so forwarding the
    /// wheel to it is exactly the dead end PRD #611 exists to explain.
    #[test]
    fn scan_mouse_mode_ignores_focus_reporting_on_its_own() {
        let (flag, mut state) = mouse_scanner();
        assert!(
            !feed_mouse(b"\x1b[?1004h", &flag, &mut state),
            "1004 is focus reporting — it must not read as mouse reporting"
        );
        assert_eq!(state.protocol, MouseProtocol::None);
        assert_eq!(state.encoding, MouseEncoding::Default);
    }

    /// Neighbouring mode numbers must not be matched by digit adjacency, and
    /// the private modes an app sets for other reasons must not count either.
    #[test]
    fn scan_mouse_mode_matches_whole_parameters_not_digit_substrings() {
        let (flag, mut state) = mouse_scanner();
        assert!(!feed_mouse(b"\x1b[?11000h", &flag, &mut state));
        assert!(!feed_mouse(b"\x1b[?10006h", &flag, &mut state));
        assert!(!feed_mouse(b"\x1b[?1049h\x1b[?2004h", &flag, &mut state));
        assert_eq!(state.protocol, MouseProtocol::None);
        assert_eq!(state.encoding, MouseEncoding::Default);
    }

    /// Defect 2, one direction. The old scanner checked every enable pattern
    /// before it considered any disable, so an enable anywhere in a chunk beat
    /// a disable that came after it — a child switching reporting off stayed
    /// flagged on until some later chunk happened to contain no enable.
    #[test]
    fn scan_mouse_mode_lets_a_later_disable_beat_an_earlier_enable_in_one_chunk() {
        let (flag, mut state) = mouse_scanner();
        assert!(
            !feed_mouse(b"\x1b[?1006h\x1b[?1000h\x1b[?1000l", &flag, &mut state),
            "the disable is later in the byte stream, so it is what holds"
        );
    }

    /// Defect 2, the other direction: byte position decides, so the same two
    /// directives in the opposite order settle the other way.
    #[test]
    fn scan_mouse_mode_lets_a_later_enable_beat_an_earlier_disable_in_one_chunk() {
        let (flag, mut state) = mouse_scanner();
        assert!(
            feed_mouse(b"\x1b[?1006h\x1b[?1000l\x1b[?1000h", &flag, &mut state),
            "the enable is later in the byte stream, so it is what holds"
        );
    }

    /// The model is not this function's opinion: it is `vt100` 0.16.2's, the
    /// same crate that parses these panes' bytes into the screen the deck
    /// renders. Feed both the identical stream and require the identical answer.
    ///
    /// This is the test the four-latch defect would have failed on the day it
    /// was written — the third stream below is the exact regression — and it
    /// keeps the two derivations from drifting if the crate is ever upgraded.
    #[test]
    fn scan_mouse_mode_agrees_with_the_vt100_parser_it_shares_a_stream_with() {
        /// What the authority says about a stream: a protocol in force, encoded
        /// the one way `forward_mouse_scroll` can speak.
        fn vt100_would_understand_an_sgr_report(stream: &[u8]) -> bool {
            let mut parser = vt100::Parser::new(24, 80, 100);
            parser.process(stream);
            parser.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None
                && parser.screen().mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr
        }

        for stream in [
            b"".as_slice(),
            b"\x1b[?1000h".as_slice(),
            b"\x1b[?1006h".as_slice(),
            b"\x1b[?1005h\x1b[?1000h".as_slice(),
            b"\x1b[?1000h\x1b[?1002h\x1b[?1002l".as_slice(),
            b"\x1b[?1000h\x1b[?1002h\x1b[?1000l".as_slice(),
            b"\x1b[?1000;1002;1006h".as_slice(),
            b"\x1b[?1000;1002;1006h\x1b[?1000;1002;1006l".as_slice(),
            b"\x1b[?9h\x1b[?1006h".as_slice(),
            b"\x1b[?1006h\x1b[?1003h\x1b[?1006l".as_slice(),
            b"\x1b[?1006h\x1b[?1003h\x1b[?1005h".as_slice(),
            b"\x1b[?1004h".as_slice(),
            b"\x1b[?1049h\x1b[?2004h\x1b[?1000;1006h".as_slice(),
            b"\x1b[?11000h\x1b[?10006h".as_slice(),
            b"hello \x1b[?1000;1006h world\r\n".as_slice(),
        ] {
            let (flag, mut state) = mouse_scanner();
            let ours = feed_mouse(stream, &flag, &mut state);
            assert_eq!(
                ours,
                vt100_would_understand_an_sgr_report(stream),
                "the scanner and the parser must derive the same answer from \
                 {:?}",
                String::from_utf8_lossy(stream)
            );
        }
    }

    /// Defect 3. A PTY read boundary falls wherever the kernel had bytes ready,
    /// so any sequence can arrive in two pieces. Split the same sequence at
    /// every interior offset: every one of them must still be seen.
    #[test]
    fn scan_mouse_mode_sees_a_sequence_split_at_any_offset() {
        // A combined form, so the whole decision — protocol AND encoding — has
        // to survive the boundary rather than half of it.
        let seq = b"\x1b[?1002;1006h";
        for split in 1..seq.len() {
            let (flag, mut state) = mouse_scanner();
            assert!(
                !feed_mouse(&seq[..split], &flag, &mut state),
                "the first half alone decides nothing (split at {split})"
            );
            assert!(
                feed_mouse(&seq[split..], &flag, &mut state),
                "the sequence completes across the boundary (split at {split})"
            );
        }
    }

    /// The two splits PRD #611 M3 calls out by name: mid-parameter, and
    /// mid-parameter-list.
    #[test]
    fn scan_mouse_mode_sees_a_split_mid_parameter_and_mid_parameter_list() {
        let (flag, mut state) = mouse_scanner();
        assert!(!feed_mouse(b"\x1b[?1006h", &flag, &mut state));
        assert!(!feed_mouse(b"\x1b[?100", &flag, &mut state));
        assert!(
            feed_mouse(b"2h", &flag, &mut state),
            "mid-parameter split: `ESC[?100` + `2h` is `ESC[?1002h`, which \
             completes the SGR-encoded protocol the earlier chunk half-made"
        );

        let (flag, mut state) = mouse_scanner();
        assert!(!feed_mouse(b"\x1b[?1006;10", &flag, &mut state));
        assert!(
            feed_mouse(b"02h", &flag, &mut state),
            "mid-parameter-list split: `ESC[?1006;10` + `02h`"
        );
    }

    /// A sequence may be cut into more than two pieces, including ones that
    /// contribute a single byte.
    #[test]
    fn scan_mouse_mode_sees_a_sequence_split_across_four_chunks() {
        let (flag, mut state) = mouse_scanner();
        for piece in [
            b"\x1b".as_slice(),
            b"[?10".as_slice(),
            b"02;1006".as_slice(),
        ] {
            assert!(!feed_mouse(piece, &flag, &mut state));
        }
        assert!(feed_mouse(b"h", &flag, &mut state));
    }

    /// A partial that never completes must leave the flag exactly where it was
    /// and must not grow the carry: the carry is only ever the shortest suffix
    /// that could still open a valid sequence.
    #[test]
    fn scan_mouse_mode_holds_an_incomplete_partial_without_deciding_anything() {
        let (flag, mut state) = mouse_scanner();
        assert!(feed_mouse(b"\x1b[?1000;1006h", &flag, &mut state));
        assert!(
            feed_mouse(b"\x1b[?1000", &flag, &mut state),
            "an unterminated sequence decides nothing, so the flag is untouched"
        );
        assert_eq!(
            state.carry.as_slice(),
            b"\x1b[?1000".as_slice(),
            "only the open sequence is carried"
        );

        // Ordinary output after it resolves nothing and clears the carry: the
        // partial turned out not to be a private-mode sequence at all.
        assert!(feed_mouse(b"; hello world\r\n", &flag, &mut state));
        assert!(state.carry.is_empty(), "nothing left that could still open");
    }

    /// The cap. A malformed or hostile stream can open a private-mode sequence
    /// and never terminate it; the carry must stay bounded whatever it does.
    #[test]
    fn scan_mouse_mode_caps_the_carry_against_an_unterminated_parameter_run() {
        let (flag, mut state) = mouse_scanner();
        let mut fed = 0usize;
        assert!(!feed_mouse(b"\x1b[?", &flag, &mut state));
        for _ in 0..512 {
            assert!(!feed_mouse(b"1;", &flag, &mut state));
            fed += 2;
            assert!(
                state.carry.len() <= MOUSE_SCAN_CARRY_MAX,
                "carry grew to {} after {fed} bytes of open parameter list",
                state.carry.len()
            );
        }
    }

    /// The other unterminated shape: introducers with nothing after them. Each
    /// aborts the one before it, so the carry never holds more than the last.
    #[test]
    fn scan_mouse_mode_caps_the_carry_against_a_run_of_bare_introducers() {
        let (flag, mut state) = mouse_scanner();
        for _ in 0..512 {
            assert!(!feed_mouse(b"\x1b[?", &flag, &mut state));
            assert!(
                state.carry.len() <= MOUSE_SCAN_CARRY_MAX,
                "carry grew to {}",
                state.carry.len()
            );
        }
    }

    /// Once the cap is blown the scanner refuses the sequence rather than
    /// completing it from a truncated prefix: the one thing it must never do is
    /// invent an enable out of bytes it did not see whole.
    #[test]
    fn scan_mouse_mode_does_not_enable_from_a_carry_it_had_to_drop() {
        let (flag, mut state) = mouse_scanner();
        let mut overlong = b"\x1b[?".to_vec();
        overlong.extend_from_slice(&b"1;".repeat(MOUSE_SCAN_CARRY_MAX));
        assert!(!feed_mouse(&overlong, &flag, &mut state));
        assert!(state.carry.is_empty(), "an over-cap partial is dropped");
        assert!(
            !feed_mouse(b"1002;1006h", &flag, &mut state),
            "the tail of a dropped sequence must not complete one on its own — \
             and this tail names both halves, so a carry that had survived \
             would have enabled the flag here"
        );
    }

    /// A claude-shaped stream: all four modes, in both the separate and the
    /// combined form, then withdrawn on exit. PRD #611 names this agent as the
    /// one most exposed to a mis-derivation here.
    #[test]
    fn scan_mouse_mode_follows_an_app_managed_agent_through_enable_and_teardown() {
        let (flag, mut state) = mouse_scanner();
        assert!(
            !feed_mouse(b"\x1b[?1049h", &flag, &mut state),
            "the alternate screen on its own is not mouse reporting"
        );
        assert!(
            !feed_mouse(b"\x1b[?1000h\x1b[?1002h", &flag, &mut state),
            "a protocol is selected but no encoding yet — an SGR report would \
             not be understood, so the deck holds off one more chunk"
        );
        assert!(feed_mouse(b"\x1b[?1003h\x1b[?1006h", &flag, &mut state));
        assert!(feed_mouse(b"prompt> \x1b[1;1H", &flag, &mut state));
        assert!(
            !feed_mouse(
                b"\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l",
                &flag,
                &mut state
            ),
            "crossterm's DisableMouseCapture shape must land as disabled"
        );
        assert!(feed_mouse(b"\x1b[?1000;1002;1003;1006h", &flag, &mut state));
    }

    /// The terminal-managed shape PRD #611 measured: exact-height DECSTBM
    /// regions plus whole-region cursor-positioned repaints, and no mouse mode
    /// anywhere in it. Shares the fixture with the `mode/scroll` family rather
    /// than carrying a second copy — and stands in for the real captured codex
    /// session, which holds conversation content and is never committed.
    #[test]
    fn scan_mouse_mode_enables_nothing_on_a_terminal_managed_repaint_stream() {
        let (flag, mut state) = mouse_scanner();
        let stream = crate::ui::synthetic_decstbm_repaint_stream(51, 120);
        // In pieces, so its sequences straddle boundaries the way they do on a
        // real PTY.
        for piece in stream.chunks(997) {
            assert!(
                !feed_mouse(piece, &flag, &mut state),
                "a repainting agent requests no mouse reporting"
            );
        }
        // Plus the one private mode codex does set, on its own.
        assert!(!feed_mouse(b"\x1b[?1004h", &flag, &mut state));
    }

    /// The scanner is reached through the shared chunk path, so both backends
    /// derive the same flag from the same bytes — including across a boundary.
    #[test]
    fn scan_mouse_mode_is_wired_through_the_shared_chunk_path() {
        let parser = Mutex::new(vt100::Parser::new(24, 80, 100));
        let mut osc8 = Osc8Filter::new();
        let mouse = AtomicBool::new(false);
        let mut mouse_scan = MouseModeScanner::default();
        let links = Mutex::new(HyperlinkMap::new());
        let bytes_since_spawn = AtomicU64::new(0);

        for piece in [
            b"hello \x1b[?1006;1000;10".as_slice(),
            b"02h world".as_slice(),
        ] {
            process_agent_output_chunk(
                piece,
                &mut osc8,
                &parser,
                &mouse,
                &mut mouse_scan,
                &links,
                &bytes_since_spawn,
            );
        }
        assert!(
            mouse.load(Ordering::Relaxed),
            "a sequence split across two chunks must still be seen by a caller \
             that only ever sees `process_agent_output_chunk`"
        );
    }
}
