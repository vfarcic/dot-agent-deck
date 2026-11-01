use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use std::any::Any;

use crate::agent_pty::{self, DOT_AGENT_DECK_PANE_ID, PTY_RESIZE_DIM_MAX, TabMembership};
use crate::daemon_client::{
    AgentListing, AttachConnection, ClientError, DaemonClient, GatedQuery, Incarnation,
    StartAgentOptions, generate_client_id,
};
use crate::event::AgentType;
use crate::focus_report::FocusReporter;
use crate::hyperlink::{HyperlinkMap, Osc8Filter, Osc8Segment};
use crate::pane::{
    AgentSpawnOptions, BoundPane, PaneController, PaneDirection, PaneError, PaneInfo, RenameOutcome,
};
use crate::terminal_modes::{
    ModeDirective, MouseEncoding, MouseModes, MouseProtocol, PrivateModeScanner,
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
    /// `Some(Mode { ... })` → dashboard (deprecated, issue #1199), `Some(Orchestration {
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
    /// Issue #1395 item 1: the per-publish context file the daemon recorded
    /// for this pane's orchestration — present only on a start-role pane
    /// ([`crate::agent_pty::AgentRecord::orchestrator_context_path`]). The
    /// rebuilt tab carries it so compaction and `/clear` re-arm from the tab's
    /// own file — when it names a per-publish file directly under that
    /// orchestration's own cwd ([`crate::agent_pty::is_own_context_path`],
    /// checked where the tab is partitioned). `None` (any other pane, or an
    /// older daemon), or a path that fails that check, keeps the fixed-path
    /// mirror fallback.
    pub orchestrator_context_path: Option<std::path::PathBuf>,
    /// PRD #1666 part 2 (G-fix6/7): the daemon process that accepted this
    /// pane's attach, which its card is learned from — unnamed from an
    /// ungated controller. Not the event stream's, which may not be open yet
    /// when hydration seeds the card (PR #1817 review 4347247784).
    pub incarnation: Incarnation,
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
    /// What this pane is attached to: the daemon-side agent id used for
    /// `stop-agent` on close and `resize-agent` from the per-pane resize
    /// worker, this pane's viewer token, and the daemon process that issued
    /// both. Shared with the per-pane I/O task so that PRD #92 F12's
    /// auto-renew-on-respawn path can swap in the NEW agent's attachment after
    /// the daemon respawns the agent behind this pane (clear=true delegate
    /// flow). All readers take a brief lock + clone before issuing the RPC —
    /// never held across `.await`.
    attached: Arc<Mutex<PaneAttachment>>,
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
    /// PRD #882 — the last geometry the DAEMON said it applied for this pane.
    ///
    /// This, not the local parser, is what the per-frame sweep compares its
    /// layout target against. The parser used to be the comparison because it
    /// was kept "in lockstep with the PTY" by the client itself — an assumption
    /// only true while one client existed (issue #883). Under the size policy
    /// the applied geometry can differ from anything this client asked for, and
    /// can move without this client asking at all, so the client tracks what it
    /// last REQUESTED here and reconciles against what the daemon answers.
    requested: Arc<Mutex<Option<(u16, u16)>>>,
}

/// What a pane is attached to, read and replaced as ONE value (PRD #1666 part
/// 2, G-fix4).
///
/// The agent id and the viewer token are per-process counters, so each means
/// something only beside the daemon process that issued it: a restarted
/// daemon — the same build included — numbers unrelated agents and other
/// clients' viewers the same. Every request aimed at the pane's agent or viewer
/// is therefore bound to [`Self::incarnation`] ([`Self::client`]). Held under
/// one lock because a reattach replaces all three: a reader that could pair an
/// old id with a new daemon's process would send that id to the new daemon.
#[derive(Debug, Clone, Default)]
struct PaneAttachment {
    agent_id: String,
    /// PRD #882 — this pane's viewer token. `None` until the first attach
    /// answers, and again for a daemon that predates the policy (it echoes no
    /// token). A resize sent without one takes the daemon's unattributed path
    /// — the pre-#882 behaviour — which is the correct fallback in both cases:
    /// there is no viewer registered to update.
    viewer: Option<String>,
    /// The daemon process that accepted the attach, from the `Hello` on its
    /// own connection. `None` from an ungated controller, whose attach sends
    /// no `Hello`; its requests then stay unbound, as they always were.
    incarnation: Option<Incarnation>,
}

impl PaneAttachment {
    /// The attachment `conn` made to `agent_id`.
    fn of(agent_id: String, conn: &AttachConnection) -> Self {
        Self {
            agent_id,
            viewer: conn.viewer().map(str::to_string),
            incarnation: conn.incarnation().cloned(),
        }
    }

    /// `base`, bound to the daemon process this attachment came from, so a
    /// request naming its agent or viewer reaches that process or nothing.
    fn client(&self, base: &DaemonClient) -> DaemonClient {
        bound_to_known(base, self.incarnation.as_ref())
    }
}

/// `base` bound to `incarnation` when one is known (PRD #1666 part 2, G-fix4).
/// With none known, a gated `base` is bound to [`Incarnation::UNNAMED`], so a
/// request about an id learned from no named process sends nothing (G-fix6);
/// an ungated one, whose replies name no process, is returned as it is.
fn bound_to_known(base: &DaemonClient, incarnation: Option<&Incarnation>) -> DaemonClient {
    match incarnation {
        Some(incarnation) => bound_to_held(base, incarnation),
        None if base.is_gated() => base.bound_to(Incarnation::UNNAMED),
        None => base.clone(),
    }
}

/// `base` bound to `incarnation`, the process a held id was learned from
/// (PRD #1666 part 2, G-fix6). An unnamed one binds a gated `base` too, which
/// then sends nothing; on an ungated `base` it leaves the requests unbound,
/// as every reply there names no process.
fn bound_to_held(base: &DaemonClient, incarnation: &Incarnation) -> DaemonClient {
    if incarnation.is_unnamed() && !base.is_gated() {
        base.clone()
    } else {
        base.bound_to(incarnation.clone())
    }
}

/// Card incarnations by pane id — see
/// [`EmbeddedPaneController::card_incarnations`].
type CardIncarnations = Arc<Mutex<HashMap<String, Incarnation>>>;

/// The daemon process every request about `card`'s pane goes to (PRD #1666
/// part 2, G-fix6, G-fix7). A card learned from a named process is bound to
/// that process, whatever this controller holds under the same pane id: a
/// local pane attached on another process is another agent's, which the card
/// may not reach. A controller reference ([`BoundPane::held_by_controller`])
/// or a card learned from no named process is bound by the controller's own
/// record of the attach that produced the pane here, live or since detached,
/// when it has one. Neither is ever re-read from a newer reply.
fn held_incarnation(
    panes: &PaneRegistry,
    cards: &CardIncarnations,
    card: &BoundPane,
) -> Incarnation {
    if !card.incarnation.is_unnamed() {
        return card.incarnation.clone();
    }
    controller_record(panes, cards, &card.pane_id).unwrap_or_else(|| card.incarnation.clone())
}

/// The process this controller attached `pane_id` on, live or since
/// detached, if it did (PRD #1666 part 2, G-fix6).
fn controller_record(
    panes: &PaneRegistry,
    cards: &CardIncarnations,
    pane_id: &str,
) -> Option<Incarnation> {
    pane_attachment_in(panes, pane_id)
        .and_then(|attachment| attachment.incarnation)
        .or_else(|| cards.lock().unwrap().get(pane_id).cloned())
}

/// PRD #1666 part 2 (G-fix7): a pane's name to go back to while renames of it
/// are unanswered — see [`EmbeddedPaneController::rename_baselines`].
#[derive(Debug, Clone)]
struct RenameBaseline {
    name: String,
    unanswered: u32,
}

/// PRD #1666 part 2 (G-fix7): the deck's answer to one rename, for the render
/// loop to bring the dashboard's names in line with the pane's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameSettled {
    pub pane_id: String,
    /// The label the pane keeps — empty for a cleared one — or `None` when the
    /// deck refused the rename for a deck restart or an unnamed deck.
    pub label: Option<String>,
    /// Whether no other rename of the pane is left unanswered. A refusal is
    /// taken back only then, to the last confirmed name.
    pub last: bool,
}

/// PRD #1666 part 2 (G-fix7): how a card's pane id relates to this
/// controller's local pane under the same id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalPaneFor {
    /// No local pane under the card's pane id.
    None,
    /// The local pane is the card's: attached on the card's process, or the
    /// card names none and defers to the controller's record.
    Card,
    /// The local pane was attached on another process than the card's — a
    /// restarted deck's agent under a reused pane id, or the replaced deck's
    /// pane under a card of the new one.
    Other,
}

/// Whether `error` refuses an action because the daemon process it was bound
/// to is not the one answering — [`ClientError::DaemonRestarted`] — or names
/// none, [`ClientError::DaemonUnnamed`] (PRD #1666 part 2, G-fix5).
fn is_identity_refusal(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::DaemonRestarted | ClientError::DaemonUnnamed
    )
}

/// [`EmbeddedPaneController::notice_identity_refusal`] on the queue itself,
/// for a task that outlives the call that spawned it.
fn notice_identity_refusal(notices: &Mutex<Vec<String>>, error: &ClientError) -> bool {
    if !is_identity_refusal(error) {
        return false;
    }
    push_notice(notices, error.to_string());
    true
}

/// Queue `notice` for the status line, unless it repeats the one queued last:
/// a rename and then a close of the same gone pane say one thing, once.
fn push_notice(notices: &Mutex<Vec<String>>, notice: String) {
    let mut notices = notices.lock().unwrap();
    if notices.last() != Some(&notice) {
        notices.push(notice);
    }
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
    /// PRD #1666 part 2 (`lifecycle/handshake/011`): the daemon answering at
    /// the end of [`REATTACH_LOOKUP_TOTAL_BUDGET`] speaks another attach
    /// protocol than this binary, so the pane was never attached to it. The
    /// footer's refusal names both protocols.
    ProtocolMismatch,
    /// PRD #1666 part 2 (G-fix5): the daemon answering after the stream ended
    /// is not the process the pane's agent was attached on — the deck
    /// restarted. Its agent under the same pane id is someone else's, so it is
    /// not attached, and input typed meanwhile is dropped rather than sent to
    /// it.
    DeckRestarted,
    /// PRD #1666 part 2 (G-fix5): the pane's attach, or the daemon answering
    /// now, named no daemon process, so there is no telling whether it is the
    /// one the pane's agent ran under. Not attached, for the same reason.
    DeckUnidentified,
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
            Self::ProtocolMismatch => {
                "The daemon now speaks another attach protocol — not attaching; pane is disconnected."
            }
            // Opens with `AGENT_GONE_AFTER_RESTART`, the desktop's words.
            Self::DeckRestarted => {
                "that agent is gone (the deck restarted) — pane is disconnected. Close it to start over."
            }
            Self::DeckUnidentified => {
                "The deck did not identify itself, so this pane was not reconnected — pane is disconnected. Close it to start over."
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

/// What `pane_id`'s pane in `panes` is attached to, or `None` when there is no
/// such pane (PRD #1666 part 2, G-fix4).
fn pane_attachment_in(panes: &PaneRegistry, pane_id: &str) -> Option<PaneAttachment> {
    let panes = panes.lock().unwrap();
    panes
        .get(pane_id)
        .map(|pane| pane.backend.attached.lock().unwrap().clone())
}

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
    /// Issue #891 — whether the pane is currently on the ALTERNATE screen.
    ///
    /// Load-bearing for [`scrollback_depth`], not decoration beside it. `vt100`
    /// builds its alternate grid with a scrollback capacity of **zero**
    /// (`vt100-0.16.2/src/screen.rs:76`) and dispatches every scrollback read and
    /// write to whichever grid the mode selects, so while this is `true` the depth
    /// above is structurally `0` however much the pane has produced — and says
    /// nothing whatever about what the normal grid retained.
    pub alternate_screen: bool,
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
/// PRD #882 — reshape a pane's parser to `(rows, cols)`, but only if that is
/// actually a change.
///
/// `vt100::Screen::set_size` is **not** a no-op for an unchanged size: it walks
/// every row and calls `row.resize(..)` unconditionally, plus scroll-region
/// arithmetic. On a full-screen pane that is thousands of cell operations while
/// holding the parser mutex — the same mutex the render path takes to draw.
///
/// This matters because #882 made redundant calls the COMMON case. With one
/// client attached the daemon grants exactly what was asked, so the optimistic
/// write in `resize_pane_pty` and the confirming answer from the resize
/// response carry the same geometry, and the second one is pure waste that the
/// render loop can block on. Guarding here keeps the single-client path at one
/// reshape per resize, as it was before this PRD.
fn set_parser_size_if_changed(parser: &Mutex<vt100::Parser>, rows: u16, cols: u16) {
    let (rows, cols) = parser_init_dims(rows, cols);
    let Ok(mut parser) = parser.lock() else {
        return;
    };
    if crate::vt_grid::size(parser.screen()) == (rows, cols) {
        return;
    }
    crate::vt_grid::set_size(parser.screen_mut(), rows, cols);
}

/// guarded by [`guarded_parser_feed`].
fn new_pane_parser(rows: u16, cols: u16) -> vt100::Parser {
    let (rows, cols) = parser_init_dims(rows, cols);
    crate::vt_grid::parser(rows, cols, PANE_SCROLLBACK_LINES)
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
    ///
    /// PRD #1105 M11: built with a `client_id` ([`DaemonClient::with_client_id`])
    /// generated once in [`Self::new`], so every attach this controller makes —
    /// a new pane, both hydration paths, and the I/O task's re-attach after a
    /// respawn, all of which go through clones of this handle — and every
    /// resize connection names the same client. The TUI process builds one
    /// controller (`main.rs`), which is what makes this one id per process.
    client: DaemonClient,
    /// PRD #1105 M11: claims focus on the daemon under [`Self::client`]'s
    /// identity when the terminal reports focus-in or the person gives input.
    /// Fed by the event loop through [`Self::report_focus`].
    focus: FocusReporter,
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
    /// Issue #1705: the newest clipboard copy an agent asked its terminal for
    /// (OSC 52), from any pane, not yet passed on. One slot rather than a
    /// queue: each copy replaces the last on the clipboard, so a copy that is
    /// overtaken before the render loop drains it would only be overwritten.
    /// The pane I/O tasks set it; the render loop takes it each frame via
    /// [`Self::take_clipboard_copy`] and passes it on to the user's own
    /// terminal, from the UI thread so it cannot land inside a frame being
    /// drawn.
    clipboard_copy: Arc<Mutex<Option<String>>>,
    /// PRD #241 F3b (review finding G2): warnings from closes that COMPLETED
    /// without being able to confirm the daemon side (see
    /// [`StopOutcome::DoneUnverified`]). `close_pane` returns `Ok(())` on that
    /// path — the card is gone and the caller has nothing to retry — so the
    /// message cannot ride the `Result`. The render loop drains this each frame
    /// (exactly like [`Self::stream_rejections`]) into `ui.status_message`, the
    /// same status line every other close outcome already reports through.
    close_warnings: Arc<Mutex<Vec<String>>>,
    /// PRD #1666 part 2 (G-fix5): the daemon process each card's agent was
    /// attached on, by pane id, recorded when this controller wires the pane
    /// and kept after a detach, until the card is closed.
    ///
    /// A pane id is not an agent identity: a restarted daemon — the same build
    /// included — can hold an unrelated agent under the same pane id. So
    /// focusing or closing a card whose terminal is not attached here looks
    /// its agent up only on the process it was attached on
    /// ([`Self::hydrate_pane`], [`Self::stop_unattached_pane`]), and a
    /// successor's agent is never adopted in its place. A card this controller
    /// never attached — one painted from a daemon-spawned `SessionStart` — has
    /// no entry here; it is looked up on the process the UI learned it from,
    /// which the call carries ([`BoundPane`], G-fix6), and a gated controller
    /// sends nothing for a card learned from no named process.
    card_incarnations: CardIncarnations,
    /// PRD #1666 part 2 (G-fix6, G-fix7): the deck's answers to renames
    /// [`PaneController::rename_pane`] had already applied. One refused because
    /// the daemon process the pane was attached on is gone, or named none, has
    /// the pane's own name put back here; the render loop drains these
    /// ([`Self::take_rename_answers`]) and puts the dashboard's names back too,
    /// so the card keeps the name it had, as the desktop's does.
    rename_refusals: Arc<Mutex<Vec<RenameSettled>>>,
    /// PRD #1666 part 2 (G-fix7): per pane with a rename the deck has not
    /// answered yet, the name to go back to if the deck refuses — the one it
    /// last confirmed, or the pane's name before the first unanswered rename —
    /// and how many renames are unanswered. A refused optimistic name is never
    /// a later rename's way back, however the answers interleave.
    rename_baselines: Arc<Mutex<HashMap<String, RenameBaseline>>>,
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

/// Issue #1540 — what the attached daemon said about the New agent form's
/// last command: who keeps it, and the daemon's value when the daemon does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonLastCommand {
    pub keeper: crate::daemon_client::LastCommandKeeper,
    /// `None` when the client keeps the value, or the daemon has none yet.
    pub command: Option<String>,
}

/// Issue #1540 — the New agent form's view of its deck's remembered command,
/// from [`EmbeddedPaneController::last_command_reader`].
#[derive(Clone)]
pub struct LastCommandReader {
    client: DaemonClient,
    runtime: tokio::runtime::Handle,
}

impl LastCommandReader {
    /// Ask the daemon, blocking the calling (TUI) thread for at most
    /// `timeout`. `None` when it does not answer in time or at all, which the
    /// form treats exactly like a daemon that does not keep the value.
    pub fn read(&self, timeout: Duration) -> Option<DaemonLastCommand> {
        use crate::daemon_client::{GatedQuery, LastCommandKeeper};
        let client = self.client.clone();
        self.runtime.block_on(async move {
            tokio::time::timeout(timeout, async move {
                let keeper = client.last_command_keeper().await.ok()?;
                let command = match keeper {
                    LastCommandKeeper::Client => None,
                    LastCommandKeeper::Daemon => match client.new_agent_options().await.ok()? {
                        GatedQuery::Answered(options) => options.last_command,
                        GatedQuery::Unsupported => {
                            return Some(DaemonLastCommand {
                                keeper: LastCommandKeeper::Client,
                                command: None,
                            });
                        }
                    },
                };
                Some(DaemonLastCommand { keeper, command })
            })
            .await
            .ok()
            .flatten()
        })
    }

    /// Offer `command` as the deck's last command if it has none yet
    /// ([`DaemonClient::seed_last_command`]), without waiting: it runs on the
    /// runtime, bounded by `timeout`, and a failure only means the value is
    /// offered again next time.
    pub fn seed(&self, command: String, timeout: Duration) {
        let client = self.client.clone();
        self.runtime.spawn(async move {
            if let Ok(Err(e)) =
                tokio::time::timeout(timeout, client.seed_last_command(&command)).await
            {
                tracing::debug!(error = %e, "seed-last-command failed");
            }
        });
    }
}

/// PRD #1258 M3 — what one host-metrics request ended with, as the host
/// overlay reads it from [`HostMetricsReader::take`].
pub type HostMetricsAnswer = Result<crate::daemon_client::HostMetricsReport, String>;

/// PRD #1258 M3 — fetches the attached daemon's host sample for the TUI's host
/// overlay **without blocking the TUI thread**: [`Self::request`] spawns the
/// query on the runtime and returns at once, and the render loop collects the
/// answer with [`Self::take`] on a later frame. At most one request is in
/// flight; a request made while one is outstanding is dropped, so a held key or
/// a fast refresh can never queue connections against a slow daemon.
///
/// The capability check is [`DaemonClient::host_metrics`]'s, so a daemon that
/// predates the verb is never sent it and answers
/// [`crate::daemon_client::HostMetricsReport::NotAvailable`].
#[derive(Clone)]
pub struct HostMetricsReader {
    client: DaemonClient,
    runtime: tokio::runtime::Handle,
    slot: Arc<Mutex<HostMetricsSlot>>,
}

#[derive(Default)]
struct HostMetricsSlot {
    in_flight: bool,
    answer: Option<HostMetricsAnswer>,
}

impl HostMetricsReader {
    /// Start a request bounded by `timeout`, unless one is already in flight.
    /// Returns whether a request was started.
    pub fn request(&self, timeout: Duration) -> bool {
        {
            let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
            if slot.in_flight {
                return false;
            }
            slot.in_flight = true;
        }
        let client = self.client.clone();
        let slot = Arc::clone(&self.slot);
        self.runtime.spawn(async move {
            let answer = match tokio::time::timeout(timeout, client.host_metrics()).await {
                Ok(Ok(report)) => Ok(report),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("the deck did not answer in time".to_string()),
            };
            let mut slot = slot.lock().unwrap_or_else(|p| p.into_inner());
            slot.in_flight = false;
            slot.answer = Some(answer);
        });
        true
    }

    /// The answer to the last request, once it has arrived; `None` while it is
    /// still in flight or after it was already taken.
    pub fn take(&self) -> Option<HostMetricsAnswer> {
        self.slot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .answer
            .take()
    }

    /// Whether a request is outstanding.
    pub fn in_flight(&self) -> bool {
        self.slot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .in_flight
    }
}

/// The answer to a Retry or Cancel press: the pane and agent generation the
/// press was for, whether it was a Retry, and the daemon's outcome or why
/// none came.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxActionAnswer {
    pub pane_id: String,
    pub agent_id: String,
    pub retry: bool,
    pub outcome: Result<String, String>,
}

/// The latest press and, once it arrived, the answer to it. An answer to an
/// earlier press is dropped, so the picker shows what its last press did,
/// whichever answer comes back first (PR #1735 review 4301453654).
#[derive(Debug, Default)]
struct InboxActionSlot {
    press: u64,
    answer: Option<InboxActionAnswer>,
}

impl InboxActionSlot {
    /// A new press: the answer to any earlier one is no longer wanted.
    fn begin(&mut self) -> u64 {
        self.press += 1;
        self.answer = None;
        self.press
    }

    /// The answer to `press`, kept only while it is the latest.
    fn land(&mut self, press: u64, answer: InboxActionAnswer) {
        if press == self.press {
            self.answer = Some(answer);
        }
    }
}

/// PRD #1666 §6.4: sends the TUI's Retry and Cancel presses to the daemon off
/// the event loop, and holds the answer for the picker to show.
#[derive(Clone)]
pub struct InboxActionSender {
    to: InboxActionTarget,
    slot: Arc<Mutex<InboxActionSlot>>,
}

/// One recorded press: pane, agent, message, revision and whether it was a
/// Retry.
type RecordedPress = (String, String, String, u64, bool);

#[derive(Clone)]
enum InboxActionTarget {
    Daemon {
        client: DaemonClient,
        runtime: tokio::runtime::Handle,
        /// The controller's panes, to bind a press to the daemon process its
        /// agent was attached from (PRD #1666 part 2, G-fix4).
        panes: PaneRegistry,
        /// And its card record, for a card whose pane is detached (G-fix6).
        cards: CardIncarnations,
    },
    /// The TUI's L1 seam: each press is recorded, nothing is sent.
    Recorder(Arc<Mutex<Vec<RecordedPress>>>),
}

impl InboxActionSender {
    /// Send one press — Retry (`retry`) or Cancel — for `message_id` of
    /// `agent_id` in `card`'s pane, at `revision`, bounded by `timeout`.
    ///
    /// PRD #1666 part 2 (G-fix6): `card` carries the daemon process the card
    /// the press was made on was learned from. The press is bound to the
    /// process this TUI attached the pane's agent on when it did, and to that
    /// one otherwise ([`held_incarnation`]), so a restarted daemon's namesake
    /// is sent nothing, attached or not.
    pub fn send(
        &self,
        card: &BoundPane,
        agent_id: &str,
        message_id: &str,
        revision: u64,
        retry: bool,
        timeout: Duration,
    ) {
        let pane_id = card.pane_id.as_str();
        let (client, runtime) = match &self.to {
            InboxActionTarget::Daemon {
                client,
                runtime,
                panes,
                cards,
            } => {
                // An attachment to another agent than the press's speaks for
                // that agent, not this one: the card's record decides.
                let attached_here = pane_attachment_in(panes, pane_id)
                    .is_none_or(|attachment| attachment.agent_id == agent_id);
                let incarnation = if attached_here {
                    held_incarnation(panes, cards, card)
                } else {
                    card.incarnation.clone()
                };
                (bound_to_held(client, &incarnation), runtime.clone())
            }
            InboxActionTarget::Recorder(sent) => {
                sent.lock().unwrap_or_else(|p| p.into_inner()).push((
                    pane_id.to_string(),
                    agent_id.to_string(),
                    message_id.to_string(),
                    revision,
                    retry,
                ));
                return;
            }
        };
        let slot = Arc::clone(&self.slot);
        let press = slot.lock().unwrap_or_else(|p| p.into_inner()).begin();
        let (pane_id, agent_id, message_id) = (
            pane_id.to_string(),
            agent_id.to_string(),
            message_id.to_string(),
        );
        let request_id = generate_client_id();
        runtime.spawn(async move {
            let answer = match tokio::time::timeout(
                timeout,
                client.inbox_action(
                    &pane_id,
                    &agent_id,
                    &message_id,
                    revision,
                    retry,
                    &request_id,
                ),
            )
            .await
            {
                Ok(Ok(crate::daemon_client::GatedQuery::Answered(reply))) => {
                    serde_json::to_value(reply.outcome)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_string))
                        .ok_or_else(|| {
                            "the deck's answer was not one this build understands".to_string()
                        })
                }
                Ok(Ok(crate::daemon_client::GatedQuery::Unsupported)) => {
                    Ok("unsupported".to_string())
                }
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("the deck did not answer in time".to_string()),
            };
            slot.lock().unwrap_or_else(|p| p.into_inner()).land(
                press,
                InboxActionAnswer {
                    pane_id,
                    agent_id,
                    retry,
                    outcome: answer,
                },
            );
        });
    }

    /// The answer to the last press, once it has arrived: what it was for,
    /// and the daemon's outcome (`applied`, `stale`, …) or why none came.
    pub fn take(&self) -> Option<InboxActionAnswer> {
        self.slot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .answer
            .take()
    }

    /// Test seam: `answer` arrives for a press made now.
    #[cfg(test)]
    pub(crate) fn answer_for_test(&self, answer: InboxActionAnswer) {
        let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        let press = slot.begin();
        slot.land(press, answer);
    }

    /// The TUI's L1 seam: a sender that records each press, and the record.
    #[doc(hidden)]
    pub fn recording() -> (Self, Arc<Mutex<Vec<RecordedPress>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        (
            InboxActionSender {
                to: InboxActionTarget::Recorder(Arc::clone(&sent)),
                slot: Arc::default(),
            },
            sent,
        )
    }
}

impl EmbeddedPaneController {
    /// Build a controller whose panes are stream-backed against the daemon
    /// at `socket_path`. Caller is responsible for ensuring the daemon is
    /// actually running — `daemon_attach::ensure_external_daemon_or_die`
    /// is the canonical pre-flight from `main`.
    pub fn new(socket_path: PathBuf, runtime: tokio::runtime::Handle) -> Self {
        let client = DaemonClient::new(socket_path).with_client_id(generate_client_id());
        Self {
            panes: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(Mutex::new(1)),
            focus: FocusReporter::new(client.clone(), runtime.clone()),
            client,
            runtime,
            stream_rejections: Arc::new(Mutex::new(Vec::new())),
            clipboard_copy: Arc::new(Mutex::new(None)),
            close_warnings: Arc::new(Mutex::new(Vec::new())),
            card_incarnations: Arc::new(Mutex::new(HashMap::new())),
            rename_refusals: Arc::new(Mutex::new(Vec::new())),
            rename_baselines: Arc::new(Mutex::new(HashMap::new())),
            any_scroll_notice_armed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// PRD #1666 part 2 (`lifecycle/handshake/011`): every pane this controller
    /// starts, attaches, hydrates or reattaches, and every listing behind
    /// those, is admitted only by a positive protocol check on the connection
    /// that carries it ([`DaemonClient::with_protocol_gate`]). The TUI builds
    /// its controller this way: the daemon behind its socket can be replaced
    /// by another build at any time after the startup handshake, and a footer
    /// saying so blocks nothing by itself.
    ///
    /// Closing an attached pane keeps its own ungated `stop-agent`, so a person
    /// can still stop the agent of a pane they already have open.
    pub fn with_protocol_gate(mut self) -> Self {
        self.client = self.client.with_protocol_gate();
        self
    }

    /// Issue #1540: a handle the New agent form reads its pre-fill through —
    /// on this controller's own client, so it asks the very daemon the TUI is
    /// attached to, local or remote.
    pub fn last_command_reader(&self) -> LastCommandReader {
        LastCommandReader {
            client: self.client.clone(),
            runtime: self.runtime.clone(),
        }
    }

    /// PRD #1258 M3: a handle the host overlay fetches through — on this
    /// controller's own client, so it describes the host of the very daemon the
    /// TUI is attached to, local or remote.
    /// PRD #1666 §6.4: the sender of this controller's Retry and Cancel
    /// presses, on the same client as every other attach.
    pub fn inbox_action_sender(&self) -> InboxActionSender {
        InboxActionSender {
            to: InboxActionTarget::Daemon {
                client: self.client.clone(),
                runtime: self.runtime.clone(),
                panes: Arc::clone(&self.panes),
                cards: Arc::clone(&self.card_incarnations),
            },
            slot: Arc::default(),
        }
    }

    /// PRD #1666 part 2 (G-fix4): what `pane_id`'s pane is attached to, read
    /// as one value, or `None` when this TUI has no pane for it.
    fn pane_attachment(&self, pane_id: &str) -> Option<PaneAttachment> {
        pane_attachment_in(&self.panes, pane_id)
    }

    pub fn host_metrics_reader(&self) -> HostMetricsReader {
        HostMetricsReader {
            client: self.client.clone(),
            runtime: self.runtime.clone(),
            slot: Arc::new(Mutex::new(HostMetricsSlot::default())),
        }
    }

    /// PRD #1105 M11: the identity every attach from this controller carries,
    /// and the one [`Self::report_focus`] claims focus as.
    pub fn client_id(&self) -> Option<&str> {
        self.client.client_id()
    }

    /// PRD #1105 M11: feed one terminal event to the focus reporter, which
    /// claims focus on the daemon when the event is a focus-in (always) or input
    /// (throttled) — see [`crate::focus_report`]. Never blocks; the returned
    /// handle is only for tests that need to wait for the claim.
    pub fn report_focus(
        &self,
        event: &crossterm::event::Event,
        now: Instant,
    ) -> Option<tokio::task::JoinHandle<()>> {
        self.focus.observe(event, now)
    }

    /// PRD #20 R20-007 (finding #10): drain and return the typed stream
    /// rejections the daemon has pushed since the last call — `(pane_id, reason)`
    /// pairs. The render loop consumes these each frame to surface honest input
    /// feedback and leave PaneInput.
    pub fn take_stream_rejections(&self) -> Vec<(String, String)> {
        std::mem::take(&mut *self.stream_rejections.lock().unwrap())
    }

    /// Issue #1705: take the newest clipboard copy an agent has asked for
    /// since the last call, already validated by
    /// [`crate::osc52::osc52_copy_text`]. The render loop passes it on to the
    /// user's terminal.
    pub fn take_clipboard_copy(&self) -> Option<String> {
        self.clipboard_copy.lock().unwrap().take()
    }

    /// PRD #241 F3b (review finding G2): drain and return the warnings queued by
    /// closes that completed WITHOUT being able to confirm the daemon side. The
    /// render loop consumes these each frame and shows them on the status line —
    /// a possibly-orphaned agent must never be a silent outcome. Empty on every
    /// ordinary close, successful or failed.
    ///
    /// PRD #1666 part 2 (G-fix5): the same queue carries the refusals of
    /// actions whose result arrives after the call returns — a rename, a focus
    /// or a reattach that found the deck restarted — so the user reads them on
    /// the same status line.
    pub fn take_close_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.close_warnings.lock().unwrap())
    }

    /// PRD #1666 part 2 (G-fix6): drain the panes whose renames the deck
    /// refused after the call returned and that have none left unanswered —
    /// see [`Self::rename_refusals`]. Drops the other answers; the render loop
    /// reads them all through [`Self::take_rename_answers`].
    pub fn take_rename_refusals(&self) -> Vec<String> {
        self.take_rename_answers()
            .into_iter()
            .filter(|answer| answer.label.is_none() && answer.last)
            .map(|answer| answer.pane_id)
            .collect()
    }

    /// PRD #1666 part 2 (G-fix7): drain the deck's answers to renames
    /// ([`RenameSettled`]), refused and kept alike.
    pub fn take_rename_answers(&self) -> Vec<RenameSettled> {
        std::mem::take(&mut *self.rename_refusals.lock().unwrap())
    }

    /// PRD #1666 part 2 (G-fix6): this controller's client, bound to the daemon
    /// process every request about `card`'s pane goes to
    /// ([`held_incarnation`]).
    fn held_client(&self, card: &BoundPane) -> DaemonClient {
        bound_to_held(
            &self.client,
            &held_incarnation(&self.panes, &self.card_incarnations, card),
        )
    }

    /// PRD #1666 part 2 (G-fix7): whether the local pane under `card`'s pane
    /// id is the card's ([`LocalPaneFor`]).
    fn local_pane_for(&self, card: &BoundPane) -> LocalPaneFor {
        let Some(attachment) = self.pane_attachment(&card.pane_id) else {
            return LocalPaneFor::None;
        };
        if card.incarnation.is_unnamed() {
            return LocalPaneFor::Card;
        }
        match attachment.incarnation {
            Some(local) if local == card.incarnation => LocalPaneFor::Card,
            _ => LocalPaneFor::Other,
        }
    }

    /// PRD #1666 part 2 (G-fix7): the local pane under `card`'s pane id belongs
    /// to another process than the card. When that pane has lost its agent —
    /// the deck that ran it was replaced — it is dropped here, so the card's
    /// own agent can be attached in its place, and this answers `true`. A pane
    /// still attached is someone else's live agent: it is kept, and the card's
    /// action is refused.
    fn drop_replaced_local_pane(&self, card: &BoundPane) -> bool {
        if self.pane_lost_reason(&card.pane_id).is_none() {
            return false;
        }
        self.panes.lock().unwrap().remove(&card.pane_id);
        true
    }

    /// Drop `pane_id`'s card from [`Self::card_incarnations`]: its close is
    /// done, so a later card under the same pane id is a new one.
    fn forget_card(&self, pane_id: &str) {
        self.card_incarnations.lock().unwrap().remove(pane_id);
    }

    /// Queue `error`'s sentence for the status line when it is a refusal of an
    /// action on an agent the deck can no longer vouch for — a restart, or a
    /// deck that named no process — and say whether it was one.
    fn notice_identity_refusal(&self, error: &ClientError) -> bool {
        notice_identity_refusal(&self.close_warnings, error)
    }

    /// Issue #1913: record that `pane_id`'s agent was lost to a deck restart,
    /// as its reattach does once it meets the restarted deck, when the pane is
    /// still attached on `replaced` — a process the caller knows the deck
    /// replaced. Answers whether it was recorded.
    pub(crate) fn mark_lost_to_restart(&self, pane_id: &str, replaced: &Incarnation) -> bool {
        let panes = self.panes.lock().unwrap();
        let Some(pane) = panes.get(pane_id) else {
            return false;
        };
        let attached_on_replaced = pane
            .backend
            .attached
            .lock()
            .unwrap()
            .incarnation
            .as_ref()
            .is_some_and(|attached_on| attached_on.same_process(replaced));
        if attached_on_replaced {
            *pane.backend.lost.lock().unwrap() = Some(PaneLostReason::DeckRestarted);
        }
        attached_on_replaced
    }

    /// Issue #1913: whether this controller holds a local pane under
    /// `pane_id` at all, attached or not.
    pub(crate) fn holds_pane(&self, pane_id: &str) -> bool {
        self.panes.lock().unwrap().contains_key(pane_id)
    }

    /// Test-only: record that `pane_id`'s agent was lost for `reason`, as
    /// the pane's reattach does, without waiting out a reattach.
    #[cfg(all(test, unix))]
    pub(crate) fn mark_pane_lost_for_test(&self, pane_id: &str, reason: PaneLostReason) {
        let panes = self.panes.lock().unwrap();
        *panes
            .get(pane_id)
            .expect("the pane")
            .backend
            .lost
            .lock()
            .unwrap() = Some(reason);
    }

    /// Test-only: end `pane_id`'s I/O task having recorded `reason`, as a
    /// reattach that gives up does, and return once input to the pane fails.
    #[cfg(all(test, unix))]
    pub(crate) fn end_pane_io_for_test(&self, pane_id: &str, reason: PaneLostReason) {
        let input_tx = {
            let mut panes = self.panes.lock().unwrap();
            let pane = panes.get_mut(pane_id).expect("the pane");
            *pane.backend.lost.lock().unwrap() = Some(reason);
            if let Some(io_task) = pane.backend.io_task.take() {
                io_task.abort();
            }
            pane.backend.input_tx.clone()
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !input_tx.is_closed() {
            assert!(
                std::time::Instant::now() < deadline,
                "the pane's I/O task never ended"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
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
    /// focused, which a vanished pane with no successor really does
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
        let (rows, cols) = crate::vt_grid::size(parser.screen());
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
                // Render-only seam: no attach, so no agent, viewer or process.
                attached: Arc::new(Mutex::new(PaneAttachment::default())),
                input_tx,
                io_task: None,
                io_state: Arc::new(AtomicU8::new(IO_FINISHED)),
                runtime: render_only_runtime(),
                daemon_path: render_only_socket_path(),
                resize_tx,
                resize_task: None,
                lost: Arc::new(Mutex::new(None)),
                // Render-only seam: nothing requested, for the pane's life.
                requested: Arc::new(Mutex::new(None)),
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
        // PRD #882: record what this pane ASKED for. The per-frame sweep
        // compares its layout target against this rather than against the
        // parser, so it issues one request per genuine layout change even while
        // the daemon is holding the agent at a smaller size for another client.
        // Comparing against the parser instead would re-send on every frame
        // forever, because under this policy the parser holds the daemon's
        // answer and the target holds the wish, and the two legitimately differ.
        *pane.backend.requested.lock().unwrap() = Some((rows, cols));
        // `send_replace` overwrites whatever value was pending and ignores
        // the no-receivers case (the worker would only be gone if the
        // pane was being torn down — losing the resize is the right
        // outcome there). The watch channel cannot block, so this returns
        // immediately and never holds the pane lock across daemon I/O.
        let _ = pane.backend.resize_tx.send_replace(Some((rows, cols)));
        // PRD #882: set the parser optimistically, and let the daemon's answer
        // correct it.
        //
        // The first cut of this dropped the optimistic write entirely, on the
        // reasoning that the daemon decides the geometry so the client should
        // wait to be told. That is wrong for the case that dominates: with one
        // client attached the daemon grants exactly what was asked, so waiting
        // buys nothing and costs a full round trip of lag on every resize — the
        // pane renders its old grid inside its new box until the answer lands.
        // It also makes the pane's geometry change at a moment unrelated to the
        // frame that caused it, which is a race for anything comparing rendered
        // frames across a layout change.
        //
        // Optimistic-then-corrected keeps the single-client path byte-for-byte
        // what it was, and in the multi-client case the transient is in the SAFE
        // direction: the parser is briefly LARGER than the PTY, so the agent's
        // bytes land where they were drawn (no clamped cursor, no overprinting)
        // with stale columns to the right until the SIGWINCH redraw repaints —
        // and the answer, or a `KIND_GEOMETRY` push, shrinks it immediately.
        //
        // This does NOT reintroduce issue #883. That bug was the per-frame
        // sweep COMPARING against this parser to decide whether to resize;
        // the sweep now compares against `requested` above, so the parser
        // holding an optimistic value cannot suppress a needed request.
        set_parser_size_if_changed(&pane.screen, rows, cols);
        Ok(())
    }

    /// PRD #882 — this pane's viewer token, or `None` if the pane never attached
    /// as a viewer (the render-only seam) or is talking to a daemon that
    /// predates the policy and echoed none.
    ///
    /// Exposed so a test can assert that a pane actually registered — "the
    /// resize carried a token" is the difference between participating in the
    /// policy and silently taking the daemon's unattributed override path, and
    /// the two are indistinguishable from the outside without it.
    pub fn viewer_token(&self, pane_id: &str) -> Option<String> {
        let panes = self.panes.lock().unwrap();
        panes
            .get(pane_id)?
            .backend
            .attached
            .lock()
            .ok()?
            .viewer
            .clone()
    }

    /// PRD #882 — the geometry this pane last ASKED the daemon for, or `None`
    /// if it has not asked yet.
    ///
    /// The per-frame sweep in `ui.rs` compares its layout target against this to
    /// decide whether a request is new. It deliberately does not consult the
    /// parser: under the size policy the parser holds the daemon's ANSWER, which
    /// legitimately differs from the request whenever another client's pane is
    /// smaller — so a parser-based comparison would find a delta on every frame
    /// and re-send forever.
    pub fn requested_dims(&self, pane_id: &str) -> Option<(u16, u16)> {
        let panes = self.panes.lock().unwrap();
        *panes.get(pane_id)?.backend.requested.lock().ok()?
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
        let (rows, cols) = crate::vt_grid::size(parser.screen());
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
        let alternate_screen = parser.screen().alternate_screen();
        Some(PaneScrollFacts {
            scrollback_depth,
            alternate_screen,
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
        // Issue #1540: a New agent form submit — start it through
        // `DaemonClient::start_form_agent`, so a deck that keeps the last
        // command records it once it has accepted the start.
        remember_command: bool,
        // Issue #1496: the authoring kind of a form start the TUI seeds
        // itself, recorded by the daemon on the agent.
        authoring_kind: Option<crate::authoring_seeds::AuthoringKind>,
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
        let (agent_id, conn) = runtime
            .block_on(async move {
                // PRD #1666 part 2 (G-fix4): the start, the attach and the
                // cleanup stop all reach the daemon process answering now, so
                // the agent this pane attaches to, or stops, is the one it
                // started — never a restarted daemon's agent with the same id.
                // On the TUI's gated client this costs one `Hello`, under the
                // start's bound.
                let start = async {
                    let client_for_calls = client.bound_to_current().await?;
                    let id = if remember_command {
                        client_for_calls
                            .start_client_seeded_form_agent(opts, authoring_kind)
                            .await
                            .map(|form| form.agent_id)?
                    } else {
                        client_for_calls.start_agent(opts).await?
                    };
                    Ok::<_, ClientError>((id, client_for_calls))
                };
                let (id, client_for_calls) = match tokio::time::timeout(
                    CREATE_PANE_START_TIMEOUT,
                    start,
                )
                .await
                {
                    Ok(Ok(started)) => started,
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
                    // PRD #882: the pane's own geometry — the same `(rows, cols)`
                    // this pane's agent was just spawned at — declared as this
                    // viewer's viewport. Declaring it at attach rather than
                    // waiting for the first layout sweep means a second client
                    // attaching in between cannot size the agent as if this pane
                    // were not there.
                    client_for_calls.attach_as_viewer(&id, Some((rows, cols))),
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
        // dims straight from `ui.rs`'s layout helpers — `right_column_pane_dims`
        // ends in a `saturating_sub(2)` border allowance — so a short or narrow terminal
        // arrives here with a zero axis and no caller in between rejects it, the
        // way `resize_panes_to_layout` does on the resize path.
        //
        // PRD #882 (raised by Greptile on PR #895): prefer the geometry the
        // attach response says the daemon has APPLIED over the dims the caller
        // computed. The caller's value is one of two stale things — the
        // `AgentRecord` read before this attach, or the pane's own layout target
        // — and another viewer can have moved the minimum in between, because
        // attaching is itself what registers this viewer and can shrink the
        // agent. The snapshot that is about to be replayed on this connection
        // was taken under the same daemon lock as `applied`, so it is the ONLY
        // geometry those bytes are interpretable at. Parsing them at the
        // caller's dims is PRD #104's scramble, reached through the new path.
        let (rows, cols) = conn.applied().unwrap_or((rows, cols));
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
        // need to follow the agent across daemon-side respawns (F9
        // clear=true delegate flow kills + replaces the agent under the
        // same pane_id_env). Share one `Arc<Mutex<PaneAttachment>>` between
        // them and `StreamBackend` so a single update in the io_task is
        // visible to the next `stop-agent` / `resize-agent` call without
        // rewiring.
        //
        // PRD #882: the viewer token rides in it for the same reason — a
        // respawn re-attaches and mints a new one, and the resize worker must
        // send whichever is current. Seeded from the attach that produced
        // `conn`, so the very first resize already names this viewer rather
        // than falling through to the daemon's unattributed path. PRD #1666
        // part 2 (G-fix4): and so does the daemon process both came from.
        let shared_attached = Arc::new(Mutex::new(PaneAttachment::of(agent_id, &conn)));
        // PRD #1666 part 2 (G-fix5): and the card keeps that process after a
        // detach, so focusing or closing it later cannot adopt a successor's
        // agent under the same pane id.
        if let Some(incarnation) = conn.incarnation() {
            self.card_incarnations
                .lock()
                .unwrap()
                .insert(pane_id.clone(), incarnation.clone());
        }

        // Per-pane resize worker: at-most-one in-flight daemon Resize per
        // pane, with intermediate values coalesced via the watch channel.
        // Survives until either `resize_tx` drops (pane removed) or the
        // worker is aborted by `StreamBackend::drop`. See the comment on
        // `resize_pane_pty` for the full rationale.
        let requested = Arc::new(Mutex::new(None));

        let resize_task = runtime.spawn(resize_worker(
            resize_rx,
            self.client.clone(),
            Arc::clone(&shared_attached),
            Arc::clone(&parser),
        ));

        let parser_for_task = Arc::clone(&parser);
        let mouse_mode_for_task = Arc::clone(&mouse_mode);
        let hyperlinks_for_task = Arc::clone(&hyperlinks);
        let bytes_for_task = Arc::clone(&bytes_since_spawn);
        let attached_for_task = Arc::clone(&shared_attached);
        let client_for_task = self.client.clone();
        let pane_id_for_task = pane_id.clone();
        let rejections_for_task = Arc::clone(&self.stream_rejections);
        let clipboard_for_task = Arc::clone(&self.clipboard_copy);
        let notices_for_task = Arc::clone(&self.close_warnings);

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
                attached_for_task,
                input_rx,
                parser_for_task,
                mouse_mode_for_task,
                hyperlinks_for_task,
                bytes_for_task,
                rejections_for_task,
                clipboard_for_task,
                notices_for_task,
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
                attached: shared_attached,
                input_tx,
                io_task: Some(io_task),
                io_state,
                runtime,
                daemon_path,
                resize_tx,
                resize_task: Some(resize_task),
                lost,
                requested,
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
    /// show "No active agents" even though the daemon owns live PTYs.
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
        let (records, incarnation) = match runtime.block_on(async move {
            tokio::time::timeout(HYDRATE_LIST_TIMEOUT, list_client.list_agents_detailed()).await
        }) {
            Ok(Ok(listing)) => (listing.records, listing.incarnation),
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
        // PRD #1666 part 2 (G-fix4): each attach is bound to the daemon process
        // that listed the agent, so a daemon restarted in between is not
        // attached under the listed id.
        let attach_client = bound_to_known(&client, incarnation.as_ref());
        for record in records {
            let agent_id = record.id.clone();
            let client_for_attach = attach_client.clone();
            let id_for_attach = agent_id.clone();
            // Bounded per-agent attach: same rationale as the list-agents
            // timeout above, scaled down because there can be up to
            // HYDRATE_MAX_PANES of these in series.
            // PRD #882: the viewport this attach declares. `record.rows/cols`
            // is the daemon's own current geometry (PRD #104 plumbed it through
            // `list_agents`), so declaring it registers this viewer without
            // moving the size. `(0, 0)` from a daemon predating #104 means "we
            // do not know" — declare nothing rather than guess, since a guessed
            // constraint under a smallest-wins policy would shrink the agent for
            // every other client too.
            let viewport_for_attach = match (record.rows, record.cols) {
                (0, _) | (_, 0) => None,
                (rows, cols) => Some((rows, cols)),
            };
            let attach_result = runtime.block_on(async move {
                tokio::time::timeout(
                    HYDRATE_ATTACH_TIMEOUT,
                    // PRD #882: declare the DAEMON's own current geometry as
                    // this viewer's viewport. It constrains nothing that was not
                    // already true, so hydration cannot shrink an agent somebody
                    // else is using, and it opts the attach into geometry
                    // pushes — which a rehydrating pane needs, because the first
                    // thing that can move the size afterwards is another client.
                    // The pane's real target arrives one frame later from
                    // `resize_panes_to_layout` and reconciles it.
                    client_for_attach.attach_as_viewer(&id_for_attach, viewport_for_attach),
                )
                .await
            });
            let conn = match attach_result {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    // Race: agent terminated between list_agents and
                    // attach, or transient daemon error. Skip this id
                    // and keep going so a single missing agent doesn't
                    // sink the rest of the rehydration. PRD #1666 part 2
                    // (G-fix6): a deck replaced since the listing is also
                    // said on the status line.
                    self.notice_identity_refusal(&e);
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
            let attached_on = conn.incarnation().cloned().unwrap_or_default();
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
                orchestrator_context_path: record
                    .orchestrator_context_path
                    .as_deref()
                    .map(std::path::PathBuf::from),
                incarnation: attached_on,
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
    fn stop_unattached_pane(&self, card: &BoundPane) -> Result<(), PaneError> {
        let pane_id = card.pane_id.as_str();
        // PRD #1666 part 2 (G-fix5, G-fix6): the card's agent is looked up only
        // on the daemon process it was attached on here, or else the one the
        // card was learned from — never on a successor that holds an unrelated
        // agent under the same pane id.
        let list_client = self.held_client(card);
        let (records, incarnation) = match self.runtime.block_on(async move {
            tokio::time::timeout(HYDRATE_LIST_TIMEOUT, list_client.list_agents_detailed()).await
        }) {
            Ok(Ok(listing)) => (listing.records, listing.incarnation),
            // The card's agent is not reachable through this deck any more, or
            // the card names no process to reach it on: the close is done
            // here, and stops nothing on whatever answers now.
            Ok(Err(e)) if is_identity_refusal(&e) => {
                self.forget_card(pane_id);
                self.notice_identity_refusal(&e);
                return Ok(());
            }
            // Every card was learned on this binary's protocol, so a daemon on
            // another one is not the card's, and was sent nothing past its
            // `Hello`: the close is done here, as above, and says so.
            Ok(Err(ClientError::ProtocolMismatch { daemon, local })) => {
                self.forget_card(pane_id);
                push_notice(
                    &self.close_warnings,
                    other_protocol_close_notice(daemon, local),
                );
                return Ok(());
            }
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
            self.forget_card(pane_id);
            return Ok(());
        };
        // PRD #1666 part 2 (G-fix4): stop the agent the listing named, on the
        // daemon process that listed it, never a successor's namesake.
        let stop_client = bound_to_known(&self.client, incarnation.as_ref());
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
                self.forget_card(pane_id);
                Ok(())
            }
            // Already gone between the list and the stop — the close is done.
            Ok(Err(e)) if is_agent_not_found(&e, &agent_id) => {
                self.forget_card(pane_id);
                Ok(())
            }
            // So is an agent whose daemon restarted between the two: it is no
            // longer reachable through this deck. Said, as an attached pane's
            // close says it.
            Ok(Err(e)) if is_identity_refusal(&e) => {
                self.forget_card(pane_id);
                self.notice_identity_refusal(&e);
                Ok(())
            }
            // Or one replaced between the two by a daemon on another protocol.
            Ok(Err(ClientError::ProtocolMismatch { daemon, local })) => {
                self.forget_card(pane_id);
                push_notice(
                    &self.close_warnings,
                    other_protocol_close_notice(daemon, local),
                );
                Ok(())
            }
            Ok(Err(e)) => Err(PaneError::CommandFailed(format!(
                "Failed to stop agent {agent_id} behind pane {pane_id}: {e}"
            ))),
            Err(_) => Err(PaneError::CommandFailed(format!(
                "stop-agent for {agent_id} (pane {pane_id}) timed out after {}s",
                CTRL_W_STOP_TIMEOUT.as_secs()
            ))),
        }
    }

    pub fn hydrate_pane(&self, card: &BoundPane) -> bool {
        let pane_id = card.pane_id.as_str();
        if self.panes.lock().unwrap().contains_key(pane_id) {
            return true;
        }

        let client = self.client.clone();
        let runtime = self.runtime.clone();

        // PRD #1666 part 2 (G-fix5, G-fix6): a card is hydrated only from the
        // daemon process it was attached on here, live or since detached, or
        // else the one the card was learned from. A successor's agent under the
        // same pane id is someone else's: the focus attaches nothing and says
        // so, and a card learned from no named process attaches nothing.
        let list_client = self.held_client(card);
        let (records, incarnation) = match runtime.block_on(async move {
            tokio::time::timeout(HYDRATE_LIST_TIMEOUT, list_client.list_agents_detailed()).await
        }) {
            Ok(Ok(listing)) => (listing.records, listing.incarnation),
            Ok(Err(e)) => {
                self.notice_identity_refusal(&e);
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
        // PRD #1666 part 2 (G-fix4): on the daemon process that listed it.
        let client_for_attach = bound_to_known(&client, incarnation.as_ref());
        // PRD #882: the viewport this attach declares. `record.rows/cols`
        // is the daemon's own current geometry (PRD #104 plumbed it through
        // `list_agents`), so declaring it registers this viewer without
        // moving the size. `(0, 0)` from a daemon predating #104 means "we
        // do not know" — declare nothing rather than guess, since a guessed
        // constraint under a smallest-wins policy would shrink the agent for
        // every other client too.
        let viewport_for_attach = match (record.rows, record.cols) {
            (0, _) | (_, 0) => None,
            (rows, cols) => Some((rows, cols)),
        };
        let conn = match runtime.block_on(async move {
            tokio::time::timeout(
                HYDRATE_ATTACH_TIMEOUT,
                // PRD #882: see the sibling hydration attach — the daemon's own
                // geometry, so this viewer joins the policy without changing it.
                client_for_attach.attach_as_viewer(&id_for_attach, viewport_for_attach),
            )
            .await
        }) {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                // PRD #1666 part 2 (G-fix6): a deck replaced between the
                // listing and the attach is said on the status line, as a
                // refused listing is.
                self.notice_identity_refusal(&e);
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
pub(crate) const CREATE_PANE_START_TIMEOUT: Duration = Duration::from_secs(5);

/// What an OpenCode start's reply may spend outside its `--version` probe
/// ([`crate::opencode_version::PROBE_TIMEOUT`]) and still arrive inside
/// [`CREATE_PANE_START_TIMEOUT`]: the `Hello`, the integration refresh and the
/// PTY spawn (PRD #2050, session/restore/014).
const START_REPLY_HEADROOM: Duration = Duration::from_secs(2);

// The daemon holds an OpenCode start's reply for the probe, so a probe that
// runs its whole bound must still leave the TUI time to hear the reply.
const _: () = assert!(
    crate::opencode_version::PROBE_TIMEOUT.as_millis() + START_REPLY_HEADROOM.as_millis()
        <= CREATE_PANE_START_TIMEOUT.as_millis()
);

/// PRD #1666 S5: how long a post of a deck-generated message may take. A post
/// writes nothing to a pane, so the daemon answers it at once.
const POST_MESSAGE_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// PRD #1666 part 2 (G-fix4): the daemon process the pane's agent ran
    /// under has been replaced, so nothing was sent. The agent is no longer
    /// reachable through this deck — which is not proof its process stopped —
    /// and the successor's agent with the same id is somebody else's.
    Restarted,
    /// PRD #1666 part 2 (PR #1817 review): the daemon answering speaks another
    /// attach protocol, so nothing past its `Hello` was sent. The pane's agent
    /// was attached on this binary's protocol, so this daemon replaced the one
    /// it ran under, and the close cannot be checked or retried against it.
    OtherProtocol { daemon: Option<u32>, local: u32 },
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
            // The close completes — the agent is not reachable through this
            // deck any more — and says why nothing was stopped here, in the
            // words the desktop's Stop uses.
            StopClass::Restarted => Some(StopOutcome::DoneUnverified(
                crate::daemon_client::AGENT_GONE_AFTER_RESTART.to_string(),
            )),
            // Completes too, rather than `Failed`: retaining the pane would
            // only fail the same way on every retry, leaving it unclosable.
            StopClass::OtherProtocol { daemon, local } => Some(StopOutcome::DoneUnverified(
                other_protocol_close_notice(daemon, local),
            )),
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
        Ok(Err(ClientError::DaemonRestarted)) => StopClass::Restarted,
        Ok(Err(ClientError::ProtocolMismatch { daemon, local })) => {
            StopClass::OtherProtocol { daemon, local }
        }
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
/// verifying — and reads directly after "Closed agent N but".
fn unverified_close_warning(pane_id_env: &str, reason: &str) -> String {
    format!(
        "Closed agent {pane_id_env} but {reason} — an agent may still be running unattended; \
         restart the deck to reattach it, or stop the daemon"
    )
}

/// PRD #1666 part 2 (PR #1817 review): the user-visible text for a close whose
/// daemon was replaced by one on another attach protocol (`daemon`, `None` when
/// its `Hello` named none; `local` is this binary's). The pane's agent ran
/// under a daemon on this binary's protocol, so the one answering now is not
/// it, and nothing but the `Hello` reached it: the pane closes here, and the
/// user is told the agent was neither checked nor stopped.
fn other_protocol_close_notice(daemon: Option<u32>, local: u32) -> String {
    let speaks = match daemon {
        Some(daemon) => format!("attach protocol v{daemon}, not this deck's v{local}"),
        None => format!("another attach protocol than this deck's v{local}"),
    };
    format!(
        "Closed this pane here only: the daemon now speaks {speaks}, so the agent it showed \
         could not be checked or stopped and may still be running"
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

/// Issue #1477: forget the application cursor mode (DECCKM) a replaced child
/// left in the pane's parser. The TUI encodes the arrows, Home and End from that
/// mode, and the parser outlives the child, so without this a replacement agent
/// would be sent the SS3 form its predecessor asked for. Called only where
/// [`reattach_replaces_the_child`] says the child changed, before the new
/// child's snapshot replays, for the reason that function gives: a same-PTY
/// reconnect keeps the mode, since its `ESC[?1h` is long past.
fn forget_replaced_childs_cursor_mode(parser: &Mutex<vt100::Parser>) {
    if let Ok(mut parser) = parser.lock() {
        parser.process(b"\x1b[?1l");
    }
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
    // What the pane is attached to, replaced on every re-attach so the resize
    // worker always names the viewer the daemon currently has (PRD #882) and
    // every request is bound to the daemon process that issued it (PRD #1666
    // part 2, G-fix4).
    attached: Arc<Mutex<PaneAttachment>>,
    mut input_rx: tokio::sync::mpsc::UnboundedReceiver<StreamCmd>,
    parser: Arc<Mutex<vt100::Parser>>,
    mouse_mode: Arc<AtomicBool>,
    hyperlinks: Arc<Mutex<HyperlinkMap>>,
    bytes_since_spawn: Arc<AtomicU64>,
    stream_rejections: Arc<Mutex<Vec<(String, String)>>>,
    clipboard_copy: Arc<Mutex<Option<String>>>,
    // PRD #1666 part 2 (G-fix5): the controller's status-line queue, for the
    // sentence a reattach that found the deck restarted leaves the user.
    notices: Arc<Mutex<Vec<String>>>,
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
                                // Issue #1705: the daemon sends its scrollback
                                // snapshot as the first STREAM_OUT of every
                                // attach (`daemon_protocol.rs`, "Replay the
                                // consistent scrollback snapshot"), so a copy in
                                // that frame is one the agent made earlier — on
                                // a reattach or a TUI restart it would overwrite
                                // the user's clipboard with stale text. Only
                                // copies in later frames are passed on. With an
                                // empty scrollback no snapshot frame is sent and
                                // the first frame is live output, so a copy in
                                // exactly that frame is the one lost.
                                let replayed = !bytes_received_this_session;
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
                                // Taken on every frame, replayed or not, so a
                                // replayed request never waits for a live one.
                                // Newest first: only the last valid copy in
                                // the frame would survive on the clipboard.
                                let requests = osc8.take_clipboard_requests();
                                if !replayed
                                    && let Some(text) = requests
                                        .iter()
                                        .rev()
                                        .find_map(|r| crate::osc52::osc52_copy_text(r))
                                {
                                    *clipboard_copy.lock().unwrap() = Some(text);
                                }
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
                            // PRD #882: the daemon applied a new geometry —
                            // because another client attached, detached or
                            // resized this agent. Reshape the parser so the
                            // bytes that follow are interpreted at the grid they
                            // were drawn for. NON-terminal, like STREAM_REJECT:
                            // the stream stays open and output keeps flowing.
                            //
                            // This is the arm that makes the policy correct for
                            // a client that is only WATCHING. Without it, a
                            // viewer that never asks for anything keeps parsing
                            // at its own geometry while the PTY sits at
                            // somebody else's — the PRD #104 mis-parse, arriving
                            // by the very mechanism meant to prevent it.
                            crate::daemon_protocol::KIND_GEOMETRY => {
                                if let Some((rows, cols)) =
                                    crate::daemon_protocol::parse_geometry_frame(&bytes)
                                {
                                    set_parser_size_if_changed(&parser, rows, cols);
                                }
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
        // PRD #882: the pane's own parser size is what it is drawing at right
        // now, so it is the honest viewport to re-declare for the fresh agent.
        let viewport = parser
            .lock()
            .ok()
            .map(|p| crate::vt_grid::size(p.screen()))
            .filter(|(rows, cols)| *rows > 0 && *cols > 0);
        // PRD #1666 part 2 (G-fix5): the lookup is bound to the daemon process
        // this pane's agent was attached on. A respawn stays inside that
        // process and is adopted, as is the same agent after a transient
        // disconnect (`lifecycle/handshake/011`). A restarted daemon numbers
        // its own agents and pane ids, so an agent it holds under this pane id
        // is not this pane's: the lookup gives up at once, the pane is marked
        // gone, and the input queued for the old agent is dropped with the
        // channel rather than sent to the new one.
        let lookup_client = attached.lock().unwrap().client(&client);
        match resolve_and_reattach(&lookup_client, &pane_id, viewport).await {
            Ok((new_agent_id, new_conn)) => {
                tracing::debug!(
                    pane_id = %pane_id,
                    new_agent_id = %new_agent_id,
                    "auto-reattach: subscribed to new agent for pane"
                );
                // The attach answered with the geometry now in force for the new
                // child. Adopt it, for the same reason the resize worker adopts
                // the answer rather than its request: the replayed snapshot that
                // is about to arrive was written at THIS geometry.
                if let Some((rows, cols)) = new_conn.applied() {
                    set_parser_size_if_changed(&parser, rows, cols);
                }
                {
                    let mut held = attached.lock().unwrap();
                    // PRD #611: the mouse flag describes a CHILD's requested
                    // reporting state, and this loop covers two situations the
                    // flag must be treated differently in. See
                    // `reattach_replaces_the_child`.
                    if reattach_replaces_the_child(&held.agent_id, &new_agent_id) {
                        mouse_mode.store(false, Ordering::Relaxed);
                        forget_replaced_childs_cursor_mode(&parser);
                    }
                    // PRD #882: the fresh attach minted a new viewer token (or
                    // none, against a daemon predating the policy). Published
                    // with the id, before the resize worker can fire again — a
                    // worker still holding the previous agent's token would fall
                    // through to the daemon's unattributed path and override
                    // every other client instead of updating this pane's
                    // constraint.
                    *held = PaneAttachment::of(new_agent_id, &new_conn);
                }
                conn_opt = Some(new_conn);
                io_state.store(IO_ATTACHED, Ordering::SeqCst);
            }
            // PRD #1666 part 2 (G-fix5): the daemon answering is not the
            // process this pane's agent ran under, or names none. Terminal,
            // and said on the status line in the desktop's words.
            Err(ReattachEnd::Refused(refusal)) => {
                tracing::warn!(
                    pane_id = %pane_id,
                    reason = "deck-restarted",
                    error = %refusal,
                    "auto-reattach: the daemon is not the process this pane's agent ran under; \
                     not adopting its agent, dropping queued input"
                );
                // G-fix7c: a restart is named by the one notice the render
                // loop shows when it drops this pane with the restart's other
                // agents, not by a sentence per pane.
                if !matches!(refusal, ClientError::DaemonRestarted) {
                    notice_identity_refusal(&notices, &refusal);
                }
                *lost.lock().unwrap() = Some(match refusal {
                    ClientError::DaemonRestarted => PaneLostReason::DeckRestarted,
                    _ => PaneLostReason::DeckUnidentified,
                });
                break 'outer;
            }
            Err(ReattachEnd::GaveUp(gave_up)) => {
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
                // A daemon on another attach protocol is the exception: the
                // agent may well be alive, and saying it is gone would send
                // the user after the wrong fault.
                *lost.lock().unwrap() = Some(if gave_up.protocol_mismatch.is_some() {
                    PaneLostReason::ProtocolMismatch
                } else {
                    PaneLostReason::AgentGone
                });
                break 'outer;
            }
        }
    }
}

/// How a [`resolve_and_reattach`] lookup ended without an attach.
enum ReattachEnd {
    /// It ran out its budget; the struct says why.
    GaveUp(ReattachGiveUp),
    /// PRD #1666 part 2 (G-fix5): a check on the lookup's bound handle refused
    /// the daemon answering — [`ClientError::DaemonRestarted`] or
    /// [`ClientError::DaemonUnnamed`] — so nothing was listed or attached and
    /// nothing will be: retrying would ask the same successor again.
    Refused(ClientError),
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
    /// PRD #1666 part 2: the last attempt found the daemon on another attach
    /// protocol (daemon's, this binary's), so it listed and attached nothing.
    /// Trailing, like the rest: cleared by an attempt whose check passed.
    protocol_mismatch: Option<(Option<u32>, u32)>,
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
        if self.protocol_mismatch.is_some() {
            "protocol-mismatch"
        } else if self.daemon_unreachable() {
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
        if self.protocol_mismatch.is_some() {
            "the daemon speaks another attach protocol"
        } else if self.daemon_unreachable() {
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
/// trapped). Returns [`ReattachEnd::GaveUp`] — carrying WHY — if no live
/// agent matches the pane within the budget, or if every `attach` attempt fails,
/// and [`ReattachEnd::Refused`] at once when `client` is bound to a daemon
/// process ([`DaemonClient::bound_to`]) and the daemon answering is not it.
async fn resolve_and_reattach(
    client: &DaemonClient,
    pane_id_env: &str,
    // PRD #882 — the geometry this pane is currently drawn at, declared as the
    // viewer's viewport on the fresh attach. A respawn drops the old agent and
    // with it every viewer constraint (they were keyed to attaches against an
    // agent that no longer exists), so re-declaring here is what restores this
    // pane's place in the policy rather than leaving the new child sized by
    // whoever happens to resize first.
    viewport: Option<(u16, u16)>,
) -> Result<(String, AttachConnection), ReattachEnd> {
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
    let mut protocol_mismatch;
    loop {
        attempts = attempts.saturating_add(1);
        // PRD #1666 part 2 (`lifecycle/handshake/011`): issue #405's protocol
        // check before every lookup — this pane's stream ended, and the daemon
        // now answering may be another build. Only a positive answer lets the
        // lookup run: one on another protocol is asked nothing more and
        // attached to never, and a check that could not be made at all — a
        // refused, garbled or cut-off `Hello`, or no daemon — is no answer, so
        // it is counted as a failed lookup and retried, never taken as leave
        // to attach. A daemon on this binary's protocol that answers within the
        // budget is still taken up. On the TUI's gated client
        // ([`DaemonClient::with_protocol_gate`]) the lookup and the attach
        // each repeat the check on their own connection, so a daemon replaced
        // after this one is refused too.
        if let Err(e) = client.check_protocol().await {
            // PRD #1666 part 2 (G-fix5): on a handle bound to the pane's
            // process, a successor — or a daemon naming none — is not this
            // pane's daemon, and asking it again will not make it one.
            if is_identity_refusal(&e) {
                return Err(ReattachEnd::Refused(e));
            }
            match e {
                crate::daemon_client::ClientError::ProtocolMismatch { daemon, local } => {
                    tracing::debug!(
                        daemon_protocol = ?daemon,
                        local_protocol = local,
                        "auto-reattach: the daemon speaks another attach protocol; not attaching"
                    );
                    protocol_mismatch = Some((daemon, local));
                    last_list_error = None;
                }
                e => {
                    protocol_mismatch = None;
                    list_errors = list_errors.saturating_add(1);
                    trailing_list_errors = trailing_list_errors.saturating_add(1);
                    last_list_error = Some(e.to_string());
                    tracing::debug!(
                        error = %e,
                        "auto-reattach: the protocol check failed; not attaching, retrying after backoff"
                    );
                }
            }
            if start.elapsed() >= REATTACH_LOOKUP_TOTAL_BUDGET {
                return Err(ReattachEnd::GaveUp(ReattachGiveUp {
                    attempts,
                    list_errors,
                    trailing_list_errors,
                    attach_errors,
                    trailing_attach_errors,
                    last_list_error,
                    last_attach_error,
                    protocol_mismatch,
                }));
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(REATTACH_LOOKUP_MAX_DELAY);
            continue;
        }
        protocol_mismatch = None;
        match client.list_agents_detailed().await {
            Ok(listing) => {
                let AgentListing {
                    records,
                    incarnation,
                    ..
                } = listing;
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
                    // PRD #1666 part 2 (G-fix4): attached on the daemon process
                    // that listed the id. One that restarted in between is
                    // sent nothing (`DaemonRestarted`, counted as a failed
                    // attach below), and the next round lists again — which,
                    // on the pane's bound lookup, refuses the successor.
                    Some(new_id) => match bound_to_known(client, incarnation.as_ref())
                        .attach_as_viewer(&new_id, viewport)
                        .await
                    {
                        Ok(conn) => return Ok((new_id, conn)),
                        Err(crate::daemon_client::ClientError::ProtocolMismatch {
                            daemon,
                            local,
                        }) => protocol_mismatch = Some((daemon, local)),
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
            // The gated lookup's own check found a daemon on another
            // protocol: one replaced since the check above.
            Err(crate::daemon_client::ClientError::ProtocolMismatch { daemon, local }) => {
                protocol_mismatch = Some((daemon, local));
                last_list_error = None;
            }
            // G-fix5: the gated lookup's own check found a successor, as
            // above.
            Err(e) if is_identity_refusal(&e) => return Err(ReattachEnd::Refused(e)),
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
            return Err(ReattachEnd::GaveUp(ReattachGiveUp {
                attempts,
                list_errors,
                trailing_list_errors,
                attach_errors,
                trailing_attach_errors,
                last_list_error,
                last_attach_error,
                protocol_mismatch,
            }));
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
    // PRD #1105 M11: the controller's own handle, so a resize connection is
    // made under the same client identity as the attach it resizes.
    client: DaemonClient,
    attached: Arc<Mutex<PaneAttachment>>,
    parser: Arc<Mutex<vt100::Parser>>,
) {
    // A receiver from `watch::channel` already counts the seed `None` as seen,
    // so the first `changed()` waits for an actual resize. Marking it seen here
    // instead also marked a resize sent before this task first ran, and that
    // one never reached the daemon.
    while rx.changed().await.is_ok() {
        let dims = *rx.borrow_and_update();
        let Some((rows, cols)) = dims else { continue };

        // Snapshot the current agent id under the std::sync mutex (brief,
        // not held across `.await`). PRD #92 F12: this can change between
        // resize ops when the io_task auto-renews the per-pane subscription
        // to a freshly-respawned agent; the next resize naturally targets
        // the new agent.
        //
        // PRD #882: and name which VIEWER is asking, so the request updates
        // this pane's constraint instead of overriding every other client's.
        // PRD #1666 part 2 (G-fix4): bound to the process that issued both, so
        // a restarted daemon that numbered another client's viewer the same is
        // sent nothing. One read, so the three always belong together.
        let held = attached.lock().unwrap().clone();
        let (id, token) = (held.agent_id.clone(), held.viewer.clone());
        let bound = held.client(&client);

        match tokio::time::timeout(
            RESIZE_DAEMON_TIMEOUT,
            bound.resize_agent_as_viewer(&id, rows, cols, token.as_deref()),
        )
        .await
        {
            // PRD #882: the daemon answers with what it ACTUALLY applied, which
            // differs from the request whenever another client decides the size
            // — a smaller pane under the fallback, or the last-focused client's
            // pane under PRD #1105, which can be larger. Size the parser from the
            // answer — sizing it from the request is what would leave this pane
            // parsing at a geometry the PTY does not have, which is PRD #104's
            // mis-parse arriving by a new route. A daemon predating the policy
            // echoes nothing (`None`) and has no policy to disagree with us, so
            // the request stands.
            Ok(Ok(applied)) => {
                let (rows, cols) = applied.unwrap_or((rows, cols));
                set_parser_size_if_changed(&parser, rows, cols);
            }
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

/// Cross-chunk state for [`scan_mouse_mode`] (PRD #611 M3). Lives beside the
/// stream's [`Osc8Filter`] and for the same reason: both hold the tail of a
/// sequence a PTY read boundary cut in half, and neither means anything to a
/// different stream.
///
/// The parser and the mouse model are shared with the daemon's output ring
/// (issue #1537), which tracks the same modes to restore them on replay — see
/// [`crate::terminal_modes`] for the semantics, which follow the repo's own
/// `vt100` 0.16.2.
#[derive(Debug, Default)]
struct MouseModeScanner {
    /// Finds private-mode directives, carrying a split sequence across chunks.
    scanner: PrivateModeScanner,
    /// The reporting protocol and encoding currently in force.
    modes: MouseModes,
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
    let MouseModeScanner { scanner, modes } = state;
    let mut touched = false;
    scanner.scan(data, |directive| match directive {
        ModeDirective::Mode { mode, set } => touched |= modes.apply(mode, set),
        // `vt100` rebuilds the screen on `ESC c`, mouse modes included.
        ModeDirective::FullReset => {
            *modes = MouseModes::default();
            touched = true;
        }
    });

    if touched {
        // The question this flag answers is not "did any mouse mode ever appear"
        // but "will the SGR report `forward_mouse_scroll` emits be understood by
        // this child?" — which needs a protocol in force AND SGR selected to
        // carry it. See the SGR paragraph on this function.
        flag.store(
            modes.protocol != MouseProtocol::None && modes.encoding == MouseEncoding::Sgr,
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

    /// Test-only: make the next [`feed_segment`] on this thread panic, so the
    /// containment path can be exercised now that vt100 0.18 no longer panics
    /// on the wide-character input that used to trigger it.
    #[cfg(test)]
    static PANIC_IN_NEXT_FEED: Cell<bool> = const { Cell::new(false) };
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
/// panics originating inside the `vt100` crate — the guard was written for
/// 0.16.2's `col_wrap` row underflow / out-of-bounds cell `unwrap()` (grid.rs)
/// on wide characters in a very short (e.g. 1-row) pane, which 0.18 fixed, and
/// stays for whatever the parser does next. Callers MUST keep any held
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
    #[cfg(test)]
    if PANIC_IN_NEXT_FEED.with(|flag| flag.replace(false)) {
        panic!("injected vt100 feed panic");
    }
    // Recomputed per segment: only a resize changes it, and no resize happens
    // within a single chunk, so this matches the previous once-per-chunk value.
    let max_row = crate::vt_grid::size(p.screen()).0.saturating_sub(1);
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
        let (rows, cols) = crate::vt_grid::size(p.screen());
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
    fn try_hydrate_pane(&self, card: &BoundPane) -> bool {
        self.hydrate_pane(card)
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
        self.pane_attachment(pane_id)
            .map(|attachment| attachment.agent_id)
    }

    fn forget_pane(&self, pane_id: &str) -> bool {
        // Dropping the pane drops its `StreamBackend`, which aborts the I/O
        // task and closes the socket — including a reattach loop the stop's
        // STREAM_END started, which would otherwise poll for a replacement
        // agent that is not coming.
        let removed = self.panes.lock().unwrap().remove(pane_id);
        removed.is_some()
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
            opts.remember_command,
            opts.authoring_kind,
        );
        result.map(|id| (id, resolved))
    }

    /// PRD #1666 part 2 (G-fix6): a card with a local pane closes it exactly as
    /// [`Self::close_pane`] does, bound to the process it was attached on; one
    /// without stops its agent only on the process the card was learned from.
    ///
    /// G-fix7: a local pane under the same pane id that was attached on another
    /// process is not the card's. A live one is someone else's agent, and the
    /// close is refused with [`crate::daemon_client::AGENT_GONE_AFTER_RESTART`];
    /// one that lost its agent is dropped, and the card's own agent is stopped
    /// on its own process.
    fn close_card(&self, card: &BoundPane) -> Result<(), PaneError> {
        match self.local_pane_for(card) {
            LocalPaneFor::Card => self.close_pane(&card.pane_id),
            LocalPaneFor::Other if !self.drop_replaced_local_pane(card) => Err(PaneError::Refused(
                crate::daemon_client::AGENT_GONE_AFTER_RESTART.to_string(),
            )),
            LocalPaneFor::Other | LocalPaneFor::None => self.stop_unattached_pane(card),
        }
    }

    /// PRD #1666 part 2 (G-fix7): focus the local pane behind `card` only when
    /// it is the card's. A live pane attached on another process is refused
    /// with [`crate::daemon_client::AGENT_GONE_AFTER_RESTART`]; one that lost
    /// its agent is dropped and reads as no local pane, so the caller's attach
    /// of the card's own agent takes its place.
    fn focus_card(&self, card: &BoundPane) -> Result<(), PaneError> {
        match self.local_pane_for(card) {
            LocalPaneFor::Card => self.focus_pane(&card.pane_id),
            LocalPaneFor::Other if !self.drop_replaced_local_pane(card) => Err(PaneError::Refused(
                crate::daemon_client::AGENT_GONE_AFTER_RESTART.to_string(),
            )),
            LocalPaneFor::Other | LocalPaneFor::None => self.focus_pane(&card.pane_id),
        }
    }

    fn local_pane_is_cards(&self, card: &BoundPane) -> bool {
        matches!(self.local_pane_for(card), LocalPaneFor::Card)
    }

    fn pane_incarnation(&self, pane_id: &str) -> Option<Incarnation> {
        self.pane_attachment(pane_id)
            .and_then(|attachment| attachment.incarnation)
    }

    fn panes_lost_to_restart(&self) -> Vec<(String, Option<Incarnation>)> {
        let panes = self.panes.lock().unwrap();
        panes
            .iter()
            .filter(|(_, pane)| {
                *pane.backend.lost.lock().unwrap() == Some(PaneLostReason::DeckRestarted)
            })
            .map(|(id, pane)| {
                let attached_on = pane.backend.attached.lock().unwrap().incarnation.clone();
                (id.clone(), attached_on)
            })
            .collect()
    }

    fn close_pane(&self, pane_id: &str) -> Result<(), PaneError> {
        // Hold the registry lock across the whole close so a stop-agent
        // failure can leave the pane in place for the user to retry. The
        // backend teardown (blocking child reap or async stop-agent) is
        // performed without the lock by detaching the pane first and only
        // re-inserting on the failure path. The guard is released before the
        // no-local-pane branch: `stop_unattached_pane` reads the registry for
        // the pane's binding (G-fix6).
        let removed = self.panes.lock().unwrap().remove(pane_id);
        let pane = {
            match removed {
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
                    //
                    // PRD #1666 part 2 (G-fix6): reached through `close_pane`,
                    // the pane id is the controller's own, with no card stamp.
                    // A card's close comes through `close_card` and carries
                    // the process the card was learned from.
                    return self.stop_unattached_pane(&BoundPane::held_by_controller(pane_id));
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
        // Snapshot the latest agent id under the shared mutex; PRD #92 F12
        // can swap this to the respawned id while the pane is alive, and
        // Ctrl+W must target the currently-bound agent, not the one we
        // first attached to. Keep the Arc around so the retry below can
        // re-read the id after a mid-reattach swap.
        //
        // PRD #1666 part 2 (G-fix4): every request below is bound to the
        // daemon process the pane is attached to. A respawn stays inside it;
        // a restart does not, and its successor's agent with the same id or
        // pane slot is not this pane's.
        let shared_attached = Arc::clone(&s.attached);
        let attachment = shared_attached.lock().unwrap().clone();
        let client = attachment.client(&DaemonClient::new(s.daemon_path.clone()));
        let initial_agent_id = attachment.agent_id;
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
            let retry_id = shared_attached.lock().unwrap().agent_id.clone();
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
                self.forget_card(pane_id);
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
                push_notice(&self.close_warnings, warning);
                self.forget_card(pane_id);
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
        let (attachment, cwd) = {
            let mut panes = self.panes.lock().unwrap();
            let pane = panes
                .get_mut(pane_id)
                .ok_or_else(|| PaneError::CommandFailed(format!("Pane {pane_id} not found")))?;
            // PRD #1666 part 2 (G-fix5): a pane whose reattach found the deck
            // restarted names an agent that is gone. The rename is refused
            // where the user can read it, and the card keeps its name.
            let gone = match *pane.backend.lost.lock().unwrap() {
                Some(PaneLostReason::DeckRestarted) => Some(ClientError::DaemonRestarted),
                Some(PaneLostReason::DeckUnidentified) => Some(ClientError::DaemonUnnamed),
                _ => None,
            };
            if let Some(refusal) = gone {
                self.notice_identity_refusal(&refusal);
                return Err(PaneError::CommandFailed(refusal.to_string()));
            }
            let previous = std::mem::replace(&mut pane.name, local_name.clone());
            self.rename_baselines
                .lock()
                .unwrap()
                .entry(pane_id.to_string())
                .or_insert(RenameBaseline {
                    name: previous,
                    unanswered: 0,
                })
                .unanswered += 1;
            // Snapshot the currently-bound agent (PRD #92 F12 can swap this
            // to a respawned id over the life of the pane).
            (
                pane.backend.attached.lock().unwrap().clone(),
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
        //
        // PRD #1666 part 2 (G-fix4): bound to the daemon process the pane is
        // attached to, so a restarted daemon's agent with the same id is not
        // given this label and this pane's cwd.
        let client = attachment.client(&self.client);
        let agent_id = attachment.agent_id;
        let agent_id_for_log = agent_id.clone();
        let daemon_label = new_label.clone();
        let notices = Arc::clone(&self.close_warnings);
        let refusals = Arc::clone(&self.rename_refusals);
        let baselines = Arc::clone(&self.rename_baselines);
        let panes = Arc::clone(&self.panes);
        let pane_id_for_revert = pane_id.to_string();
        self.runtime.spawn(async move {
            let result = client.set_agent_label(&agent_id, daemon_label, cwd).await;
            // G-fix5: a relabel refused because the deck restarted is said on
            // the status line, as the desktop says it; any other failure
            // resyncs quietly, as before. G-fix6: the refused name is also
            // taken back — here, and on the dashboard through
            // `rename_refusals` — as the desktop keeps the daemon's name when
            // it refuses. G-fix7: taken back to the name the deck last
            // confirmed, once no rename of the pane is left unanswered, so
            // overlapping renames never restore a refused one.
            let refused = match &result {
                Ok(()) => false,
                Err(e) => notice_identity_refusal(&notices, e),
            };
            if let Err(e) = &result {
                tracing::debug!(
                    agent_id = %agent_id_for_log,
                    error = %e,
                    "rename_pane: set_agent_label failed — local rename kept, daemon will resync on next reconnect"
                );
            }
            let mut baselines = baselines.lock().unwrap();
            let Some(baseline) = baselines.get_mut(&pane_id_for_revert) else {
                return;
            };
            baseline.unanswered = baseline.unanswered.saturating_sub(1);
            let last = baseline.unanswered == 0;
            // Any other failure keeps the local name, as before.
            if !refused {
                baseline.name = local_name.clone();
            }
            let restore = baseline.name.clone();
            if last {
                baselines.remove(&pane_id_for_revert);
            }
            drop(baselines);
            if refused
                && last
                && let Some(pane) = panes.lock().unwrap().get_mut(&pane_id_for_revert)
            {
                pane.name = restore;
            }
            refusals.lock().unwrap().push(RenameSettled {
                pane_id: pane_id_for_revert,
                label: (!refused).then_some(local_name),
                last,
            });
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

    fn post_message(
        &self,
        pane: &BoundPane,
        subject: &str,
        body: &str,
        origin: crate::inbox_hub::PostOrigin,
    ) -> Result<String, PaneError> {
        // PRD #1666 part 2 (G-fix4, G-fix6): addressed to the pane's agent on
        // the daemon process that agent runs under, and to nobody on a
        // successor. A pane with no local attachment — a detached card, or one
        // never attached here — is posted to on the process this controller
        // attached it on, or else the one the card was learned from. A local
        // pane attached on another process than the card's speaks for that
        // process's agent, so it names none (PR #1817 review 4347326632).
        let pane_id = pane.pane_id.as_str();
        let client = self.held_client(pane);
        let agent_id = match self.local_pane_for(pane) {
            LocalPaneFor::Card => self.pane_attachment(pane_id).map(|a| a.agent_id),
            LocalPaneFor::Other | LocalPaneFor::None => None,
        };
        let reply = self.runtime.block_on(async move {
            tokio::time::timeout(
                POST_MESSAGE_TIMEOUT,
                client.post_message(pane_id, agent_id.as_deref(), subject, body, None, origin),
            )
            .await
        });
        match reply {
            Ok(Ok(GatedQuery::Answered(id))) => Ok(id),
            Ok(Ok(GatedQuery::Unsupported)) => Err(PaneError::CommandFailed(
                "the daemon has no agent inbox; upgrade it to deliver this message".to_string(),
            )),
            Ok(Err(e)) => Err(PaneError::CommandFailed(format!("post-message: {e}"))),
            Err(_) => Err(PaneError::CommandFailed(format!(
                "post-message timed out after {}ms",
                POST_MESSAGE_TIMEOUT.as_millis()
            ))),
        }
    }

    fn report_orchestrator_context(&self, pane_id: &str, context_path: &std::path::Path) {
        // PRD #1666 part 2 (G-fix4, G-fix6): recorded for the pane's agent on
        // the daemon process it runs under. A role pane is one this controller
        // attached; one it holds no record for is reported to nobody on a
        // gated controller.
        let client = self.held_client(&BoundPane::held_by_controller(pane_id));
        let pane_id = pane_id.to_string();
        let context_path = context_path.to_path_buf();
        self.runtime.spawn(async move {
            match client
                .record_orchestrator_context(&pane_id, &context_path)
                .await
            {
                Ok(GatedQuery::Answered(())) => {}
                Ok(GatedQuery::Unsupported) => tracing::debug!(
                    pane_id,
                    "the daemon does not record re-arm publications; it keeps the context file \
                     it recorded at the start"
                ),
                // The daemon logs its own refusals, at the level each deserves:
                // most are routine (an orchestration opened with `Ctrl+n` has no
                // recorded file to follow).
                Err(e @ crate::daemon_client::ClientError::Server(_)) => tracing::debug!(
                    pane_id,
                    path = %context_path.display(),
                    error = %e,
                    "the daemon did not record the orchestrator's re-armed context file"
                ),
                Err(e) => tracing::warn!(
                    pane_id,
                    path = %context_path.display(),
                    error = %e,
                    "could not report the orchestrator's re-armed context file to the daemon"
                ),
            }
        });
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
    use spec::spec;

    /// PR #1735 review (4301453654): two presses whose answers come back out
    /// of order leave the later press's answer, and the earlier one's is
    /// dropped.
    #[test]
    fn an_earlier_presss_late_answer_is_dropped() {
        let answer = |message: &str| super::InboxActionAnswer {
            pane_id: "p".to_string(),
            agent_id: "a".to_string(),
            retry: true,
            outcome: Ok(message.to_string()),
        };
        let mut slot = super::InboxActionSlot::default();
        let first = slot.begin();
        let second = slot.begin();
        slot.land(second, answer("second"));
        slot.land(first, answer("first"));
        assert_eq!(slot.answer.take(), Some(answer("second")));
        assert_eq!(slot.answer, None);
    }

    use super::*;

    /// The whole point of `PaneLostReason` is that the user reads it, so guard
    /// the strings against regressing back into internal vocabulary. The old
    /// message was `Pane <id> stream I/O task ended` — it named a task the user
    /// has no concept of and said nothing about what to do.
    #[test]
    fn pane_lost_messages_are_user_facing_and_actionable() {
        for reason in [
            PaneLostReason::AgentKeptCrashing,
            PaneLostReason::AgentGone,
            PaneLostReason::DeckRestarted,
            PaneLostReason::DeckUnidentified,
        ] {
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
    /// PRD #1666 part 2 (G-fix5): a pane whose deck restarted says so in the
    /// words the desktop uses for the same refusal (CLAUDE.md rule 22).
    #[test]
    fn a_pane_whose_deck_restarted_says_what_the_desktop_says() {
        assert!(
            PaneLostReason::DeckRestarted
                .user_message()
                .starts_with(crate::daemon_client::AGENT_GONE_AFTER_RESTART)
        );
    }

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
            protocol_mismatch: None,
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
            protocol_mismatch: None,
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
            protocol_mismatch: None,
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
            protocol_mismatch: None,
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
            protocol_mismatch: None,
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
            protocol_mismatch: None,
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
            protocol_mismatch: None,
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
            protocol_mismatch: None,
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
            protocol_mismatch: None,
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
            protocol_mismatch: None,
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
            protocol_mismatch: None,
        };
        assert!(
            !never_asked.daemon_unreachable(),
            "0 attempts / 0 errors must not read as 'every lookup failed'"
        );
    }

    /// Regression: `vt100` 0.16.2 panicked (`col_wrap` row-underflow / an
    /// out-of-bounds cell `unwrap()` in grid.rs) when a wide character wrapped
    /// in a 1-row pane — the geometry a pane collapses to when many are stacked
    /// into one column (the crash observed over `connect`). vt100 0.18 no
    /// longer panics on that input, so the panic is now injected
    /// ([`PANIC_IN_NEXT_FEED`]) and the wide-character chunk is checked
    /// separately to still feed cleanly. A panicking chunk must be contained by
    /// `process_agent_output_chunk` so a single malformed chunk from any agent
    /// can never crash the whole TUI: the call returns normally, the guard flag
    /// clears, the parser mutex is not poisoned, and a later chunk still
    /// processes.
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

        // The crate itself now handles this input; a panic here would be a
        // vt100 regression, which the guarded path below still contains.
        let unguarded = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let mut p = crate::vt_grid::parser(1, 10, 1000);
            p.process(crasher);
        }));
        assert!(
            unguarded.is_ok(),
            "vt100 0.18 feeds a wide character wrapping in a 1-row grid without panicking"
        );

        let parser = Mutex::new(crate::vt_grid::parser(1, 10, 1000));
        let mut osc8 = Osc8Filter::new();
        let mouse = AtomicBool::new(false);
        let mut mouse_scan = MouseModeScanner::default();
        let links = Mutex::new(HyperlinkMap::new());
        let bytes_since_spawn = AtomicU64::new(0);

        // The real path must NOT panic on the crashing chunk.
        PANIC_IN_NEXT_FEED.with(|flag| flag.set(true));
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

        assert!(
            !PANIC_IN_NEXT_FEED.with(Cell::get),
            "the injected panic must have fired inside the guarded feed"
        );

        // A subsequent chunk still flows through the same parser.
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

    /// Issue #1477: a replacement child starts in the ordinary cursor mode,
    /// whatever its predecessor asked for, and keeps what is on screen.
    #[test]
    fn a_replaced_childs_application_cursor_mode_is_forgotten() {
        let parser = Mutex::new(crate::vt_grid::parser(4, 20, 0));
        parser.lock().unwrap().process(b"old child\x1b[?1h");
        assert!(parser.lock().unwrap().screen().application_cursor());

        forget_replaced_childs_cursor_mode(&parser);

        let parser = parser.lock().unwrap();
        assert!(!parser.screen().application_cursor());
        assert!(parser.screen().contents().contains("old child"));
    }

    /// Scenario: Ten numbered lines fill a ten-row pane, which is then made
    /// five rows tall and back to ten. The shorter pane shows the newest five
    /// with the cursor on its last row, and the taller one shows all ten again.
    ///
    /// vt100 0.18 moves the oldest rows into scrollback on a shrink; 0.16 cut
    /// rows off the bottom, hiding the latest output and the cursor's row.
    #[spec("resize/layout/003")]
    #[test]
    fn resize_layout_003_a_shorter_pane_keeps_its_latest_output_and_a_taller_one_restores_it() {
        let parser = Mutex::new(new_pane_parser(10, 20));
        let lines: Vec<String> = (1..=10).map(|n| format!("LINE_{n:02}")).collect();
        parser
            .lock()
            .unwrap()
            .process(lines.join("\r\n").as_bytes());

        set_parser_size_if_changed(&parser, 5, 20);
        {
            let p = parser.lock().unwrap();
            let visible: Vec<String> = p.screen().rows(0, 20).collect();
            assert_eq!(visible, lines[5..], "the newest five lines stay visible");
            assert_eq!(p.screen().cursor_position(), (4, 7));
        }

        set_parser_size_if_changed(&parser, 10, 20);
        let p = parser.lock().unwrap();
        let visible: Vec<String> = p.screen().rows(0, 20).collect();
        assert_eq!(
            visible, lines,
            "growing back pulls the rows out of scrollback"
        );
        assert_eq!(p.screen().cursor_position(), (9, 7));
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
            let size = crate::vt_grid::size(screen.lock().unwrap().screen());
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
        let size = crate::vt_grid::size(screen.lock().unwrap().screen());
        assert_eq!(size, (21, 38), "usable dims must pass through unchanged");
    }

    /// The shared constructor is the whole point of the fix: one definition of
    /// "valid parser geometry", applied in one place. Pin both halves.
    #[test]
    fn new_pane_parser_clamps_and_passes_through() {
        assert_eq!(
            crate::vt_grid::size(new_pane_parser(0, 0).screen()),
            (24, 80)
        );
        assert_eq!(
            crate::vt_grid::size(new_pane_parser(0, 80).screen()),
            (24, 80)
        );
        assert_eq!(
            crate::vt_grid::size(new_pane_parser(24, 0).screen()),
            (24, 80)
        );
        assert_eq!(
            crate::vt_grid::size(new_pane_parser(u16::MAX, u16::MAX).screen()),
            (24, 80)
        );
        assert_eq!(
            crate::vt_grid::size(new_pane_parser(21, 38).screen()),
            (21, 38)
        );
        assert_eq!(crate::vt_grid::size(new_pane_parser(1, 1).screen()), (1, 1));
    }

    /// The failure mode issue #363 warns about is a *fourth* call site added
    /// later that quietly skips the guard — the same drift that produced this
    /// bug, and the same class as any rule kept in two hand-maintained copies.
    ///
    /// So pin the structural invariant rather than only the behaviour: in this
    /// file's non-test code there is exactly ONE parser construction (a
    /// `vt100::Parser::new`, or the `vt_grid::parser` that wraps it), and it is
    /// inside `new_pane_parser`. `include_str!` is compile-time, so this reads
    /// no file at runtime and stays inside the fast tier's budget.
    #[test]
    fn parser_is_constructed_in_exactly_one_place() {
        const SRC: &str = include_str!("embedded_pane.rs");
        // Split off this very test module; parsers built inside it are fixtures
        // (`wide_char_in_one_row_pane_does_not_crash_the_tui` deliberately
        // constructs a raw 1x10 parser to prove the vt100 bug still exists).
        // `include_str!` yields the file exactly as checked out, and a CRLF
        // checkout — Windows' default before the root `.gitattributes` pinned
        // LF, and still what a clone made before that has — makes every
        // `\n`-anchored marker below (this split, and the `\n}\n` that ends the
        // helper) silently fail to match. Normalize once so the guard reads the
        // same source on every platform.
        let src = SRC.replace("\r\n", "\n");
        let (prod, _tests) = src
            .split_once("\n#[cfg(test)]\nmod tests {")
            .expect("test module marker moved; update this guard");

        // Both spellings construct a parser: the raw vt100 call, and the
        // `vt_grid` wrapper that takes the deck's `u16` geometry.
        let mut hits: Vec<usize> = ["vt100::Parser::new(", "vt_grid::parser("]
            .iter()
            .flat_map(|needle| prod.match_indices(needle).map(|(i, _)| i))
            .collect();
        hits.sort_unstable();
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
            "the one parser construction must live inside `new_pane_parser`"
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
        assert_eq!(state.modes.protocol, MouseProtocol::ButtonMotion);

        let (flag, mut state) = mouse_scanner();
        assert!(!feed_mouse(b"\x1b[?1000h", &flag, &mut state));
        assert!(!feed_mouse(b"\x1b[?1003h", &flag, &mut state));
        assert_eq!(
            state.modes.protocol,
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
            state.modes.protocol,
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
        assert_eq!(state.modes.protocol, MouseProtocol::None);
        assert_eq!(state.modes.encoding, MouseEncoding::Default);
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
        assert_eq!(state.modes.protocol, MouseProtocol::None);
        assert_eq!(state.modes.encoding, MouseEncoding::Default);
    }

    /// Neighbouring mode numbers must not be matched by digit adjacency, and
    /// the private modes an app sets for other reasons must not count either.
    #[test]
    fn scan_mouse_mode_matches_whole_parameters_not_digit_substrings() {
        let (flag, mut state) = mouse_scanner();
        assert!(!feed_mouse(b"\x1b[?11000h", &flag, &mut state));
        assert!(!feed_mouse(b"\x1b[?10006h", &flag, &mut state));
        assert!(!feed_mouse(b"\x1b[?1049h\x1b[?2004h", &flag, &mut state));
        assert_eq!(state.modes.protocol, MouseProtocol::None);
        assert_eq!(state.modes.encoding, MouseEncoding::Default);
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
            let mut parser = crate::vt_grid::parser(24, 80, 100);
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
            state.scanner.carry(),
            b"\x1b[?1000".as_slice(),
            "only the open sequence is carried"
        );

        // Ordinary output after it resolves nothing and clears the carry: the
        // partial turned out not to be a private-mode sequence at all.
        assert!(feed_mouse(b"; hello world\r\n", &flag, &mut state));
        assert!(
            state.scanner.carry().is_empty(),
            "nothing left that could still open"
        );
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
                state.scanner.carry().len() <= crate::terminal_modes::CARRY_MAX,
                "carry grew to {} after {fed} bytes of open parameter list",
                state.scanner.carry().len()
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
                state.scanner.carry().len() <= crate::terminal_modes::CARRY_MAX,
                "carry grew to {}",
                state.scanner.carry().len()
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
        overlong.extend_from_slice(&b"1;".repeat(crate::terminal_modes::CARRY_MAX));
        assert!(!feed_mouse(&overlong, &flag, &mut state));
        assert!(
            state.scanner.carry().is_empty(),
            "an over-cap partial is dropped"
        );
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

    /// Issue #1537 review: `ESC c` (RIS) makes `vt100` rebuild the screen, mouse
    /// modes included, so the flag follows it — split across chunks too.
    #[test]
    fn scan_mouse_mode_turns_reporting_off_on_a_full_reset() {
        let (flag, mut state) = mouse_scanner();
        assert!(feed_mouse(b"\x1b[?1000;1006h", &flag, &mut state));
        assert!(feed_mouse(b"output \x1b", &flag, &mut state));
        assert!(
            !feed_mouse(b"c", &flag, &mut state),
            "a full reset must withdraw mouse reporting"
        );
        assert_eq!(state.modes, MouseModes::default());
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
        let parser = Mutex::new(crate::vt_grid::parser(24, 80, 100));
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

    /// PRD #1105 M11 step 3 — the TUI identifies itself on every attach and
    /// claims focus from terminal events. The classification and the throttle's
    /// arithmetic are unit-tested in `crate::focus_report`; these drive the
    /// controller against a socket, so what they assert is what reaches the
    /// daemon.
    #[cfg(unix)]
    mod focus_claims {
        use super::*;
        use crate::agent_pty::{AgentPtyRegistry, SpawnOptions};
        use crate::daemon_protocol::{
            AttachResponse, CAP_FOCUS_GAINED, CAP_LIST_PROJECTS, KIND_REQ, PROTOCOL_VERSION,
            bind_attach_listener, read_frame, serve_attach, write_resp,
        };
        use crate::focus_report::INPUT_CLAIM_INTERVAL;
        use crossterm::event::{
            Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
        };

        /// `bind_attach_listener` flips the process umask around its `bind(2)`;
        /// hold this across tempdir + bind, as `daemon_client`'s tests do.
        static BIND_LOCK: Mutex<()> = Mutex::new(());

        pub(super) fn runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("build the test runtime")
        }

        pub(super) fn bind(
            runtime: &tokio::runtime::Runtime,
            tag: &str,
        ) -> (
            tempfile::TempDir,
            PathBuf,
            crate::platform::ipc::IpcListener,
        ) {
            let _guard = BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            // The listener registers with the reactor as it is built.
            let _runtime = runtime.enter();
            let dir = crate::test_temp::tempdir().expect("scratch dir for the socket");
            let path = dir.path().join(format!("{tag}.sock"));
            let listener = bind_attach_listener(&path).expect("bind the attach socket");
            (dir, path, listener)
        }

        /// The production attach server over a real registry.
        fn real_daemon(
            runtime: &tokio::runtime::Runtime,
        ) -> (tempfile::TempDir, PathBuf, Arc<AgentPtyRegistry>) {
            crate::test_isolation::detach_from_any_live_deck();
            let (dir, path, listener) = bind(runtime, "focus-real");
            let registry = Arc::new(AgentPtyRegistry::new());
            let served = Arc::clone(&registry);
            runtime.spawn(async move {
                let (events, _) = tokio::sync::broadcast::channel(16);
                let _ = serve_attach(listener, served, events).await;
            });
            (dir, path, registry)
        }

        /// A daemon that answers `hello` with `advertised` as its capability
        /// list and records every other request it is sent: `focus-gained`
        /// claims by the `client_id` they name, anything else by its op.
        ///
        /// It serves one connection at a time, and it can hold its `hello`
        /// replies until [`Self::release_hello`], which keeps a claim waiting on
        /// its capability handshake — unsent — for as long as a test needs.
        struct ScriptedDaemon {
            _dir: tempfile::TempDir,
            path: PathBuf,
            claims: Arc<Mutex<Vec<String>>>,
            other_ops: Arc<Mutex<Vec<String>>>,
            hellos: Arc<std::sync::atomic::AtomicUsize>,
            hello_gate: tokio::sync::watch::Sender<bool>,
            server: tokio::task::JoinHandle<()>,
        }

        impl ScriptedDaemon {
            fn start(
                runtime: &tokio::runtime::Runtime,
                advertised: Option<Vec<&'static str>>,
            ) -> Self {
                Self::start_gated(runtime, advertised, true)
            }

            /// [`Self::start`], with every `hello` reply held until
            /// [`Self::release_hello`].
            fn start_holding_hello(
                runtime: &tokio::runtime::Runtime,
                advertised: Option<Vec<&'static str>>,
            ) -> Self {
                Self::start_gated(runtime, advertised, false)
            }

            fn start_gated(
                runtime: &tokio::runtime::Runtime,
                advertised: Option<Vec<&'static str>>,
                hello_open: bool,
            ) -> Self {
                let (dir, path, listener) = bind(runtime, "focus-scripted");
                let claims = Arc::new(Mutex::new(Vec::new()));
                let other_ops = Arc::new(Mutex::new(Vec::new()));
                let hellos = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let hello_gate = tokio::sync::watch::Sender::new(hello_open);
                let (server_claims, server_other) = (Arc::clone(&claims), Arc::clone(&other_ops));
                let server_hellos = Arc::clone(&hellos);
                let mut gate = hello_gate.subscribe();
                let server = runtime.spawn(async move {
                    while let Ok(mut stream) = listener.accept().await {
                        let Ok(Some((KIND_REQ, payload))) = read_frame(&mut stream).await else {
                            continue;
                        };
                        let request: serde_json::Value =
                            serde_json::from_slice(&payload).expect("decode the request");
                        let op = request["op"].as_str().unwrap_or_default().to_string();
                        if op == "hello" {
                            server_hellos.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let _ = gate.wait_for(|open| *open).await;
                        }
                        let response = match op.as_str() {
                            "hello" => AttachResponse {
                                capabilities: advertised
                                    .clone()
                                    .map(|caps| caps.into_iter().map(String::from).collect()),
                                ..AttachResponse::hello(PROTOCOL_VERSION)
                            },
                            "focus-gained" => {
                                server_claims.lock().unwrap().push(
                                    request["client_id"]
                                        .as_str()
                                        .unwrap_or_default()
                                        .to_string(),
                                );
                                AttachResponse {
                                    ok: true,
                                    ..Default::default()
                                }
                            }
                            _ => {
                                server_other.lock().unwrap().push(op.clone());
                                AttachResponse::err(format!(
                                    "malformed request: unknown variant `{op}`"
                                ))
                            }
                        };
                        let _ = write_resp(&mut stream, &response).await;
                    }
                });
                Self {
                    _dir: dir,
                    path,
                    claims,
                    other_ops,
                    hellos,
                    hello_gate,
                    server,
                }
            }

            fn claims(&self) -> Vec<String> {
                self.claims.lock().unwrap().clone()
            }

            /// How many `hello` requests have arrived, answered or held.
            fn hellos(&self) -> usize {
                self.hellos.load(std::sync::atomic::Ordering::SeqCst)
            }

            fn release_hello(&self) {
                self.hello_gate.send_replace(true);
            }
        }

        impl Drop for ScriptedDaemon {
            fn drop(&mut self) {
                self.server.abort();
            }
        }

        fn key(c: char) -> Event {
            Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
        }

        fn mouse(kind: MouseEventKind) -> Event {
            Event::Mouse(MouseEvent {
                kind,
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            })
        }

        /// Wait for every claim the reporter started, so a count read afterwards
        /// is final rather than a race against spawned tasks.
        fn finish(runtime: &tokio::runtime::Runtime, started: Vec<tokio::task::JoinHandle<()>>) {
            runtime.block_on(async {
                for claim in started {
                    claim.await.expect("the claim task must not panic");
                }
            });
        }

        /// Scenario: a TUI controller is attached to a real daemon; the terminal
        /// reports focus-in, so the daemon records that TUI as the last-focused
        /// client. Another client then claims focus, and a second focus-in —
        /// well inside the input throttle's window — takes it straight back.
        #[test]
        fn a_terminal_focus_in_claims_focus_as_the_controllers_client() {
            let runtime = runtime();
            let (_dir, path, registry) = real_daemon(&runtime);
            let controller = EmbeddedPaneController::new(path.clone(), runtime.handle().clone());
            let tui = controller
                .client_id()
                .expect("a TUI controller always has an identity")
                .to_string();
            assert_eq!(
                registry.focused_client(),
                None,
                "fixture: nobody has claimed focus"
            );

            let t0 = Instant::now();
            let claim = controller
                .report_focus(&Event::FocusGained, t0)
                .expect("focus-in starts a claim");
            finish(&runtime, vec![claim]);
            assert_eq!(registry.focused_client().as_deref(), Some(tui.as_str()));

            let desktop = DaemonClient::new(path).with_client_id(generate_client_id());
            runtime
                .block_on(desktop.focus_gained())
                .expect("the other client's claim");
            assert_eq!(registry.focused_client().as_deref(), desktop.client_id());

            let claim = controller
                .report_focus(&Event::FocusGained, t0 + Duration::from_millis(10))
                .expect("focus-in is never throttled, however recent the last claim");
            finish(&runtime, vec![claim]);
            assert_eq!(
                registry.focused_client().as_deref(),
                Some(tui.as_str()),
                "switching back to the TUI must take focus back at once"
            );
            registry.shutdown_all();
        }

        /// Scenario: a burst of keys, a paste and a wheel spin inside one
        /// throttle window reaches the daemon as exactly one `focus-gained`
        /// claim; pointer motion claims nothing; the first key at the window's
        /// end claims again, and a focus-in right after that claims regardless.
        #[test]
        fn input_reaches_the_daemon_as_one_claim_per_window() {
            let runtime = runtime();
            let daemon = ScriptedDaemon::start(&runtime, Some(vec![CAP_FOCUS_GAINED]));
            let controller =
                EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone());
            let tui = controller.client_id().expect("identity").to_string();

            let t0 = Instant::now();
            let mut burst: Vec<Event> = "hello world".chars().map(key).collect();
            burst.push(Event::Paste("pasted".into()));
            burst.extend((0..20).map(|_| mouse(MouseEventKind::ScrollDown)));
            let step = INPUT_CLAIM_INTERVAL / (burst.len() as u32 + 1);
            let mut started = Vec::new();
            for (index, event) in burst.iter().enumerate() {
                started.extend(controller.report_focus(event, t0 + step * index as u32));
            }
            finish(&runtime, started);
            assert_eq!(
                daemon.claims(),
                vec![tui.clone()],
                "{} input events inside one window must reach the daemon as ONE claim",
                burst.len()
            );

            let later = t0 + INPUT_CLAIM_INTERVAL * 3;
            assert!(
                controller
                    .report_focus(&mouse(MouseEventKind::Moved), later)
                    .is_none(),
                "bare pointer motion is not input, even with the window long closed"
            );
            // Each claim is finished before the next event, because a newer
            // claim drops one still unsent — that is
            // `a_claim_still_unsent_when_a_newer_event_arrives_is_never_sent`'s
            // subject, and this test is about what the throttle admits.
            let started: Vec<_> = controller
                .report_focus(&key('x'), t0 + INPUT_CLAIM_INTERVAL)
                .into_iter()
                .collect();
            assert_eq!(started.len(), 1, "the window's end admits the next input");
            finish(&runtime, started);
            let started: Vec<_> = controller
                .report_focus(
                    &Event::FocusGained,
                    t0 + INPUT_CLAIM_INTERVAL + Duration::from_millis(1),
                )
                .into_iter()
                .collect();
            finish(&runtime, started);
            assert_eq!(
                daemon.claims(),
                vec![tui.clone(), tui.clone(), tui],
                "the window's end admits the next input, and focus-in is never throttled"
            );
            assert!(daemon.other_ops.lock().unwrap().is_empty());
        }

        /// Scenario: a focus-in starts a claim, and the daemon holds the claim's
        /// capability handshake so it cannot be sent yet. The terminal then
        /// reports focus-out, the handshake is released, and the daemon never
        /// receives the claim. Then, against a fresh daemon, a second focus-in
        /// arrives while the first claim is held the same way: after the release
        /// the daemon receives exactly one claim, not the stale one as well.
        #[test]
        fn a_claim_still_unsent_when_a_newer_event_arrives_is_never_sent() {
            let runtime = runtime();
            let t0 = Instant::now();

            let daemon =
                ScriptedDaemon::start_holding_hello(&runtime, Some(vec![CAP_FOCUS_GAINED]));
            let controller =
                EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone());
            let claim = controller
                .report_focus(&Event::FocusGained, t0)
                .expect("focus-in starts a claim");
            wait_until("the claim waits on its handshake", || daemon.hellos() == 1);
            assert!(
                controller
                    .report_focus(&Event::FocusLost, t0 + Duration::from_millis(10))
                    .is_none(),
                "focus-out starts no claim of its own"
            );
            daemon.release_hello();
            finish(&runtime, vec![claim]);
            assert!(
                daemon.claims().is_empty(),
                "a claim still unsent when the terminal reported focus-out must be dropped, \
                 not sent late: {:?}",
                daemon.claims()
            );

            let daemon =
                ScriptedDaemon::start_holding_hello(&runtime, Some(vec![CAP_FOCUS_GAINED]));
            let controller =
                EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone());
            let tui = controller.client_id().expect("identity").to_string();
            let stale = controller
                .report_focus(&Event::FocusGained, t0)
                .expect("focus-in starts a claim");
            wait_until("the first claim waits on its handshake", || {
                daemon.hellos() == 1
            });
            let newer = controller
                .report_focus(&Event::FocusGained, t0 + Duration::from_millis(10))
                .expect("a second focus-in starts a claim");
            daemon.release_hello();
            finish(&runtime, vec![stale, newer]);
            assert_eq!(
                daemon.claims(),
                vec![tui],
                "the newer claim is sent and the one it superseded is not"
            );
            assert!(daemon.other_ops.lock().unwrap().is_empty());
        }

        /// Scenario: the TUI runs against daemons that predate focus-driven
        /// sizing — one advertising no capabilities at all, one advertising only
        /// PRD #819's verbs (every release through v0.40.2). Focus-in and input
        /// both happen, and neither daemon is sent a `focus-gained` request.
        #[test]
        fn nothing_is_claimed_against_a_daemon_that_does_not_advertise_focus_gained() {
            let runtime = runtime();
            for advertised in [None, Some(vec![CAP_LIST_PROJECTS])] {
                let daemon = ScriptedDaemon::start(&runtime, advertised.clone());
                let controller =
                    EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone());
                let t0 = Instant::now();
                let started: Vec<_> = [
                    (Event::FocusGained, t0),
                    (key('a'), t0 + INPUT_CLAIM_INTERVAL),
                    (Event::FocusGained, t0 + INPUT_CLAIM_INTERVAL * 2),
                ]
                .iter()
                .filter_map(|(event, at)| controller.report_focus(event, *at))
                .collect();
                assert_eq!(started.len(), 3, "fixture: every event passed the throttle");
                finish(&runtime, started);
                assert!(
                    daemon.claims().is_empty(),
                    "advertised {advertised:?}: no `focus-gained` may reach this daemon"
                );
                assert!(
                    daemon.other_ops.lock().unwrap().is_empty(),
                    "advertised {advertised:?}: nothing but the handshake may be sent"
                );
            }
        }

        /// Every viewer of every agent, with the client each belongs to.
        fn all_viewer_clients(registry: &AgentPtyRegistry) -> Vec<(String, Option<String>)> {
            registry
                .agent_records()
                .into_iter()
                .flat_map(|record| {
                    registry
                        .viewers_of(&record.id)
                        .into_values()
                        .map(move |viewer| (record.id.clone(), viewer.client_id))
                })
                .collect()
        }

        fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !done() {
                assert!(Instant::now() < deadline, "timed out waiting until {what}");
                std::thread::sleep(Duration::from_millis(25));
            }
        }

        /// Scenario: one TUI controller reaches agents by every attach path it
        /// has — startup hydration, hydrating a single pane, opening a new pane,
        /// and the automatic re-attach after an agent is replaced under its
        /// pane — and a second controller hydrates the same daemon. Every viewer
        /// the first controller registered carries its one client id, and the
        /// second controller's carry a different one.
        #[test]
        fn every_attach_path_carries_the_controllers_one_client_id() {
            let runtime = runtime();
            let (_dir, path, registry) = real_daemon(&runtime);
            let spawn = |pane: &str| {
                registry
                    .spawn_agent(SpawnOptions {
                        command: Some("/bin/cat"),
                        env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
                        ..SpawnOptions::default()
                    })
                    .expect("spawn a PTY agent")
            };
            let at_startup = spawn("focus-startup");
            let controller = EmbeddedPaneController::new(path.clone(), runtime.handle().clone());
            let tui = controller.client_id().expect("identity").to_string();

            assert_eq!(
                controller.hydrate_from_daemon().len(),
                1,
                "hydration attach"
            );
            let late = spawn("focus-late");
            assert!(
                controller.hydrate_pane(&BoundPane::held_by_controller("focus-late")),
                "single-pane hydration attach"
            );
            controller
                .create_pane(Some("/bin/cat"), None)
                .expect("new-pane attach");
            let attached = all_viewer_clients(&registry);
            assert_eq!(
                attached.len(),
                3,
                "one viewer per attach so far: {attached:?}"
            );

            // The re-attach: replace the startup agent under the same pane id,
            // and the pane's I/O task finds and attaches to the replacement.
            registry
                .close_agent(&at_startup)
                .expect("close the startup agent");
            let replacement = spawn("focus-startup");
            wait_until("the pane re-attached to the replacement agent", || {
                !registry.viewers_of(&replacement).is_empty()
            });

            let viewers = all_viewer_clients(&registry);
            assert_eq!(viewers.len(), 3, "{viewers:?}");
            assert!(
                viewers.iter().any(|(agent, _)| *agent == replacement)
                    && viewers.iter().any(|(agent, _)| *agent == late),
                "fixture: the re-attach and the single-pane hydration are both counted: \
                 {viewers:?}"
            );
            for (agent, client) in &viewers {
                assert_eq!(
                    client.as_deref(),
                    Some(tui.as_str()),
                    "agent {agent}: every attach this TUI made names the same client"
                );
            }

            let other = EmbeddedPaneController::new(path, runtime.handle().clone());
            assert_eq!(other.hydrate_from_daemon().len(), 3);
            let other_id = other.client_id().expect("identity").to_string();
            assert_ne!(other_id, tui, "a second TUI is a second client");
            let by_other = all_viewer_clients(&registry)
                .into_iter()
                .filter(|(_, client)| client.as_deref() == Some(other_id.as_str()))
                .count();
            assert_eq!(
                by_other, 3,
                "the second controller's attaches carry its own id"
            );

            drop((controller, other));
            registry.shutdown_all();
        }
    }
}

/// PRD #1666 part 2 (`lifecycle/handshake/011`): the TUI's own attach, listing
/// and start paths, against the scripted daemons of
/// [`crate::daemon_client::gate_fixture`].
#[cfg(all(test, unix))]
mod protocol_gate_tests {
    use super::*;
    use crate::daemon_client::gate_fixture::{GATE_FIXTURE_PROCESS, GateDaemon, GateHello};
    use crate::pane::PaneController;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build the test runtime")
    }

    /// Scenario: a pane's stream ended and auto-reattach starts looking for
    /// its agent, on a daemon whose `Hello` is refused, answered with garbage,
    /// or never answered — while its listing and attach verbs work. No check
    /// passed, so for the whole retry window it lists nothing and attaches
    /// nothing; a daemon whose check passes is reattached at once (control).
    #[tokio::test]
    async fn reattach_attaches_only_after_a_positive_protocol_check() {
        for hello in [GateHello::Refused, GateHello::Garbled, GateHello::Silent] {
            let daemon = GateDaemon::spawn(move |_| hello);
            // Ungated, so this pins the reattach loop's own check rather than
            // the client's gate.
            let client = DaemonClient::new(daemon.path.clone());
            let outcome = tokio::time::timeout(
                Duration::from_millis(1500),
                resolve_and_reattach(&client, "pane", None),
            )
            .await;
            assert!(
                !matches!(outcome, Ok(Ok(_))),
                "{hello:?}: reattached without a positive protocol check"
            );
            assert_eq!(
                daemon.recorded(),
                Vec::<String>::new(),
                "{hello:?}: listed or attached without a positive protocol check"
            );
        }
        let daemon = GateDaemon::spawn(|_| GateHello::Matching);
        let client = DaemonClient::new(daemon.path.clone());
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            resolve_and_reattach(&client, "pane", None),
        )
        .await;
        assert!(
            matches!(outcome, Ok(Ok(_))),
            "control: a passing check must reattach"
        );
    }

    /// Scenario: the TUI's pane controller, after the daemon behind its socket
    /// was replaced by one on the next attach protocol, is asked to open a new
    /// agent pane, to hydrate every pane from the daemon, and to hydrate one
    /// pane. Nothing is started, listed or attached on that daemon; the same
    /// asks on a daemon on this protocol reach it (control).
    #[test]
    fn a_gated_controller_starts_lists_and_attaches_nothing_on_another_protocol() {
        let runtime = runtime();
        let daemon = runtime.block_on(async { GateDaemon::spawn(|_| GateHello::Newer) });
        let controller = EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone())
            .with_protocol_gate();
        let created = controller.create_pane(Some("cat"), None);
        assert!(
            created.is_err(),
            "a new pane on another protocol: {created:?}"
        );
        assert!(controller.hydrate_from_daemon().is_empty());
        assert!(!controller.hydrate_pane(&BoundPane::held_by_controller("pane")));
        assert_eq!(
            daemon.recorded(),
            Vec::<String>::new(),
            "the TUI started, listed or attached on a daemon on another protocol"
        );

        let matching = runtime.block_on(async { GateDaemon::spawn(|_| GateHello::Matching) });
        let controller =
            EmbeddedPaneController::new(matching.path.clone(), runtime.handle().clone())
                .with_protocol_gate();
        let _ = controller.create_pane(Some("cat"), None);
        // A card learned from the matching daemon's own process.
        let _ = controller.hydrate_pane(&BoundPane::new(
            "pane",
            Incarnation::from(Some(GATE_FIXTURE_PROCESS.to_string())),
        ));
        let reached = matching.recorded();
        for op in ["start-agent", "list-agents", "attach-stream"] {
            assert!(
                reached.iter().any(|r| r.ends_with(&format!(":{op}"))),
                "control: {op} must reach a matching daemon: {reached:?}"
            );
        }
        drop(controller);
        drop(runtime);
    }
}

/// PRD #1666 part 2 (G-fix4): the TUI's pane controller acts on a pane's agent
/// only on the daemon process it attached to, against the scripted same-build
/// restarts of [`crate::daemon_client::restart_fixture`].
#[cfg(all(test, unix))]
mod incarnation_binding_tests {
    use super::*;
    use crate::daemon_client::restart_fixture::RestartingDaemon;
    use crate::pane::PaneController;
    use spec::spec;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build the test runtime")
    }

    /// Wait (bounded) until `done` holds; the controller sends its renames
    /// and resizes from spawned tasks.
    fn wait_for(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    const ACTIONS: [&str; 4] = ["set-agent-label", "post-message", "resize", "stop-agent"];

    /// Scenario: an agent card is known to the TUI but its terminal is detached, then the same-build daemon restarts and reuses its pane id. Ctrl+W must close only the stale card, display the gone sentence, and send the successor neither a listing nor a stop; a detached card on the original incarnation still closes its agent.
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_an_unattached_stale_card_stops_nothing() {
        let runtime = runtime();
        for restarted in [false, true] {
            let daemon = runtime.block_on(async { RestartingDaemon::spawn("a") });
            let controller =
                EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone())
                    .with_protocol_gate();
            let hydrated = controller.hydrate_from_daemon();
            assert_eq!(hydrated.len(), 1);
            controller
                .detach_pane("pane-1")
                .expect("card without a local terminal");
            assert!(controller.pane_ids().is_empty());
            if restarted {
                daemon.restart_as("b");
            }
            let result = controller.close_pane("pane-1");
            assert!(result.is_ok(), "closing the card completes: {result:?}");
            if restarted {
                assert_eq!(
                    daemon.received_by("b"),
                    Vec::<String>::new(),
                    "stale unattached card must not resolve or stop the successor"
                );
                assert_eq!(
                    controller.take_close_warnings(),
                    vec![crate::daemon_client::AGENT_GONE_AFTER_RESTART.to_string()]
                );
            } else {
                assert!(
                    daemon.received_by("a").contains(&"stop-agent".to_string()),
                    "control: current detached card stops its agent"
                );
            }
        }
    }

    /// Scenario: the TUI holds a pane attached on its daemon, or a card whose terminal it detached, and the daemon is replaced at its socket by one on the next attach protocol before the user presses Ctrl+W. The pane or card closes here, the status line says the agent could not be checked or stopped, and the replacement is sent nothing past its Hello; without the replacement the close stops the agent (control).
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_a_pane_whose_daemon_changed_protocol_closes_here() {
        let runtime = runtime();
        let local = crate::daemon_protocol::PROTOCOL_VERSION;
        for detached in [false, true] {
            for replaced in [false, true] {
                let case = format!("detached={detached} replaced={replaced}");
                let daemon = runtime.block_on(async { RestartingDaemon::spawn("a") });
                let controller =
                    EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone())
                        .with_protocol_gate();
                assert_eq!(controller.hydrate_from_daemon().len(), 1, "{case}");
                if detached {
                    controller.detach_pane("pane-1").unwrap();
                }
                if replaced {
                    daemon.restart_as("other-protocol-b");
                    if !detached {
                        // Where the pane's reattach leaves it on meeting the
                        // replacement.
                        controller
                            .mark_pane_lost_for_test("pane-1", PaneLostReason::ProtocolMismatch);
                    }
                }
                let result = controller.close_pane("pane-1");
                assert!(result.is_ok(), "{case}: Ctrl+W must close: {result:?}");
                assert!(
                    controller.pane_ids().is_empty(),
                    "{case}: the pane was kept"
                );
                if replaced {
                    assert_eq!(
                        daemon.received_by("other-protocol-b"),
                        Vec::<String>::new(),
                        "{case}: the replacement was sent a request about the old agent"
                    );
                    assert_eq!(
                        controller.take_close_warnings(),
                        vec![other_protocol_close_notice(Some(local + 1), local)],
                        "{case}"
                    );
                } else {
                    assert!(
                        daemon.received_by("a").contains(&"stop-agent".to_string()),
                        "{case}: control: the close stops the agent"
                    );
                    assert_eq!(
                        controller.take_close_warnings(),
                        Vec::<String>::new(),
                        "{case}"
                    );
                }
            }
        }
    }

    /// Scenario: hydrate a card, detach its terminal, and restart the daemon with an unrelated agent under the same pane id. Focusing that stale card must not hydrate the successor or send it a terminal attach; the same operation without a restart succeeds.
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_hydrating_a_stale_card_never_adopts_the_successor() {
        let runtime = runtime();
        for restarted in [false, true] {
            let daemon = runtime.block_on(async { RestartingDaemon::spawn("a") });
            let controller =
                EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone())
                    .with_protocol_gate();
            assert_eq!(controller.hydrate_from_daemon().len(), 1);
            controller.detach_pane("pane-1").unwrap();
            if restarted {
                daemon.restart_as("b");
            }
            let hydrated = controller.hydrate_pane(&BoundPane::held_by_controller("pane-1"));
            if restarted {
                assert!(
                    !hydrated,
                    "stale card hydrated the replacement's same-pane agent; received={:?}",
                    daemon.received_by("b")
                );
                assert!(
                    !daemon
                        .received_by("b")
                        .contains(&"attach-stream".to_string())
                );
            } else {
                assert!(hydrated, "control: same-incarnation card hydrates");
            }
        }
    }

    /// Scenario: the TUI opens a pane on a daemon, the daemon restarts as the same build, and the user renames the pane. The restarted daemon is sent no relabel, and the status line says "that agent is gone (the deck restarted)"; without the restart, the rename reaches the daemon and says nothing (control).
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_a_rename_after_a_restart_says_the_agent_is_gone() {
        let runtime = runtime();
        for restarted in [false, true] {
            let daemon = runtime.block_on(async { RestartingDaemon::spawn("a") });
            let controller =
                EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone())
                    .with_protocol_gate();
            let pane = controller
                .create_pane(Some("cat"), None)
                .expect("a pane on the first process");
            if restarted {
                daemon.restart_as("b");
            }
            controller
                .rename_pane(&pane, "renamed")
                .expect("the rename is taken locally");
            if restarted {
                let notices = std::cell::RefCell::new(Vec::new());
                wait_for("the refused relabel to reach the status line", || {
                    notices
                        .borrow_mut()
                        .extend(controller.take_close_warnings());
                    !notices.borrow().is_empty()
                });
                assert_eq!(
                    notices.into_inner(),
                    vec![crate::daemon_client::AGENT_GONE_AFTER_RESTART.to_string()]
                );
                assert_eq!(daemon.received_by("b"), Vec::<String>::new());
            } else {
                wait_for("control: the relabel to reach the daemon", || {
                    daemon
                        .received_by("a")
                        .contains(&"set-agent-label".to_string())
                });
                assert_eq!(controller.take_close_warnings(), Vec::<String>::new());
            }
        }
    }

    /// Scenario: a pane's reattach found its deck restarted, so the pane is marked gone, and the user then renames it. The rename is refused with "that agent is gone (the deck restarted)" on the status line, the card keeps its name, and no relabel is sent anywhere.
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_renaming_a_gone_pane_keeps_its_name() {
        let runtime = runtime();
        let daemon = runtime.block_on(async { RestartingDaemon::spawn("a") });
        let controller = EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone())
            .with_protocol_gate();
        let pane = controller
            .create_pane(Some("cat"), None)
            .expect("a pane on the first process");
        daemon.restart_as("b");
        let name_before = {
            let panes = controller.panes.lock().unwrap();
            let pane = panes.get(&pane).expect("the pane");
            *pane.backend.lost.lock().unwrap() = Some(PaneLostReason::DeckRestarted);
            pane.name.clone()
        };
        let renamed = controller.rename_pane(&pane, "stale-rename");
        assert!(
            matches!(&renamed, Err(PaneError::CommandFailed(m)) if m == crate::daemon_client::AGENT_GONE_AFTER_RESTART),
            "{renamed:?}"
        );
        assert_eq!(
            controller.panes.lock().unwrap().get(&pane).unwrap().name,
            name_before,
            "the gone pane keeps its name"
        );
        assert_eq!(
            controller.take_close_warnings(),
            vec![crate::daemon_client::AGENT_GONE_AFTER_RESTART.to_string()]
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !daemon
                .received_by("a")
                .contains(&"set-agent-label".to_string())
        );
        assert_eq!(daemon.received_by("b"), Vec::<String>::new());
    }

    /// Rename, message, resize and close the pane, in that order. The resize
    /// is sent by the pane's resize worker, which a close aborts with the
    /// pane, so the close waits for `sent` to say the daemon was asked.
    fn act_on(
        controller: &EmbeddedPaneController,
        pane: &str,
        sent: impl Fn(),
    ) -> Result<(), PaneError> {
        let _ = controller.rename_pane(pane, "renamed");
        let _ = controller.post_message(
            &BoundPane::held_by_controller(pane),
            "subject",
            "body",
            crate::inbox_hub::PostOrigin::OrchestratorRemit,
        );
        controller.resize_pane_pty(pane, 30, 100)?;
        sent();
        controller.close_pane(pane)
    }

    /// Scenario: the TUI opens a pane on a daemon, the daemon restarts as the
    /// same build and numbers an unrelated agent `1` and viewer `v1` again, and
    /// the user then renames the pane, messages its agent, resizes it and
    /// closes it with Ctrl+W. The restarted daemon is sent none of those; the
    /// close still removes the pane and says "that agent is gone (the deck
    /// restarted)". Without the restart, each one reaches the daemon (control).
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_a_pane_acts_only_on_the_daemon_process_it_attached_to() {
        let runtime = runtime();
        let daemon = runtime.block_on(async { RestartingDaemon::spawn("a") });
        let controller = EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone())
            .with_protocol_gate();
        let pane = controller
            .create_pane(Some("cat"), None)
            .expect("a pane on the first process");
        daemon.restart_as("b");

        let closed = act_on(&controller, &pane, || {
            wait_for(
                "the rename, message and resize to ask the restarted daemon",
                || daemon.hellos("b") >= ACTIONS.len() - 1,
            )
        });
        wait_for("every action to ask the restarted daemon", || {
            daemon.hellos("b") >= ACTIONS.len()
        });
        assert!(closed.is_ok(), "the close completes: {closed:?}");
        assert!(
            !controller.pane_ids().contains(&pane),
            "the closed pane is gone"
        );
        assert_eq!(
            controller.take_close_warnings(),
            vec![crate::daemon_client::AGENT_GONE_AFTER_RESTART.to_string()],
            "the close says why nothing was stopped"
        );
        assert_eq!(
            daemon.received_by("b"),
            Vec::<String>::new(),
            "the restarted daemon was sent an action aimed at the pane's agent"
        );

        let control = runtime.block_on(async { RestartingDaemon::spawn("a") });
        let controller =
            EmbeddedPaneController::new(control.path.clone(), runtime.handle().clone())
                .with_protocol_gate();
        let pane = controller
            .create_pane(Some("cat"), None)
            .expect("a pane on the control daemon");
        act_on(&controller, &pane, || {
            wait_for("control: the resize to reach the daemon", || {
                control.received_by("a").contains(&"resize".to_string())
            })
        })
        .expect("control: the close stops the agent");
        wait_for("control: every action to reach the daemon", || {
            let reached = control.received_by("a");
            ACTIONS.iter().all(|op| reached.iter().any(|r| r == op))
        });
        drop(controller);
        drop(runtime);
    }

    /// Scenario: a pane's stream ended and auto-reattach looks its agent up
    /// on a daemon that restarts as the same build right after answering the
    /// lookup. The restarted daemon is never attached under the id the old one
    /// listed: it is sent an attach only after a listing of its own, and the
    /// pane ends up attached on the restarted daemon's process.
    #[spec("lifecycle/handshake/013")]
    #[tokio::test]
    async fn handshake_013_reattach_attaches_only_on_the_process_that_listed() {
        let daemon = RestartingDaemon::spawn("a");
        daemon.restart_after("list-agents", "b");
        let client = DaemonClient::new(daemon.path.clone()).with_protocol_gate();
        let Ok(Ok((_, conn))) = tokio::time::timeout(
            Duration::from_secs(5),
            resolve_and_reattach(&client, "pane-1", None),
        )
        .await
        else {
            panic!("the restarted daemon's agent is attached once it has listed it");
        };
        assert_eq!(daemon.received_by("a"), vec!["list-agents".to_string()]);
        assert_eq!(
            daemon.received_by("b"),
            vec!["list-agents".to_string(), "attach-stream".to_string()],
            "the restarted daemon was attached under an id it never listed"
        );
        assert_eq!(
            conn.incarnation().and_then(Incarnation::as_deref),
            Some("b"),
            "the pane records the process it attached on"
        );
    }

    /// Scenario: the TUI hydrates its panes from the daemon, and the daemon
    /// restarts as the same build right after answering the listing. The
    /// restarted daemon is attached under none of the listed ids; with no
    /// restart, the listed agent is hydrated (control).
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_hydration_attaches_only_on_the_daemon_process_that_listed() {
        let runtime = runtime();
        let daemon = runtime.block_on(async { RestartingDaemon::spawn("a") });
        daemon.restart_after("list-agents", "b");
        let controller = EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone())
            .with_protocol_gate();
        assert!(controller.hydrate_from_daemon().is_empty());
        assert!(daemon.hellos("b") > 0, "precondition: the attach asked b");
        assert_eq!(daemon.received_by("b"), Vec::<String>::new());

        let control = runtime.block_on(async { RestartingDaemon::spawn("a") });
        let controller =
            EmbeddedPaneController::new(control.path.clone(), runtime.handle().clone())
                .with_protocol_gate();
        assert_eq!(controller.hydrate_from_daemon().len(), 1, "control");
        drop(controller);
        drop(runtime);
    }

    /// A card on `pane-1` learned from the daemon process `process`, as the
    /// event stream stamps one it painted (G-fix6).
    fn learned_from(process: &str) -> BoundPane {
        BoundPane::new("pane-1", Incarnation::from(Some(process.to_string())))
    }

    /// What one action on a card this controller never attached sent, and
    /// what it answered: a fresh daemon `a` and a fresh gated controller, the
    /// card stamped `card`, and a same-build restart to `b` first when
    /// `restarted`.
    fn on_a_never_attached_card(
        restarted: bool,
        card: BoundPane,
        act: impl FnOnce(&EmbeddedPaneController, &BoundPane) -> String,
    ) -> (Vec<String>, Vec<String>, String, Vec<String>) {
        let runtime = runtime();
        let daemon = runtime.block_on(async { RestartingDaemon::spawn("a") });
        let controller = EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone())
            .with_protocol_gate();
        if restarted {
            daemon.restart_as("b");
        }
        let answer = act(&controller, &card);
        // A press is sent from a spawned task; give it the time a reply takes.
        std::thread::sleep(Duration::from_millis(150));
        let notices = controller.take_close_warnings();
        let (a, b) = (daemon.received_by("a"), daemon.received_by("b"));
        drop(controller);
        drop(runtime);
        (a, b, answer, notices)
    }

    /// One action on a card, answering what it returned, for a message.
    type CardUse = fn(&EmbeddedPaneController, &BoundPane) -> String;

    /// Every way a card the TUI never attached — one painted from a
    /// daemon-surfaced `SessionStart`, such as a reused `sched-…` id — turns
    /// its pane id into a request: focus (hydrate), Ctrl+W, the configuration
    /// post, and an inbox press.
    fn never_attached_uses() -> Vec<(&'static str, &'static str, CardUse)> {
        vec![
            ("focus", "attach-stream", |c, card| {
                c.try_hydrate_pane(card).to_string()
            }),
            ("close", "stop-agent", |c, card| {
                format!("{:?}", c.close_card(card))
            }),
            ("post", "post-message", |c, card| {
                format!(
                    "{:?}",
                    c.post_message(
                        card,
                        "configuration request",
                        "body",
                        crate::inbox_hub::PostOrigin::ConfigGen
                    )
                )
            }),
            ("press", "inbox-retry", |c, card| {
                c.inbox_action_sender()
                    .send(card, "1", "m-1", 1, true, Duration::from_secs(2));
                String::new()
            }),
        ]
    }

    /// Scenario: the TUI holds a card it never attached, painted from a daemon-surfaced `SessionStart` on the daemon process `a`. The daemon restarts as the same build and reuses the card's pane id for an unrelated agent; the user then focuses the card, closes it with Ctrl+W, sends it the configuration request, and presses Retry on its inbox. The restarted daemon is sent none of those, and the close says "that agent is gone (the deck restarted)". With no restart, each one reaches `a` (control).
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_a_never_attached_card_acts_only_on_the_process_it_was_learned_from() {
        for (name, op, act) in never_attached_uses() {
            let (a, b, answer, notices) = on_a_never_attached_card(true, learned_from("a"), act);
            assert_eq!(
                b,
                Vec::<String>::new(),
                "{name}: the successor was sent a request for the stale card ({answer})"
            );
            assert!(a.is_empty(), "{name}: {a:?}");
            if name == "close" {
                assert_eq!(
                    notices,
                    vec![crate::daemon_client::AGENT_GONE_AFTER_RESTART.to_string()],
                    "close: {answer}"
                );
            }
            if name == "focus" {
                assert_eq!(answer, "false", "focus adopted the successor's agent");
            }

            let (a, b, answer, _) = on_a_never_attached_card(false, learned_from("a"), act);
            assert!(
                a.contains(&op.to_string()),
                "control, {name}: the card's own process gets {op}: {a:?} ({answer})"
            );
            assert!(b.is_empty());
        }
    }

    /// Scenario: the TUI holds a card it never attached that was learned while no named stream was open, so it carries no daemon process. Focusing it, closing it, posting to it and pressing Retry on it send nothing to the daemon that answers, which names a process; the close says "the deck did not identify itself, so the action on that agent was not sent".
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_a_card_learned_from_no_named_process_sends_nothing() {
        for (name, _, act) in never_attached_uses() {
            let card = BoundPane::new("pane-1", Incarnation::UNNAMED);
            let (a, _, answer, notices) = on_a_never_attached_card(false, card, act);
            assert_eq!(a, Vec::<String>::new(), "{name}: {answer}");
            if name == "close" {
                assert_eq!(
                    notices,
                    vec![crate::daemon_client::AGENT_IDENTITY_UNKNOWN.to_string()],
                    "close: {answer}"
                );
            }
        }
    }

    /// Scenario: the TUI hydrates a card, detaches its terminal, and the daemon restarts as the same build with an unrelated agent under the same pane id. Sending that card the configuration request posts nothing to the restarted daemon; without the restart, the post reaches the card's own process (control).
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_a_detached_cards_post_reaches_only_its_process() {
        let runtime = runtime();
        for restarted in [false, true] {
            let daemon = runtime.block_on(async { RestartingDaemon::spawn("a") });
            let controller =
                EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone())
                    .with_protocol_gate();
            assert_eq!(controller.hydrate_from_daemon().len(), 1);
            controller.detach_pane("pane-1").unwrap();
            if restarted {
                daemon.restart_as("b");
            }
            // The UI's card for it was learned from `a` too; what binds the
            // post is the controller's record of the attach.
            let posted = controller.post_message(
                &BoundPane::held_by_controller("pane-1"),
                "configuration request",
                "body",
                crate::inbox_hub::PostOrigin::ConfigGen,
            );
            if restarted {
                assert!(posted.is_err(), "{posted:?}");
                assert_eq!(daemon.received_by("b"), Vec::<String>::new());
            } else {
                assert!(posted.is_ok(), "control: {posted:?}");
                assert!(
                    daemon
                        .received_by("a")
                        .contains(&"post-message".to_string())
                );
            }
        }
    }

    /// Scenario: the TUI opens a pane on deck process `a`, the deck restarts as the same build, and the user renames the pane twice before the deck answers either. Both renames are refused, and the pane goes back to the name it had before the first one — never to the first rename's name — and the dashboard is told once, when no rename is left unanswered. Without the restart the pane keeps the second name and nothing is taken back (control).
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_overlapping_refused_renames_go_back_to_the_last_confirmed_name() {
        let runtime = runtime();
        for restarted in [false, true] {
            let daemon = runtime.block_on(async { RestartingDaemon::spawn("a") });
            let controller =
                EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone())
                    .with_protocol_gate();
            let pane = controller
                .create_pane(Some("cat"), None)
                .expect("a pane on the first process");
            let before = controller.panes.lock().unwrap()[&pane].name.clone();
            if restarted {
                daemon.restart_as("b");
            }
            // Neither rename is answered before both are sent; otherwise the
            // first can settle alone and is then rightly its own last.
            daemon.hold();
            controller
                .rename_pane(&pane, "first")
                .expect("taken locally");
            controller
                .rename_pane(&pane, "second")
                .expect("taken locally");
            daemon.release();
            let answers = std::cell::RefCell::new(Vec::new());
            wait_for("both renames to be answered", || {
                answers
                    .borrow_mut()
                    .extend(controller.take_rename_answers());
                answers.borrow().len() == 2
            });
            let answers = answers.into_inner();
            assert!(answers.iter().all(|answer| answer.pane_id == pane));
            assert_eq!(answers.iter().filter(|answer| answer.last).count(), 1);
            let name = controller.panes.lock().unwrap()[&pane].name.clone();
            if restarted {
                assert!(
                    answers.iter().all(|answer| answer.label.is_none()),
                    "{answers:?}"
                );
                assert_eq!(name, before, "a refused rename's name was restored");
            } else {
                assert!(
                    answers.iter().all(|answer| answer.label.is_some()),
                    "{answers:?}"
                );
                assert_eq!(name, "second", "control");
            }
        }
    }

    /// Scenario: two TUIs share a deck. This one holds a dashboard card learned from deck process `a` for pane `1`; the deck restarts as the same build, and this TUI starts its own first pane on `b`, also numbered `1`. Focusing, closing and posting through the old card refuse with "that agent is gone (the deck restarted)", and `b`'s agent is sent nothing and keeps its pane. A card learned from `b` itself focuses, posts to and closes that pane (control).
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_a_card_never_reaches_another_processs_pane_under_its_id() {
        let runtime = runtime();
        for stale in [false, true] {
            let daemon = runtime.block_on(async { RestartingDaemon::spawn("b") });
            let controller =
                EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone())
                    .with_protocol_gate();
            let pane = controller
                .create_pane(Some("cat"), None)
                .expect("this TUI's own pane on b");
            let card = BoundPane::new(
                pane.clone(),
                Incarnation::from(Some(if stale { "a" } else { "b" }.to_string())),
            );
            let before = daemon.received_by("b");

            let focused = controller.focus_card(&card);
            let posted = controller.post_message(
                &card,
                "configuration request",
                "body",
                crate::inbox_hub::PostOrigin::ConfigGen,
            );
            let closed = controller.close_card(&card);

            if stale {
                let gone = crate::daemon_client::AGENT_GONE_AFTER_RESTART.to_string();
                assert!(
                    matches!(&focused, Err(PaneError::Refused(m)) if *m == gone),
                    "{focused:?}"
                );
                assert!(posted.is_err(), "{posted:?}");
                assert!(
                    matches!(&closed, Err(PaneError::Refused(m)) if *m == gone),
                    "{closed:?}"
                );
                assert_eq!(
                    daemon.received_by("b"),
                    before,
                    "b's agent was sent something"
                );
                assert!(
                    controller.panes.lock().unwrap().contains_key(&pane),
                    "b's pane was taken from it"
                );
            } else {
                assert!(focused.is_ok(), "control: {focused:?}");
                assert!(posted.is_ok(), "control: {posted:?}");
                assert!(closed.is_ok(), "control: {closed:?}");
                let sent = daemon.received_by("b");
                assert!(
                    sent.contains(&"post-message".to_string()),
                    "control: {sent:?}"
                );
                assert!(
                    sent.contains(&"stop-agent".to_string()),
                    "control: {sent:?}"
                );
            }
        }
    }

    /// PR #1817 review 4347326632: this TUI's pane `1` was attached on deck
    /// process `a`, the deck restarted as `b`, and the dashboard's card for
    /// pane `1` was learned from `b`. Posting to that card goes to `b` and
    /// names no agent from `a`'s attach, which `b` would refuse as another
    /// agent's. A card learned from `a` itself names the agent this TUI
    /// attached (control).
    #[test]
    fn a_post_to_a_successors_card_names_no_agent_from_the_replaced_attach() {
        let runtime = runtime();
        for replaced in [false, true] {
            let daemon = runtime.block_on(async { RestartingDaemon::spawn("a") });
            let controller =
                EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone())
                    .with_protocol_gate();
            let pane = controller
                .create_pane(Some("cat"), None)
                .expect("this TUI's pane on a");
            let attached = controller.pane_attachment(&pane).unwrap().agent_id;
            let process = if replaced {
                daemon.restart_as("b");
                "b"
            } else {
                "a"
            };
            let card = BoundPane::new(pane.clone(), Incarnation::from(Some(process.to_string())));

            let posted = controller.post_message(
                &card,
                "configuration request",
                "body",
                crate::inbox_hub::PostOrigin::ConfigGen,
            );

            assert!(posted.is_ok(), "{process}: {posted:?}");
            let sent = daemon.requests_to(process, "post-message");
            assert_eq!(sent.len(), 1, "{process}: {sent:?}");
            let named = sent[0]["expected_agent_id"].as_str();
            if replaced {
                assert_eq!(named, None, "a's agent named to b: {sent:?}");
            } else {
                assert_eq!(named, Some(attached.as_str()), "control: {sent:?}");
            }
        }
    }

    /// Scenario: the user focuses a card the TUI never attached, learned from the daemon process `a`, and the deck restarts as the same build between listing the card's agent and attaching it, and again on startup hydration. Nothing is attached on the restarted deck, and the status line says "that agent is gone (the deck restarted)" rather than the refusal being logged only; with no restart, the focus attaches and says nothing (control).
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_a_deck_replaced_mid_hydration_is_said_on_the_status_line() {
        let runtime = runtime();
        for restarted in [false, true] {
            let daemon = runtime.block_on(async { RestartingDaemon::spawn("a") });
            let controller =
                EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone())
                    .with_protocol_gate();
            if restarted {
                daemon.restart_after("list-agents", "b");
            }
            let hydrated = controller.try_hydrate_pane(&learned_from("a"));
            let gone = vec![crate::daemon_client::AGENT_GONE_AFTER_RESTART.to_string()];
            if restarted {
                assert!(!hydrated);
                assert_eq!(daemon.received_by("b"), Vec::<String>::new());
                assert_eq!(controller.take_close_warnings(), gone, "focus");

                let startup = runtime.block_on(async { RestartingDaemon::spawn("a") });
                startup.restart_after("list-agents", "b");
                let controller =
                    EmbeddedPaneController::new(startup.path.clone(), runtime.handle().clone())
                        .with_protocol_gate();
                assert!(controller.hydrate_from_daemon().is_empty());
                assert_eq!(controller.take_close_warnings(), gone, "startup hydration");
            } else {
                assert!(hydrated, "control");
                assert_eq!(controller.take_close_warnings(), Vec::<String>::new());
            }
        }
    }

    /// Scenario: the TUI opens a pane on a daemon, the daemon restarts as the same build, and the user renames the pane. The deck refuses the rename after the TUI showed it, so the pane takes its old name back and the render loop is told to put the dashboard's names back too, as the desktop keeps the deck's name when it refuses a rename. Without the restart, the new name stays (control).
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_a_refused_rename_takes_the_name_back() {
        let runtime = runtime();
        for restarted in [false, true] {
            let daemon = runtime.block_on(async { RestartingDaemon::spawn("a") });
            let controller =
                EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone())
                    .with_protocol_gate();
            let pane = controller
                .create_pane(Some("cat"), None)
                .expect("a pane on the first process");
            let before = controller.panes.lock().unwrap()[&pane].name.clone();
            if restarted {
                daemon.restart_as("b");
            }
            controller
                .rename_pane(&pane, "renamed")
                .expect("the rename is taken locally");
            if restarted {
                let refused = std::cell::RefCell::new(Vec::new());
                wait_for("the refused rename to be taken back", || {
                    refused
                        .borrow_mut()
                        .extend(controller.take_rename_refusals());
                    !refused.borrow().is_empty()
                });
                assert_eq!(refused.into_inner(), vec![pane.clone()]);
                assert_eq!(controller.panes.lock().unwrap()[&pane].name, before);
            } else {
                wait_for("control: the relabel to reach the daemon", || {
                    daemon
                        .received_by("a")
                        .contains(&"set-agent-label".to_string())
                });
                std::thread::sleep(Duration::from_millis(50));
                assert_eq!(controller.take_rename_refusals(), Vec::<String>::new());
                assert_eq!(controller.panes.lock().unwrap()[&pane].name, "renamed");
            }
        }
    }
}

#[cfg(all(test, unix))]
mod resize_worker_tests {
    use super::*;
    use crate::daemon_client::restart_fixture::RestartingDaemon;

    /// A resize asked for before the pane's resize worker first runs, as the
    /// first layout pass after a pane is created can be, still reaches the
    /// daemon. The pane records the request and draws at the new size either
    /// way, so a lost one leaves the agent's PTY at the old size until the
    /// layout changes again.
    #[tokio::test]
    async fn a_resize_asked_before_the_worker_first_runs_reaches_the_daemon() {
        let daemon = RestartingDaemon::spawn("a");
        let (resize_tx, resize_rx) = tokio::sync::watch::channel(None);
        resize_tx.send_replace(Some((30, 100)));
        let attached = Arc::new(Mutex::new(PaneAttachment {
            agent_id: "1".into(),
            viewer: None,
            incarnation: None,
        }));
        let worker = tokio::spawn(resize_worker(
            resize_rx,
            DaemonClient::new(daemon.path.clone()),
            attached,
            Arc::new(Mutex::new(new_pane_parser(24, 80))),
        ));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !daemon.received_by("a").contains(&"resize".to_string()) {
            assert!(
                Instant::now() < deadline,
                "the resize never reached the daemon"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        worker.abort();
        drop(resize_tx);
    }
}
