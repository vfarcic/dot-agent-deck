use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use dot_agent_deck::daemon_client::{Endpoint, EndpointIdentity, FocusReport};
use dot_agent_deck::daemon_protocol::{
    KIND_DETACH, KIND_GEOMETRY, KIND_STREAM_END, KIND_STREAM_IN, KIND_STREAM_OUT,
    KIND_STREAM_REJECT, parse_geometry_frame, read_frame, write_frame,
};
use dot_agent_deck::platform::transport::TransportWriteHalf;
use tauri::ipc::{Channel, Response};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Mutex as AsyncMutex;

use crate::daemon_bridge::DaemonLinks;
use crate::dto::{
    TerminalAttachResult, TerminalState, TerminalStateEvent, safe_message, validate_agent_id,
    validate_dimensions, validate_terminal_input,
};
use crate::endpoint_tunnels::EndpointTunnels;
use crate::generation::Generation;

/// One deck's watcher slot: who claimed it, and the handle that ends it.
///
/// PRD #742 M8 gave the claim a name. The slot was a bare
/// `Option<JoinHandle>` — enough to say *a* watcher is starting, not enough to
/// say **which**, which is the whole of what
/// [`DesktopState::register_watcher`] has to decide. See the `watchers` field.
struct WatcherClaim {
    /// Minted by [`DesktopState::watcher_claims`]. Unique for the life of the
    /// process, so a slot re-created for the same deck never compares equal to
    /// the claim it replaced.
    token: u64,
    /// `None` between the claim and the spawn, and for the whole of that window
    /// there is nothing to abort — which is why the claim is made first and why
    /// `retain_watchers` removing an un-registered claim is enough to make the
    /// handle that arrives later abort itself.
    handle: Option<tauri::async_runtime::JoinHandle<()>>,
}

impl WatcherClaim {
    fn new(token: u64) -> Self {
        Self {
            token,
            handle: None,
        }
    }

    /// Is the task behind this claim still there to watch the deck (PRD #742
    /// M14)?
    ///
    /// A watcher loops forever by construction, so the only way its task ENDS
    /// is a panic or an abort — and in both cases the claim outlives it and,
    /// before this, went on refusing every later
    /// [`DesktopState::start_watcher_once_for`] for that deck. The deck then had
    /// no watcher and no way to get one short of restarting the app: the four
    /// paths that re-run `ensure_snapshot_watchers` — `desktop_bootstrap`,
    /// which the webview's Reconnect reaches, a settings save's
    /// `apply_selection`, and the two `desktop_run_action` arms that
    /// re-bootstrap — all go through that same refusal.
    ///
    /// M14 is what makes it worth naming. A deck with no watcher emits no
    /// snapshot, and the fleet view now renders a deck it has heard nothing
    /// from as PENDING rather than leaving it off the screen, so the cost moved
    /// from an absence nobody could see to a group that waits forever. Treating
    /// a finished task as no claim at all makes Reconnect the remedy it looks
    /// like.
    ///
    /// `None` is LIVE, not dead: that is the window between the claim and the
    /// spawn, where the task exists and its handle has not been handed over yet.
    /// Reading it as dead would let a second watcher start beside the first.
    fn watching(&self) -> bool {
        match &self.handle {
            Some(handle) => !handle.inner().is_finished(),
            None => true,
        }
    }
}

#[derive(Clone)]
struct TerminalSession {
    agent_id: String,
    /// The deck this session was attached over, captured at creation (PRD
    /// #1105's cross-deck pane, issue #1116).
    ///
    /// # Why the endpoint and not just the wire id
    ///
    /// [`resize`] has to reach that deck's daemon, and the only way to do that
    /// is to hand [`DaemonLinks::trusted`] an [`Endpoint`]. Holding the
    /// endpoint itself means the resize resolves through the deck the session
    /// belongs to rather than re-deriving one from the applied selection — a
    /// second lookup that could answer differently, which is exactly the
    /// read-at-use-time shape issue #1116 is about.
    ///
    /// It is also the registry's key: [`DesktopState::publish_scoped_session`]
    /// evicts by `(deck, agent)` rather than by agent id alone, because agent
    /// ids are per-daemon monotonic integers and two decks routinely mint the
    /// same one. Evicting by the bare id meant attaching deck B's `planner`
    /// silently tore down the live pane showing deck A's.
    endpoint: Endpoint,
    channel_id: u32,
    generation: u64,
    /// PRD #741 M3: a boxed transport half, not the IPC backend's concrete one.
    ///
    /// This is the site that decided the seam's shape. `TerminalSession` lives
    /// in a `HashMap` inside `DesktopState`, a Tauri-managed singleton every
    /// command reaches, so a type parameter here propagates to the registry, to
    /// `DesktopState` and to every command signature. The box costs one vtable
    /// dispatch per `KIND_STREAM_IN` frame (one per keystroke batch), which is
    /// invisible next to the syscall it precedes.
    ///
    /// **Not** because a local and a remote session could not share one map: an
    /// earlier version of this comment said so and it is false under PRD #741's
    /// DECISION 1A, where a remote deck is an `ssh -L` forwarded Unix socket and
    /// therefore the *same* concrete transport. M5 adds no second
    /// `AttachTransport` impl. See `platform::transport`'s module docs for the
    /// three reasons that do hold.
    ///
    /// Its `Drop` still half-closes, which is what tells the daemon this viewer
    /// is gone when a tile closes without an explicit DETACH.
    writer: Arc<AsyncMutex<TransportWriteHalf>>,
    /// PRD #882 — the viewer token this session's attach was given, sent back on
    /// every resize so the request updates THIS tile's constraint. Two tiles can
    /// show the same agent, so the token is what tells them apart — the process
    /// identity behind the connection cannot.
    viewer: Option<String>,
    /// PRD #741 M9 — a lease of this session's own on the transport it streams
    /// over.
    ///
    /// **This field is the residual M7 named and left as an argument.** The
    /// argument was: a session is alive only while a link is, so the tunnel is
    /// held for it in practice. That is true of the *establishment* moment and
    /// of nothing after it — `DaemonLinks` drops a link on a selection change
    /// and rebuilds one every `HANDSHAKE_REVALIDATE_INTERVAL`, while
    /// [`Self::writer`] is a write half on the forwarded socket that outlives
    /// the request that opened it by the whole life of the tile. Without a lease
    /// here, `apply_selection`'s `EndpointTunnels::retain` drops the map's
    /// handle, the last holder lets go, the `ssh` child dies, and a still-open
    /// terminal loses its transport mid-stream.
    ///
    /// M9 is where that stopped being hypothetical: it is the milestone that
    /// puts several tiles on a *remote* deck and gives the user a control that
    /// changes the selection while they are watching one.
    ///
    /// Never read. It exists for its `Drop` order — the lease is released when
    /// the session leaves the registry, which is what lets the `ssh` child go
    /// once the last tile on that deck has detached.
    _transport: Arc<crate::endpoint_tunnels::TunnelLease>,
}

pub(crate) struct DesktopState {
    sessions: Mutex<HashMap<String, TerminalSession>>,
    attach_gate: AsyncMutex<()>,
    next_generation: AtomicU64,
    /// The snapshot watchers, one per **observed deck** (PRD #742 M3).
    ///
    /// # Why this replaced an `AtomicBool`
    ///
    /// It was `watcher_started: AtomicBool` — one claim per *process*, because
    /// there was one watcher per process. The N-deck form of exactly that is one
    /// claim per *deck*: two watchers on one deck would fold the same broadcast
    /// twice and emit two snapshots per coalescing window for it, and a watcher
    /// left running for a deck the user has dropped from the fleet goes on
    /// emitting that deck's records into a view that no longer has a group for
    /// them.
    ///
    /// # The value is a claim, and the claim carries a token
    ///
    /// [`DesktopState::start_watcher_once_for`] claims the slot and the caller
    /// spawns the task and hands the handle back through
    /// [`DesktopState::register_watcher`]. Claiming before spawning rather than
    /// after is what makes the claim atomic: two callers racing to start the
    /// same deck's watcher cannot both win, which is the property the
    /// `AtomicBool`'s `swap` had.
    ///
    /// **The token is PRD #742 M8, and it is what makes a claim tell itself
    /// apart from a re-claim.** The slot used to be a bare
    /// `Option<JoinHandle>`, so `register_watcher` could ask only "is there a
    /// claim here" — and if the deck left and rejoined the observed set inside
    /// the claim -> spawn -> register window, watcher A's handle landed in
    /// watcher B's slot and B's own handle then *replaced* it. Replacing drops a
    /// `JoinHandle` rather than aborting it, so A went on running untracked and
    /// unstoppable for the life of the process: a leaked task and a
    /// double-watched deck, folding the same broadcast twice and emitting twice
    /// per coalescing window. With a token the slot answers "is this claim
    /// **mine**", and a handle that arrives for somebody else's claim is aborted
    /// like one that arrives for no claim at all.
    ///
    /// # A `std::sync::Mutex`, deliberately
    ///
    /// [`tauri::async_runtime::JoinHandle::abort`] is sync and every method here
    /// is sync, so an async mutex would buy nothing and would put
    /// `clippy::await_holding_lock` in front of the `retain_watchers` call site
    /// inside `retarget_selection`.
    watchers: Mutex<HashMap<EndpointIdentity, WatcherClaim>>,
    /// Mints [`WatcherClaim::token`] (PRD #742 M8).
    ///
    /// The same counter `crate::generation` gives the two establishment maps,
    /// used for the other thing a monotonic value answers: every `bump` hands
    /// back a number no other `bump` hands back, so a claim can be named by one
    /// and a later claim for the same deck can never be mistaken for it.
    watcher_claims: Generation,
    /// PRD #741 M4(a): the established daemon links, keyed by endpoint.
    ///
    /// This is the field the milestone is about. Before it, nothing in this
    /// process owned a daemon connection of any kind — every `trusted_daemon()`
    /// took a fresh handshake and dropped it, so a `get_snapshot()` cost two
    /// connections and the watcher paid that up to 6.667 times a second.
    ///
    /// An [`Arc`] rather than a plain field because the snapshot watcher is a
    /// `'static` spawned task that outlives any borrow of this state; it holds
    /// its own handle and invalidates through it when its event stream ends.
    pub(crate) daemon: Arc<DaemonLinks>,
    /// PRD #741 M7: the live `ssh -N -L` children, keyed by endpoint.
    ///
    /// **Beside [`DaemonLinks`], never inside it.** A `TrustedDaemon` is
    /// re-established every `HANDSHAKE_REVALIDATE_INTERVAL` (5 s); a tunnel
    /// owned by one would re-authenticate ssh every five seconds. The two maps
    /// share a key and nothing else — see `endpoint_tunnels`'s module docs for
    /// the ownership and teardown rules M9 and #742 inherit.
    ///
    /// An [`Arc`] for the same reason `daemon` is: the snapshot watcher is a
    /// `'static` task that outlives any borrow of this state.
    pub(crate) tunnels: Arc<EndpointTunnels>,
    /// How many times the selected deck has changed (PRD #741 M9).
    ///
    /// # Why the watcher needs telling, rather than noticing
    ///
    /// A watcher's event subscription is a connection to **one daemon**, opened
    /// once and read until it ends. Nothing about a selection change ends it:
    /// the old deck is still there, still healthy, still pushing.
    ///
    /// **What that used to mean, and what PRD #742 M3 changed.** With one
    /// watcher following the selection, missing this signal meant folding the
    /// *previous* deck's broadcasts into the agent view that answered snapshots
    /// for the *new* one and re-emitting them as `desktop://daemon-event` — one
    /// machine's fleet under another machine's name, the outcome PRD #741 exists
    /// to make impossible. That is no longer how it is prevented: there is a
    /// watcher per observed deck now, each pinned to its own endpoint for its
    /// whole life, so the fold and the label cannot come from different decks
    /// whether or not this signal ever arrives. The isolation is structural, and
    /// this is not what carries it.
    ///
    /// It is still the signal that a watcher's held link has been **invalidated
    /// under it** — `retarget_selection` calls `DaemonLinks::invalidate_all` —
    /// so the watcher re-establishes at once rather than on its next stream
    /// event, and, through `SubscriptionEnd::SelectionChanged`, without the
    /// backoff a genuinely dead deck earns.
    ///
    /// A departed deck is a different signal with a different mechanism:
    /// [`Self::retain_watchers`] ends its watcher outright.
    ///
    /// A [`tokio::sync::watch`] rather than a `Notify`: `changed()` is
    /// cancel-safe *and* edge-tracking per receiver, so a change that lands
    /// while the watcher is between subscriptions is still observed on its next
    /// wait instead of being lost. The value is a counter and nobody reads it;
    /// what carries the signal is that it moved.
    pub(crate) selection: tokio::sync::watch::Sender<u64>,
    /// PRD #1105 M11 step 4: whether the app window holds focus, as Tauri last
    /// reported it through [`window_focus_changed`].
    ///
    /// Read by [`establish`], so a terminal opened while the window is focused
    /// claims focus on its deck at once. Without that, focusing the app first
    /// and opening an agent's pane second — the ordinary order — would claim
    /// nothing on that deck until the window next lost and regained focus,
    /// leaving the new pane sized by whichever client claimed last.
    window_focused: AtomicBool,
}

impl Default for DesktopState {
    fn default() -> Self {
        // ONE tunnel map, reachable by two names. `DaemonLinks` needs it
        // because establishment is where a transport is acquired; the state
        // needs it because the selection-change and app-exit teardowns are
        // commands, not handshakes. Cloning the `Arc` is what keeps them the
        // same map rather than two that drift.
        let daemon = Arc::new(DaemonLinks::default());
        Self {
            sessions: Mutex::new(HashMap::new()),
            attach_gate: AsyncMutex::new(()),
            next_generation: AtomicU64::new(1),
            watchers: Mutex::new(HashMap::new()),
            watcher_claims: Generation::default(),
            daemon: Arc::clone(&daemon),
            tunnels: daemon.tunnels(),
            selection: tokio::sync::watch::Sender::new(0),
            window_focused: AtomicBool::new(false),
        }
    }
}

impl DesktopState {
    fn sessions(&self) -> Result<MutexGuard<'_, HashMap<String, TerminalSession>>, String> {
        self.sessions
            .lock()
            .map_err(|_| "desktop terminal session registry lock was poisoned".to_string())
    }

    /// PRD #1105 M11: record the window's focus without claiming anything — for
    /// seeding the state at startup, before any terminal is attached. A focus
    /// change goes through [`window_focus_changed`], which also claims.
    pub(crate) fn set_window_focused(&self, focused: bool) {
        self.window_focused.store(focused, Ordering::Relaxed);
    }

    /// PRD #1105 M11: every deck this app holds at least one **viewer** on, once
    /// each. A session with no viewer token registered nothing the daemon sizes
    /// by, so it is not a reason to claim focus there.
    fn viewed_decks(&self) -> Vec<Endpoint> {
        let Ok(sessions) = self.sessions() else {
            return Vec::new();
        };
        let mut seen = HashSet::new();
        sessions
            .values()
            .filter(|session| session.viewer.is_some())
            .filter(|session| seen.insert(session.endpoint.identity()))
            .map(|session| session.endpoint.clone())
            .collect()
    }

    /// The watcher registry, poison-tolerant.
    ///
    /// Recovered rather than refused, unlike [`Self::sessions`]: the map holds
    /// deck keys and abort handles, so a panic elsewhere cannot leave it in a
    /// state a later caller would misread — and refusing here would mean a
    /// panicked watcher permanently stopping every *other* deck from getting
    /// one.
    fn watchers(&self) -> MutexGuard<'_, HashMap<EndpointIdentity, WatcherClaim>> {
        self.watchers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Claim the watcher for one deck. `Some(token)` if this call is the one
    /// that must start it (PRD #742 M3; the token is M8).
    ///
    /// The N-deck form of the `AtomicBool` swap this replaced — one claim per
    /// deck rather than one per process. The caller that gets a token spawns the
    /// task and hands that token and its handle to [`Self::register_watcher`].
    pub(crate) fn start_watcher_once_for(&self, deck: &EndpointIdentity) -> Option<u64> {
        let mut watchers = self.watchers();
        // PRD #742 M14: a claim whose task has ENDED is not a claim. See
        // `WatcherClaim::watching` for why a watcher's task ending at all is
        // already a bug, and why refusing on the strength of it was the worse
        // of the two failures.
        if watchers.get(deck).is_some_and(WatcherClaim::watching) {
            return None;
        }
        // Minted under the map lock, so the token in the slot and the token the
        // caller holds are written in one critical section.
        let token = self.watcher_claims.bump();
        watchers.insert(deck.clone(), WatcherClaim::new(token));
        Some(token)
    }

    /// Hand the spawned task's handle to the claim [`Self::start_watcher_once_for`]
    /// made, so [`Self::retain_watchers`] can end it.
    ///
    /// A handle is stored **only into the claim that asked for it**. Anything
    /// else is aborted on the spot, and there are two ways to be anything else:
    ///
    /// - *the claim is gone* — the deck left the observed set between the claim
    ///   and the spawn, so this task is already watching a deck nobody asked
    ///   about;
    /// - *the claim is somebody else's* — the deck left **and rejoined** in that
    ///   window, so the slot now belongs to a later watcher (PRD #742 M8). This
    ///   is the case the pre-M8 `Some(slot) => *slot = Some(handle)` could not
    ///   see: it wrote this handle into the newer claim, whose own
    ///   `register_watcher` then replaced it — *dropping* a `JoinHandle` instead
    ///   of aborting it, and leaving this task running untracked forever.
    ///
    /// Either way the handle is aborted rather than dropped, which is the
    /// difference between a task that stops and a task nothing can stop.
    pub(crate) fn register_watcher(
        &self,
        deck: &EndpointIdentity,
        token: u64,
        handle: tauri::async_runtime::JoinHandle<()>,
    ) {
        match self.watchers().get_mut(deck) {
            Some(claim) if claim.token == token => claim.handle = Some(handle),
            _ => handle.abort(),
        }
    }

    /// End the watcher for every deck `observed` no longer names (PRD #742 M3).
    ///
    /// The sibling of the `EndpointTunnels::retain` on the line above its call
    /// site: the transport half of a departed deck's teardown was already a set
    /// difference, and so is this one.
    ///
    /// **What `abort` does and does not buy.** A watcher parked in its
    /// `select!` stops before its next emit; one already inside `snapshot_with`
    /// is cancelled at its next await, so it may have an emit in flight. The
    /// stronger property — the watcher ends *before* any further record from
    /// that deck reaches the webview — needs a running Tauri app to observe and
    /// is not pinned by any test here; it is stated rather than claimed.
    pub(crate) fn retain_watchers(&self, observed: &HashSet<EndpointIdentity>) {
        self.watchers().retain(|deck, claim| {
            if observed.contains(deck) {
                return true;
            }
            if let Some(handle) = claim.handle.take() {
                handle.abort();
            }
            false
        });
    }

    /// Which decks currently have a watcher.
    #[cfg(test)]
    pub(crate) fn watched_decks(&self) -> HashSet<EndpointIdentity> {
        self.watchers().keys().cloned().collect()
    }

    /// Announce that the selected deck has changed (PRD #741 M9). See
    /// [`Self::selection`].
    pub(crate) fn selection_changed(&self) {
        self.selection.send_modify(|generation| *generation += 1);
    }

    /// Install one session, replacing any earlier session for **the same agent
    /// on the same deck** — but only if `scope` is still the fleet's.
    ///
    /// The deck half of the eviction rule is PRD #1105's cross-deck pane. It
    /// read `existing.agent_id != session.agent_id`, which is a statement about
    /// a name that is unique only within a daemon — so attaching `planner` on
    /// build-box evicted the live `planner` session on the local deck, closing
    /// a pane the user was watching and leaving its webview channel talking to
    /// a torn-down stream. One session per agent per deck is the invariant the
    /// single-deck version was reaching for.
    ///
    /// # The revalidation is the observed-set boundary, and the LOCK is what
    /// makes it exact
    ///
    /// `crate::retarget_selection` writes the applied selection (bumping the
    /// epoch) and *then* calls [`detach_decks_outside`], which takes this same
    /// registry lock to collect what it will tear down. So the two orderings
    /// are both correct and there is no third:
    ///
    /// - this call takes the lock first → it may still see the old epoch and
    ///   publish, and the teardown's collect then finds the session and detaches
    ///   it;
    /// - the teardown's collect takes the lock first → the selection write has
    ///   already landed, so the read below sees the bumped epoch and refuses.
    ///
    /// Checking outside the lock would leave exactly the window the third audit
    /// described: check, teardown, insert.
    ///
    /// # It refuses a scope that is not about this session's deck
    ///
    /// Not defensive tidiness — the scope is the operation's single statement of
    /// which deck it is, and a session built for another one would make that
    /// statement false while still revalidating happily. Nothing in the tree
    /// does this; the refusal is here so that a future caller which does gets a
    /// message instead of a silently mis-filed session.
    fn publish_scoped_session(
        &self,
        session_id: String,
        session: TerminalSession,
        scope: &crate::dto::DeckScope,
    ) -> Result<(), Box<RejectedSession>> {
        let deck = session.endpoint.identity();
        if deck != scope.identity() {
            return Err(Box::new(RejectedSession {
                reason: "a terminal session may only be published under a scope for its own deck"
                    .to_string(),
                session,
            }));
        }
        let mut sessions = match self.sessions() {
            Ok(sessions) => sessions,
            Err(reason) => return Err(Box::new(RejectedSession { reason, session })),
        };
        if let Err(reason) = scope.revalidate() {
            drop(sessions);
            return Err(Box::new(RejectedSession { reason, session }));
        }
        sessions.retain(|_, existing| {
            existing.agent_id != session.agent_id || existing.endpoint.identity() != deck
        });
        sessions.insert(session_id, session);
        Ok(())
    }

    /// [`Self::publish_scoped_session`] for a test pinning the eviction rule
    /// rather than the boundary.
    ///
    /// **`#[cfg(test)]` is load-bearing.** It is what makes
    /// `publish_scoped_session` the *only* way production code can put a session
    /// in this registry — a fact the compiler enforces rather than a convention
    /// a reviewer has to check. Half of the boundary's proof is that: the other
    /// half is a test of the refusal itself.
    ///
    /// The scope it supplies is captured now and names this session's own deck,
    /// so the boundary stays armed and simply has nothing to object to.
    #[cfg(test)]
    fn insert_unique_session(
        &self,
        session_id: String,
        session: TerminalSession,
    ) -> Result<(), String> {
        let scope = crate::dto::DeckScope::capturing(session.endpoint.clone());
        self.publish_scoped_session(session_id, session, &scope)
            .map_err(|rejected| rejected.reason)
    }
}

/// A session that was built and then refused publication, handed back so the
/// caller can DETACH it rather than leaking a viewer the daemon still sizes for.
///
/// The session travels with the reason because the only useful thing to do with
/// a refusal here is to say goodbye on the wire — see [`detach_unpublished`].
struct RejectedSession {
    reason: String,
    session: TerminalSession,
}

fn session_id(generation: u64) -> String {
    format!("terminal-{generation:016x}")
}

fn emit_terminal_state(app: &AppHandle, event: TerminalStateEvent) {
    let _ = app.emit("desktop://terminal-state", event);
}

/// PRD #882 — tell the frontend the daemon changed this agent's applied
/// geometry, so the tile can reshape its xterm grid to match.
fn emit_terminal_geometry(app: &AppHandle, event: crate::dto::TerminalGeometryEvent) {
    let _ = app.emit("desktop://terminal-geometry", event);
}

fn rejection_notice(reason: &[u8]) -> Vec<u8> {
    let reason = safe_message(String::from_utf8_lossy(reason));
    let reason = reason
        .chars()
        .map(|character| {
            if matches!(character, '\n' | '\t') {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let reason = if reason.trim().is_empty() {
        "the deck refused terminal input"
    } else {
        reason.trim()
    };
    format!("\r\n[agent-deck] terminal input rejected: {reason}\r\n").into_bytes()
}

/// What [`establish`] hands [`attach`].
///
/// The read half travels separately from the [`TerminalAttachResult`] because a
/// **reused** session has one and not the other: nothing was attached, so there
/// is no new stream to read and no `Attached` event to emit.
#[derive(Debug)]
struct Established {
    result: TerminalAttachResult,
    stream: Option<dot_agent_deck::platform::transport::TransportReadHalf>,
    /// PRD #1105 M11: the focus claim this attach started, if the window was
    /// focused. Production lets it run; it is carried out only so a test can
    /// wait for it rather than poll the daemon. Those tests are Unix-only.
    #[cfg_attr(not(all(test, unix)), allow(dead_code))]
    focus_claim: Option<tauri::async_runtime::JoinHandle<Result<FocusReport, String>>>,
}

/// Everything [`attach`] does except emitting and spawning — which is to say,
/// the whole of the operation that has a **deck**.
///
/// # Why this is split out
///
/// The same reason `crate::retarget_selection` is: reaching an emit needs an
/// `AppHandle`, which needs a running Tauri app, and every interesting decision
/// here happens before the first one. What that buys is specific rather than
/// tidy — the observed-set boundary below is reachable from a test, over a
/// scripted socket, with the fleet moved underneath it mid-flight. It was not
/// before, and the third identity audit found the hole where the tests could
/// not go.
///
/// [`attach`] holds no deck at all after this returns, so there is nothing left
/// up there to get wrong.
async fn establish(
    state: &DesktopState,
    deck_id: Option<String>,
    agent_id: String,
    on_output: &Channel<Response>,
    viewport: Option<(u16, u16)>,
) -> Result<Established, String> {
    validate_agent_id(&agent_id)?;
    // ONE capture for the whole operation, before the first await (issue
    // #1116). Resolved BEFORE the gate too, so a bad deck id is refused
    // without queueing behind somebody else's handshake.
    let scope = crate::dto::DeckScope::resolve(deck_id.as_deref())?;
    let deck = scope.identity();
    let _attach_guard = state.attach_gate.lock().await;
    // The gate is process-wide and a handshake over `ssh` can hold it for
    // tens of seconds, so the fleet can have moved entirely while this attach
    // sat in the queue. Refusing here is HARM REDUCTION rather than the
    // boundary: it means no `ssh` child is spawned and no credential is
    // presented to a host the user has just removed. The boundary itself is
    // `publish_scoped_session` below, which is the check that cannot be
    // outrun — this one can, by a removal that lands after it.
    scope.revalidate()?;
    let channel_id = on_output.id();
    // The deck is part of the reuse check for the same reason it is part of the
    // registry key: `(agent_id, channel_id)` alone would answer "you already
    // have this" for another deck's namesake.
    if let Some((session_id, session)) = state.sessions()?.iter().find(|(_, session)| {
        session.agent_id == agent_id
            && session.channel_id == channel_id
            && session.endpoint.identity() == deck
    }) {
        return Ok(Established {
            result: TerminalAttachResult {
                session_id: session_id.clone(),
                agent_id,
                generation: session.generation,
                reused: true,
                // A reused session keeps whatever geometry it already has; the
                // frontend's grid is already sized to it and no attach happened.
                applied_rows: None,
                applied_cols: None,
            },
            stream: None,
            focus_claim: None,
        });
    }
    // This deck's earlier session for this agent, never another deck's.
    detach_agent_on(state, &deck, &agent_id).await;
    let daemon = state.daemon.trusted(scope.endpoint()).await?;
    daemon.require_compatible()?;
    // PRD #882: a half-measured tile (one axis zero) declares nothing rather
    // than a geometry it does not mean — under a smallest-wins policy a bogus
    // constraint would shrink the agent for every other client too.
    let viewport = viewport.filter(|(rows, cols)| *rows > 0 && *cols > 0);
    // PRD #741 M9: taken from the link the attach is about to ride, BEFORE the
    // attach, so the session and its stream are leased to the same transport.
    let transport = daemon.transport();
    // PRD #882: participate in the policy either way. The tile very often
    // attaches BEFORE the webview has measured it — the shown-set effect drives
    // the attach and `FitAddon` runs a frame later — and a tile that attached as
    // a non-participant would send its first resize with no viewer token, which
    // the daemon applies as an unattributed override across every other client.
    // Registering now and contributing a constraint on the first resize is the
    // difference between joining the policy and overriding it.
    let connection = match viewport {
        Some(viewport) => {
            daemon
                .client
                .attach_as_viewer(&agent_id, Some(viewport))
                .await
        }
        None => daemon.client.attach_pending_viewport(&agent_id).await,
    }
    .map_err(|error| safe_message(error.to_string()))?;
    let viewer = connection.viewer().map(|v| v.to_string());
    let applied = connection.applied();
    let (reader, writer) = connection.into_split();
    let generation = state.next_generation.fetch_add(1, Ordering::Relaxed);
    let session_id = session_id(generation);
    let session = TerminalSession {
        agent_id: agent_id.clone(),
        // From the SCOPE, so the session's deck and the deck this operation
        // authenticated against are one value rather than two that agree.
        endpoint: scope.endpoint().clone(),
        channel_id,
        generation,
        writer: Arc::new(AsyncMutex::new(writer)),
        viewer,
        _transport: transport,
    };
    match state.publish_scoped_session(session_id.clone(), session, &scope) {
        Ok(()) => {}
        Err(rejected) => {
            // The audit's point about the tunnel/link epoch: refusing to
            // publish is not enough on its own, because the lease and the open
            // stream reached this frame anyway. So say goodbye on the wire
            // before dropping them — otherwise the daemon carries this viewer
            // until it notices a half-closed transport, and until then it is
            // sizing the agent to a pane that will never be drawn.
            detach_unpublished(rejected.session).await;
            return Err(rejected.reason);
        }
    }

    // PRD #1105 M11: a pane opened while the window is focused claims focus on
    // its deck, because the claim the window made when it gained focus covered
    // only the decks it had a viewer on then. After the attach rather than
    // before it, so a refused attach claims nothing. Spawned, so the attach does
    // not wait on a second round trip.
    let focus_claim = state.window_focused.load(Ordering::Relaxed).then(|| {
        tauri::async_runtime::spawn(claim_focus_on(
            Arc::clone(&state.daemon),
            scope.endpoint().clone(),
        ))
    });

    Ok(Established {
        focus_claim,
        result: TerminalAttachResult {
            session_id,
            agent_id,
            generation,
            reused: false,
            // PRD #882: the geometry in force at attach time, resolved under the
            // same daemon lock as the scrollback replay that is about to arrive
            // on the channel. The frontend sizes its grid from this before
            // writing those bytes, so the replay is parsed at the geometry it
            // was written at rather than at whatever the tile happened to
            // measure.
            applied_rows: applied.map(|(rows, _)| rows),
            applied_cols: applied.map(|(_, cols)| cols),
        },
        stream: Some(reader),
    })
}

/// PRD #1105 M11 step 4 — the bound on one focus claim once the deck's link is
/// held. The link's own establishment has its own bounds; this covers only the
/// claim's request and reply, so a wedged daemon cannot hold the task open.
const FOCUS_CLAIM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// PRD #1105 M11 step 4 — claim focus on one deck as this app's client.
///
/// Through the deck's held link, whose client carries
/// [`crate::daemon_bridge::desktop_client_id`]. [`FocusReport::Withheld`] when
/// the daemon does not advertise `focus-gained` — the check is
/// `DaemonClient::focus_gained`'s, answered from the capability set the link
/// captured at its own handshake, so an older daemon is never sent the claim.
async fn claim_focus_on(
    links: Arc<DaemonLinks>,
    endpoint: Endpoint,
) -> Result<FocusReport, String> {
    let daemon = links.trusted(&endpoint).await?;
    daemon.require_compatible()?;
    match tokio::time::timeout(FOCUS_CLAIM_TIMEOUT, daemon.client.focus_gained()).await {
        Ok(claimed) => claimed.map_err(|error| safe_message(error.to_string())),
        Err(_) => Err("focus claim timed out".to_string()),
    }
}

/// PRD #1105 M11 step 4 — the app window gained or lost focus.
///
/// Records the state for [`establish`], and on **gaining** focus claims it on
/// every deck this app holds a viewer on, one claim per deck, concurrently so a
/// slow remote deck does not delay a local one. Losing focus claims nothing:
/// the contract has no focus-lost message, because under "last focused wins"
/// leaving the app for a browser must reflow nothing.
///
/// **Why those decks.** Since the cross-deck attach the app views agents on
/// several decks at once, and each daemon keeps its own last-focused client, so
/// a claim has to be made per deck. A deck where this app holds a viewer is
/// exactly a deck where the claim can change an agent's size: under the rule, a
/// focused client with no viewer of an agent falls through to the fallback for
/// it. Claiming on the other observed decks would change nothing there, and
/// would spend a handshake on each — an `ssh` round trip for a remote deck,
/// or a reconnect attempt against one that is down — every time the window is
/// focused. The deck a pane is opened on *after* focus-in is covered by the
/// claim in [`establish`].
///
/// **Why the Tauri window event, and not the webview's.** Tauri's runtime
/// produces `WindowEvent::Focused` per platform — from GTK's `focus-in-event` on
/// Linux and `windowDidBecomeKey` on macOS, and on Windows it synthesizes the
/// event from WebView2's `GotFocus`/`LostFocus` — so one Rust-side handler
/// covers all three. The DOM `focus` event gets no such treatment from Tauri,
/// and handling it would also route a Rust-side decision through the webview
/// and back.
///
/// **No input trigger.** Typing into a terminal does not claim. Keystrokes go
/// to the focused window, so they reach a terminal only after the window gained
/// focus and claimed. The bytes that reach [`write`] would also be the wrong
/// signal: `TerminalViewport` forwards everything xterm.js's `onData` emits,
/// which includes xterm.js's own replies to an agent's terminal queries, and
/// those would claim focus while the person is looking at another client.
///
/// Returns each deck's outcome, for tests; production discards it.
pub(crate) async fn window_focus_changed(
    state: &DesktopState,
    focused: bool,
) -> Vec<(EndpointIdentity, Result<FocusReport, String>)> {
    state.window_focused.store(focused, Ordering::Relaxed);
    if !focused {
        return Vec::new();
    }
    let claims: Vec<_> = state
        .viewed_decks()
        .into_iter()
        .map(|endpoint| {
            let deck = endpoint.identity();
            let claim =
                tauri::async_runtime::spawn(claim_focus_on(Arc::clone(&state.daemon), endpoint));
            (deck, claim)
        })
        .collect();
    let mut outcomes = Vec::with_capacity(claims.len());
    for (deck, claim) in claims {
        let outcome = claim
            .await
            .unwrap_or_else(|error| Err(safe_message(error.to_string())));
        outcomes.push((deck, outcome));
    }
    outcomes
}

pub(crate) async fn attach(
    app: &AppHandle,
    state: &DesktopState,
    // PRD #1105 — which deck's `agent_id` this is. `None` means the selected
    // deck; see [`crate::dto::DeckScope::resolve`] for why that fallback exists
    // and why the resolution is against the observed set.
    deck_id: Option<String>,
    agent_id: String,
    on_output: Channel<Response>,
    // PRD #882 — the geometry this tile can draw the agent at, measured by
    // `FitAddon` in the webview. Declaring it registers the tile as a viewer, so
    // the daemon sizes the agent to the smallest pane among every client
    // watching it and tells this tile whenever that changes.
    viewport: Option<(u16, u16)>,
) -> Result<TerminalAttachResult, String> {
    let Established { result, stream, .. } =
        establish(state, deck_id, agent_id, &on_output, viewport).await?;
    let Some(mut reader) = stream else {
        // A reused session: the caller already has a live stream task and an
        // `Attached` event from the attach that created it.
        return Ok(result);
    };
    let generation = result.generation;
    let session_id = result.session_id.clone();
    let agent_id = result.agent_id.clone();

    emit_terminal_state(
        app,
        TerminalStateEvent {
            session_id: session_id.clone(),
            agent_id: agent_id.clone(),
            generation,
            state: TerminalState::Attached,
            message: None,
        },
    );

    let app_for_stream = app.clone();
    let session_for_stream = session_id.clone();
    let agent_for_stream = agent_id.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            match read_frame(&mut reader).await {
                Ok(Some((KIND_STREAM_OUT, data))) => {
                    if let Err(error) = on_output.send(Response::new(data)) {
                        emit_terminal_state(
                            &app_for_stream,
                            TerminalStateEvent {
                                session_id: session_for_stream.clone(),
                                agent_id: agent_for_stream.clone(),
                                generation,
                                state: TerminalState::Error,
                                message: Some(safe_message(format!(
                                    "terminal output channel closed: {error}"
                                ))),
                            },
                        );
                        break;
                    }
                }
                Ok(Some((KIND_STREAM_END, reason))) => {
                    emit_terminal_state(
                        &app_for_stream,
                        TerminalStateEvent {
                            session_id: session_for_stream.clone(),
                            agent_id: agent_for_stream.clone(),
                            generation,
                            state: TerminalState::End,
                            message: (!reason.is_empty())
                                .then(|| safe_message(String::from_utf8_lossy(&reason))),
                        },
                    );
                    break;
                }
                // PRD #882: the daemon applied a new geometry for this agent.
                // Non-terminal, like a rejection — the stream stays open and
                // output keeps flowing; only the grid changes.
                Ok(Some((KIND_GEOMETRY, payload))) => {
                    if let Some((rows, cols)) = parse_geometry_frame(&payload) {
                        emit_terminal_geometry(
                            &app_for_stream,
                            crate::dto::TerminalGeometryEvent {
                                session_id: session_for_stream.clone(),
                                agent_id: agent_for_stream.clone(),
                                generation,
                                rows,
                                cols,
                            },
                        );
                    }
                }
                Ok(Some((KIND_STREAM_REJECT, reason))) => {
                    // Protocol v6 rejections are explicitly non-terminal: the
                    // daemon refused one input frame because the target is no
                    // longer writable, but the attachment remains useful for
                    // output. Surface the reason in-band without tearing down
                    // the session or changing the frontend lifecycle contract.
                    if let Err(error) = on_output.send(Response::new(rejection_notice(&reason))) {
                        emit_terminal_state(
                            &app_for_stream,
                            TerminalStateEvent {
                                session_id: session_for_stream.clone(),
                                agent_id: agent_for_stream.clone(),
                                generation,
                                state: TerminalState::Error,
                                message: Some(safe_message(format!(
                                    "terminal output channel closed: {error}"
                                ))),
                            },
                        );
                        break;
                    }
                }
                Ok(None) => {
                    emit_terminal_state(
                        &app_for_stream,
                        TerminalStateEvent {
                            session_id: session_for_stream.clone(),
                            agent_id: agent_for_stream.clone(),
                            generation,
                            state: TerminalState::End,
                            message: None,
                        },
                    );
                    break;
                }
                Ok(Some((kind, _))) => {
                    emit_terminal_state(
                        &app_for_stream,
                        TerminalStateEvent {
                            session_id: session_for_stream.clone(),
                            agent_id: agent_for_stream.clone(),
                            generation,
                            state: TerminalState::Error,
                            message: Some(format!(
                                "unexpected terminal frame kind 0x{kind:02x} from the deck"
                            )),
                        },
                    );
                    break;
                }
                Err(error) => {
                    emit_terminal_state(
                        &app_for_stream,
                        TerminalStateEvent {
                            session_id: session_for_stream.clone(),
                            agent_id: agent_for_stream.clone(),
                            generation,
                            state: TerminalState::Error,
                            message: Some(safe_message(error.to_string())),
                        },
                    );
                    break;
                }
            }
        }

        let state = app_for_stream.state::<DesktopState>();
        if let Ok(mut sessions) = state.sessions()
            && sessions
                .get(&session_for_stream)
                .is_some_and(|session| session.generation == generation)
        {
            sessions.remove(&session_for_stream);
        }
    });

    Ok(result)
}

/// A best-effort DETACH over a session that was built and then **not**
/// published, so the daemon drops this viewer now rather than when it notices a
/// half-closed transport.
///
/// Bounded for the reason [`detach_matching`]'s loop is: the write travels over
/// a transport that may be an `ssh` child on its way out. Consuming the session
/// rather than borrowing it is the other half of the job — its `_transport`
/// lease is released on drop, which is what lets that child go.
async fn detach_unpublished(session: TerminalSession) {
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), async {
        let mut writer = session.writer.lock().await;
        let _ = write_frame(&mut *writer, KIND_DETACH, &[]).await;
    })
    .await;
}

pub(crate) async fn write(
    state: &DesktopState,
    session_id: &str,
    data: &[u8],
) -> Result<(), String> {
    validate_terminal_input(data)?;
    let writer = state
        .sessions()?
        .get(session_id)
        .map(|session| Arc::clone(&session.writer))
        .ok_or_else(|| format!("terminal session not found: {}", safe_message(session_id)))?;
    let mut writer = writer.lock().await;
    write_frame(&mut *writer, KIND_STREAM_IN, data)
        .await
        .map_err(|error| safe_message(error.to_string()))
}

/// PRD #1105 — the daemon is resolved from the **session's own** endpoint, not
/// from the applied selection.
///
/// This was `trusted_daemon(&state.daemon)`, and it is the sharpest instance of
/// issue #1116's pattern left in the crate: a resize carries a session id, so
/// the deck it belongs to is knowable exactly, and reading the current
/// selection instead sent this pane's measured grid to another machine's
/// same-id agent. Under the daemon's viewer size policy that is not a
/// display blemish — it reflows that agent's PTY and every other client
/// watching it.
///
/// No deck parameter is added for this. The session id names one attach on one
/// deck, so asking the caller to repeat the deck would introduce a second
/// opinion about it — and a wrong one would be trusted over the session's.
pub(crate) async fn resize(
    state: &DesktopState,
    session_id: &str,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let (rows, cols) = validate_dimensions(rows, cols)?;
    let (agent_id, viewer, endpoint) = state
        .sessions()?
        .get(session_id)
        .map(|session| {
            (
                session.agent_id.clone(),
                session.viewer.clone(),
                session.endpoint.clone(),
            )
        })
        .ok_or_else(|| format!("terminal session not found: {}", safe_message(session_id)))?;
    let daemon = state.daemon.trusted(&endpoint).await?;
    daemon.require_compatible()?;
    // PRD #882: name this tile's viewer so the request updates its constraint
    // rather than overriding every other client's. The daemon answers with what
    // it actually applied; the frontend learns that number from the
    // `desktop://terminal-geometry` event the daemon pushes to every viewer,
    // including this one, so nothing is returned here.
    daemon
        .client
        .resize_agent_as_viewer(&agent_id, rows, cols, viewer.as_deref())
        .await
        .map(|_| ())
        .map_err(|error| safe_message(error.to_string()))
}

pub(crate) async fn detach(state: &DesktopState, session_id: &str) -> Result<bool, String> {
    let session = state.sessions()?.remove(session_id);
    let Some(session) = session else {
        return Ok(false);
    };
    let mut writer = session.writer.lock().await;
    let _ = write_frame(&mut *writer, KIND_DETACH, &[]).await;
    Ok(true)
}

/// Every session for one agent **on one deck**.
///
/// The deck term is PRD #1105's: without it this tore down another machine's
/// live pane whenever its agent happened to share an id, which is the ordinary
/// case rather than a contrived one.
pub(crate) async fn detach_agent_on(state: &DesktopState, deck: &EndpointIdentity, agent_id: &str) {
    detach_matching(state, |session| {
        session.agent_id == agent_id && session.endpoint.identity() == *deck
    })
    .await;
}

/// Every session on a deck the app no longer observes (PRD #1105).
///
/// # The rule this replaced, and why
///
/// `retarget_selection` called [`detach_all`] whenever the SELECTED deck moved.
/// That was right while attach was process-global: every session belonged to
/// the selected deck by construction, so moving the selection meant every one
/// of them was about to be talking to the wrong daemon.
///
/// A session now names its own deck and resolves its own link, so a selection
/// move says nothing about it — and tearing one down would close a pane the
/// user deliberately opened on another machine, which is the whole feature.
/// What DOES end a session is its deck leaving the observed set: `retain` is
/// about to drop that deck's transport and `retain_watchers` its watcher, so
/// there is nothing left for the stream to ride on or report through.
///
/// That makes this the exact sibling of `EndpointTunnels::retain` and
/// `DesktopState::retain_watchers` — the same set difference, one line earlier
/// so a DETACH frame still has a transport to travel over — and it is why the
/// call site is no longer gated on `moved`: a deck can leave the observed set
/// without the selected deck changing at all.
pub(crate) async fn detach_decks_outside(
    state: &DesktopState,
    observed: &HashSet<EndpointIdentity>,
) {
    detach_matching(state, |session| {
        !observed.contains(&session.endpoint.identity())
    })
    .await;
}

/// Every session on one deck — used where that deck's daemon is being stopped
/// or replaced, which ends its streams whatever the selection says.
pub(crate) async fn detach_deck(state: &DesktopState, endpoint: &Endpoint) {
    let deck = endpoint.identity();
    detach_matching(state, |session| session.endpoint.identity() == deck).await;
}

/// Every session, on every deck. App exit only — see [`detach_decks_outside`]
/// for why a selection change is no longer one of its callers.
pub(crate) async fn detach_all(state: &DesktopState) {
    detach_matching(state, |_| true).await;
}

/// The one teardown loop the four verbs above share.
///
/// Bounded per session, because a detach writes a frame over a transport that
/// may be an `ssh` child on its way out and app exit is behind this.
async fn detach_matching(state: &DesktopState, mut wanted: impl FnMut(&TerminalSession) -> bool) {
    let session_ids = match state.sessions() {
        Ok(sessions) => sessions
            .iter()
            .filter(|(_, session)| wanted(session))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>(),
        Err(_) => return,
    };
    for session_id in session_ids {
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            detach(state, &session_id),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_session_ids_are_stable_and_distinct() {
        assert_eq!(session_id(1), "terminal-0000000000000001");
        assert_ne!(session_id(1), session_id(2));
    }

    #[test]
    fn stream_rejection_notice_is_bounded_sanitized_and_non_ansi() {
        let notice = rejection_notice(b"history-only\n\x1b[31m");
        let notice = String::from_utf8(notice).unwrap();
        assert_eq!(
            notice,
            "\r\n[agent-deck] terminal input rejected: history-only [31m\r\n"
        );
        assert!(!notice.contains('\u{1b}'));
    }

    #[tokio::test]
    async fn detach_is_idempotent_for_unknown_session() {
        let state = DesktopState::default();
        assert!(!detach(&state, "terminal-missing").await.unwrap());
        assert!(!detach(&state, "terminal-missing").await.unwrap());
    }

    /// A lease on a local deck, which holds no child and needs no ssh — enough
    /// to stand in for the one an attach takes (PRD #741 M9).
    #[cfg(unix)]
    async fn fixture_lease() -> Arc<crate::endpoint_tunnels::TunnelLease> {
        use dot_agent_deck::daemon_client::{Endpoint, LocalEndpoint};
        crate::endpoint_tunnels::EndpointTunnels::default()
            .acquire(&Endpoint::Local(LocalEndpoint::at(
                "/tmp/dot-agent-deck-terminal-lease-test.sock",
            )))
            .await
            .expect("a local deck always leases")
    }

    /// A deck to file a fixture session under, named by its socket path.
    #[cfg(unix)]
    fn fixture_deck(path: &str) -> Endpoint {
        use dot_agent_deck::daemon_client::LocalEndpoint;
        Endpoint::Local(LocalEndpoint::at(path))
    }

    #[cfg(unix)]
    fn fixture_session(
        agent_id: &str,
        generation: u64,
        transport: Arc<crate::endpoint_tunnels::TunnelLease>,
    ) -> TerminalSession {
        fixture_session_on(
            fixture_deck("/tmp/dot-agent-deck-terminal-fixture.sock"),
            agent_id,
            generation,
            transport,
        )
    }

    #[cfg(unix)]
    fn fixture_session_on(
        endpoint: Endpoint,
        agent_id: &str,
        generation: u64,
        transport: Arc<crate::endpoint_tunnels::TunnelLease>,
    ) -> TerminalSession {
        let (stream, _peer) = tokio::net::UnixStream::pair().unwrap();
        // PRD #741 M3: the native split then boxed, matching what
        // `AttachConnection::into_split` hands production — a
        // `tokio::io::split` half here would be a fixture whose teardown
        // differs from the real one.
        let (_, writer) = stream.into_split();
        let writer = TransportWriteHalf::new(writer);
        TerminalSession {
            agent_id: agent_id.into(),
            endpoint,
            channel_id: generation as u32,
            generation,
            writer: Arc::new(AsyncMutex::new(writer)),
            // Test seam: no attach happened, so there is no viewer token.
            viewer: None,
            _transport: transport,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn registry_keeps_only_one_session_per_agent() {
        let lease = fixture_lease().await;
        let state = DesktopState::default();
        state
            .insert_unique_session(
                "terminal-1".into(),
                fixture_session("agent-a", 1, Arc::clone(&lease)),
            )
            .unwrap();
        state
            .insert_unique_session(
                "terminal-2".into(),
                fixture_session("agent-a", 2, Arc::clone(&lease)),
            )
            .unwrap();

        let sessions = state.sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(sessions.contains_key("terminal-2"));
    }

    /// A settings document selecting the whole fleet, with one remote row per
    /// host — the state under which two decks are observed at once.
    #[cfg(unix)]
    fn fleet_settings(hosts: &[&str]) -> crate::settings::DesktopSettings {
        fleet_selecting(hosts, crate::settings::Selection::All)
    }

    /// The same document under an explicit selection, so a test can move the
    /// SELECTED deck without changing which decks are observed and vice versa —
    /// the two axes PRD #1105 separated.
    #[cfg(unix)]
    fn fleet_selecting(
        hosts: &[&str],
        selection: crate::settings::Selection,
    ) -> crate::settings::DesktopSettings {
        use crate::settings::{
            DesktopSettings, EndpointId, EndpointSettings, RemoteEndpointSettings,
        };
        use dot_agent_deck::remote_tunnel::{Hostname, RemoteSocketPath};

        let remote = hosts
            .iter()
            .enumerate()
            .map(|(index, host)| {
                let id = EndpointId::parse(&row_id(index)).expect("a valid id");
                let mut row =
                    RemoteEndpointSettings::new(id, Hostname::parse(host).expect("a valid host"));
                row.socket = Some(RemoteSocketPath::parse("/run/deck.sock").expect("a path"));
                row
            })
            .collect();
        DesktopSettings {
            endpoints: Some(EndpointSettings { remote, selection }),
            ..DesktopSettings::default()
        }
    }

    #[cfg(unix)]
    fn row_id(index: usize) -> String {
        format!("deck00000000000{index}")
    }

    /// The endpoint the row at `index` resolves to, read off the `All` form of
    /// the same hosts — where `connectable_endpoints()` is the local deck
    /// followed by each row in order.
    #[cfg(unix)]
    fn row_endpoint(hosts: &[&str], index: usize) -> Endpoint {
        fleet_settings(hosts)
            .connectable_endpoints()
            .into_iter()
            .nth(index + 1)
            .expect("the row is connectable")
    }

    /// PRD #1105 — a deck id from the webview resolves to THAT deck's endpoint,
    /// and to nothing else.
    ///
    /// The control is the second assertion: the two wire ids resolve to
    /// different endpoints, so the first is about the id rather than about the
    /// function answering with the selected deck whatever it is handed — which
    /// is exactly what `trusted_daemon` did and what this replaces.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_deck_id_resolves_to_its_own_observed_endpoint() {
        let settings = fleet_settings(&["build-box.example.com", "laptop.example.com"]);
        crate::dto::apply_settings_selection(&settings);

        let observed = crate::dto::observed_decks();
        assert_eq!(observed.len(), 3, "the local deck and the two rows");
        for endpoint in &observed {
            let wire = crate::dto::deck_wire_id(endpoint);
            let resolved =
                crate::dto::DeckScope::resolve(Some(&wire)).expect("an observed deck resolves");
            assert_eq!(
                resolved.identity(),
                endpoint.identity(),
                "{wire} must resolve to the deck it names"
            );
        }

        let ids: HashSet<String> = observed.iter().map(crate::dto::deck_wire_id).collect();
        assert_eq!(
            ids.len(),
            3,
            "the three decks are distinguishable by wire id"
        );
    }

    /// An id this app is not observing is REFUSED rather than falling back to
    /// the selected deck.
    ///
    /// Falling back is the failure mode worth naming: it would turn a stale or
    /// forged deck id into a silent attach against whatever deck is in force —
    /// the original defect, reachable through the new parameter.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unobserved_deck_id_is_refused_rather_than_falling_back() {
        let settings = fleet_settings(&["build-box.example.com"]);
        crate::dto::apply_settings_selection(&settings);

        let error = crate::dto::DeckScope::resolve(Some("deck-ffffffffffffffff"))
            .expect_err("an unknown deck id must not resolve");
        assert!(error.contains("observing"), "the refusal says why: {error}");

        // `None` is the documented "the selected deck" case, and stays that way.
        let selected =
            crate::dto::DeckScope::resolve(None).expect("no deck id means the selected deck");
        assert_eq!(
            selected.identity(),
            crate::dto::selected_endpoint().identity()
        );
    }

    /// PRD #1105 — two decks' same-id agents are two sessions, and replacing one
    /// leaves the other alone.
    ///
    /// Agent ids are per-daemon monotonic integers, so `planner` on build-box
    /// and `planner` on the local deck are the ordinary case. The registry
    /// evicted by bare agent id, so attaching the second closed the first —
    /// tearing down a pane the user was watching on another machine.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_session_registry_keeps_one_session_per_agent_per_deck() {
        let lease = fixture_lease().await;
        let state = DesktopState::default();
        let deck_a = fixture_deck("/tmp/dot-agent-deck-registry-a.sock");
        let deck_b = fixture_deck("/tmp/dot-agent-deck-registry-b.sock");

        state
            .insert_unique_session(
                "terminal-a1".into(),
                fixture_session_on(deck_a.clone(), "planner", 1, Arc::clone(&lease)),
            )
            .unwrap();
        state
            .insert_unique_session(
                "terminal-b1".into(),
                fixture_session_on(deck_b.clone(), "planner", 2, Arc::clone(&lease)),
            )
            .unwrap();

        {
            let sessions = state.sessions().unwrap();
            assert_eq!(sessions.len(), 2, "one `planner` per deck, both live");
            assert!(sessions.contains_key("terminal-a1"));
            assert!(sessions.contains_key("terminal-b1"));
        }

        // Re-attaching deck A's `planner` replaces deck A's session and only it.
        state
            .insert_unique_session(
                "terminal-a2".into(),
                fixture_session_on(deck_a, "planner", 3, Arc::clone(&lease)),
            )
            .unwrap();
        let sessions = state.sessions().unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(
            !sessions.contains_key("terminal-a1"),
            "deck A's old session went"
        );
        assert!(
            sessions.contains_key("terminal-b1"),
            "deck B's same-id session is untouched"
        );
        assert!(sessions.contains_key("terminal-a2"));
    }

    /// `detach_agent_on` ends one deck's `planner` and leaves the other's.
    #[cfg(unix)]
    #[tokio::test]
    async fn detaching_one_decks_agent_leaves_the_other_decks_namesake() {
        let lease = fixture_lease().await;
        let state = DesktopState::default();
        let deck_a = fixture_deck("/tmp/dot-agent-deck-detach-agent-a.sock");
        let deck_b = fixture_deck("/tmp/dot-agent-deck-detach-agent-b.sock");
        state
            .insert_unique_session(
                "terminal-a".into(),
                fixture_session_on(deck_a.clone(), "planner", 1, Arc::clone(&lease)),
            )
            .unwrap();
        state
            .insert_unique_session(
                "terminal-b".into(),
                fixture_session_on(deck_b, "planner", 2, Arc::clone(&lease)),
            )
            .unwrap();

        detach_agent_on(&state, &deck_a.identity(), "planner").await;

        let sessions = state.sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(sessions.contains_key("terminal-b"));
    }

    /// PRD #1105's `detach_all` decision, as a test rather than a comment: a
    /// deck that is still OBSERVED keeps its terminal when the selection moves,
    /// and a deck that left loses it.
    ///
    /// The selected deck is deliberately not an input here, which is the point.
    /// `retarget_selection` used to detach everything whenever the selected deck
    /// moved; what ends a session now is its deck leaving the set this app
    /// connects to, because that is when its transport and its watcher go.
    #[cfg(unix)]
    #[tokio::test]
    async fn only_the_departed_decks_sessions_are_detached() {
        let lease = fixture_lease().await;
        let state = DesktopState::default();
        let staying = fixture_deck("/tmp/dot-agent-deck-retain-staying.sock");
        let leaving = fixture_deck("/tmp/dot-agent-deck-retain-leaving.sock");
        state
            .insert_unique_session(
                "terminal-staying".into(),
                fixture_session_on(staying.clone(), "planner", 1, Arc::clone(&lease)),
            )
            .unwrap();
        state
            .insert_unique_session(
                "terminal-leaving".into(),
                fixture_session_on(leaving, "planner", 2, Arc::clone(&lease)),
            )
            .unwrap();

        let observed: HashSet<EndpointIdentity> = [staying.identity()].into_iter().collect();
        detach_decks_outside(&state, &observed).await;

        let sessions = state.sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(
            sessions.contains_key("terminal-staying"),
            "a still-observed deck's pane survives a selection move"
        );
    }

    /// `detach_deck` ends one deck's sessions — what Stop/Replace daemon does —
    /// and no other deck's.
    #[cfg(unix)]
    #[tokio::test]
    async fn detaching_one_deck_leaves_every_other_decks_sessions() {
        let lease = fixture_lease().await;
        let state = DesktopState::default();
        let stopped = fixture_deck("/tmp/dot-agent-deck-stop-local.sock");
        let untouched = fixture_deck("/tmp/dot-agent-deck-stop-remote.sock");
        state
            .insert_unique_session(
                "terminal-stopped".into(),
                fixture_session_on(stopped.clone(), "planner", 1, Arc::clone(&lease)),
            )
            .unwrap();
        state
            .insert_unique_session(
                "terminal-untouched".into(),
                fixture_session_on(untouched, "planner", 2, Arc::clone(&lease)),
            )
            .unwrap();

        detach_deck(&state, &stopped).await;

        let sessions = state.sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(sessions.contains_key("terminal-untouched"));
    }

    /// PRD #1105's `detach_all` decision, pinned at the CALL SITE that used to
    /// implement the old one.
    ///
    /// # Why this exists beside `only_the_departed_decks_sessions_are_detached`
    ///
    /// That test drives `detach_decks_outside` directly, so it says the helper
    /// is right and nothing about whether `retarget_selection` calls it.
    /// Restoring the previous rule there — `if moved { detach_all }` — left it
    /// green, which is the "a test written alongside a fix that passes either
    /// way" trap. This one moves the SELECTED deck while the session's deck
    /// stays observed, which is exactly the case the two rules disagree about:
    /// the old one tore the pane down, the new one keeps it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_selection_move_keeps_a_still_observed_decks_terminal() {
        use crate::settings::{EndpointId, Selection};

        let hosts = ["build-box.example.com", "laptop.example.com"];
        let build_box = row_endpoint(&hosts, 0);
        // The user is on build-box alone, with a pane open on one of its agents.
        let only_build_box = fleet_selecting(
            &hosts,
            Selection::One(EndpointId::parse(&row_id(0)).expect("a valid id")),
        );
        let state = DesktopState::default();
        crate::retarget_selection(&state, &only_build_box).await;

        let lease = fixture_lease().await;
        state
            .insert_unique_session(
                "terminal-build-box".into(),
                fixture_session_on(build_box.clone(), "planner", 1, lease),
            )
            .unwrap();

        // They switch to All Decks. The selected deck MOVES — `All` resolves to
        // the local one — and build-box is still observed.
        let whole_fleet = fleet_settings(&hosts);
        let moved = crate::retarget_selection(&state, &whole_fleet).await;

        assert!(
            moved,
            "the selected deck moved from build-box to the local one"
        );
        let sessions = state.sessions().unwrap();
        assert!(
            sessions.contains_key("terminal-build-box"),
            "a pane on a deck the app still observes survives a selection move"
        );
    }

    /// The other half of the same decision: a deck that LEAVES the observed set
    /// loses its sessions, even though the selected deck did not move.
    ///
    /// Under `All` the selection always resolves to the local deck, so removing
    /// a row changes the observed set and nothing else — the case the old
    /// `if moved` gate could not see at all. Its transport is about to be
    /// retained away and its watcher ended, so there is nothing left for the
    /// stream to ride on.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_deck_leaving_the_fleet_loses_its_terminal_without_the_selection_moving() {
        let both = ["build-box.example.com", "laptop.example.com"];
        let build_box = row_endpoint(&both, 0);
        let laptop = row_endpoint(&both, 1);
        let state = DesktopState::default();
        crate::retarget_selection(&state, &fleet_settings(&both)).await;

        let lease = fixture_lease().await;
        state
            .insert_unique_session(
                "terminal-build-box".into(),
                fixture_session_on(build_box, "planner", 1, Arc::clone(&lease)),
            )
            .unwrap();
        state
            .insert_unique_session(
                "terminal-laptop".into(),
                fixture_session_on(laptop, "planner", 2, lease),
            )
            .unwrap();

        // The laptop row is removed from the document.
        let moved = crate::retarget_selection(&state, &fleet_settings(&both[..1])).await;

        assert!(
            !moved,
            "under All the selected deck is the local one either way"
        );
        let sessions = state.sessions().unwrap();
        assert!(
            !sessions.contains_key("terminal-laptop"),
            "the departed deck's session is torn down although the selection did not move"
        );
        assert!(
            sessions.contains_key("terminal-build-box"),
            "and the deck that stayed keeps its pane"
        );
    }

    /// PRD #1105 — a resize reaches the deck the SESSION was attached over,
    /// not whichever deck is selected when it lands.
    ///
    /// Observed through the refusal, which names the address it refused: the
    /// session is on a local socket that does not exist, while the selected
    /// deck is a remote row. Under `trusted_daemon` the refusal named the
    /// remote deck, which is the whole defect — this client's measured grid
    /// went to another machine's same-id agent, and under the daemon's
    /// viewer size policy that reflows its PTY and every other client
    /// watching it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_resize_reaches_the_deck_its_session_was_attached_over() {
        use crate::settings::{EndpointId, Selection};

        let hosts = ["build-box.example.com"];
        crate::dto::apply_settings_selection(&fleet_selecting(
            &hosts,
            Selection::One(EndpointId::parse(&row_id(0)).expect("a valid id")),
        ));
        let selected = crate::dto::selected_endpoint();
        assert!(
            matches!(selected, Endpoint::Remote(_)),
            "the state under test: a REMOTE deck is selected"
        );

        let session_socket = "/tmp/dot-agent-deck-resize-session-deck.sock";
        let lease = fixture_lease().await;
        let state = DesktopState::default();
        state
            .insert_unique_session(
                "terminal-probe".into(),
                fixture_session_on(fixture_deck(session_socket), "planner", 1, lease),
            )
            .unwrap();

        let error = resize(&state, "terminal-probe", 80, 24)
            .await
            .expect_err("that socket does not exist, so the resize cannot land");
        assert!(
            error.contains(session_socket),
            "the resize went to the session's own deck: {error}"
        );
        assert!(
            !error.contains("build-box"),
            "and never to the selected one: {error}"
        );
    }

    /// A live session holds the transport open on its own account, so dropping
    /// every OTHER holder does not release it (PRD #741 M9).
    ///
    /// This is the residual M7 named, pinned as a type rather than argued. The
    /// tunnel map is the only other holder here, and `retain` with an empty set
    /// is exactly what `apply_selection` does when the user picks a different
    /// deck; the session's own lease is what keeps the `ssh` child alive until
    /// the tile that is still streaming over it detaches.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_live_terminal_session_holds_its_own_transport_lease() {
        use dot_agent_deck::daemon_client::{Endpoint, LocalEndpoint};
        let tunnels = crate::endpoint_tunnels::EndpointTunnels::default();
        let endpoint = Endpoint::Local(LocalEndpoint::at(
            "/tmp/dot-agent-deck-terminal-lease-retain.sock",
        ));
        let lease = tunnels.acquire(&endpoint).await.expect("lease");
        let state = DesktopState::default();
        state
            .insert_unique_session(
                "terminal-1".into(),
                fixture_session("agent-a", 1, Arc::clone(&lease)),
            )
            .unwrap();
        // Everything the app itself holds is now gone: the map's handle, and
        // the caller's.
        tunnels.retain(&std::collections::HashSet::new()).await;
        drop(lease);
        assert_eq!(tunnels.held().await, 0, "the map released its handle");

        let held = {
            let sessions = state.sessions().unwrap();
            Arc::strong_count(&sessions["terminal-1"]._transport)
        };
        assert_eq!(
            held, 1,
            "the session is the last holder, so the transport is still alive for it"
        );

        // Detaching is what lets it go.
        assert!(detach(&state, "terminal-1").await.unwrap());
        assert!(state.sessions().unwrap().is_empty());
    }

    // -----------------------------------------------------------------------
    // Issue #1116 — the two await-races the third identity audit found, tested
    // at their CALLERS.
    //
    // # Why these need a scripted daemon and an env var, when the nine tests
    // above needed neither
    //
    // Both defects are a second read of the applied selection on the far side
    // of a daemon round trip. A test that never reaches a daemon cannot put
    // anything between the two reads, so it cannot tell a fixed caller from a
    // broken one — which is exactly how `detach_agent_on`'s own green test
    // proved the helper and said nothing about the caller that misused it.
    //
    // So the local deck's address has to be a socket this test controls, and
    // `DOT_AGENT_DECK_ATTACH_SOCKET` is the only way to move it: the local
    // endpoint is resolved from config and no settings document can name it.
    // Under nextest each test owns its process, which is what makes writing a
    // process-global variable safe here — the same reason the selection tests
    // in this file can write `APPLIED_SELECTION` without a lock, and the
    // reason this crate's five selection-touching tests fail under a plain
    // `cargo test` (one process, threads) while passing under `cargo
    // test-fast`.
    // -----------------------------------------------------------------------

    /// Point the LOCAL deck at `socket` and bind a listener on it, owner-only.
    ///
    /// The 0o600 restatement is `daemon_bridge`'s `bind_trusted` reason
    /// verbatim: the client refuses a socket the ambient umask left group- or
    /// world-accessible, and flipping the process umask around `bind(2)` is the
    /// wrong tool in a shared-process test run.
    #[cfg(unix)]
    fn bind_local_deck(socket: &std::path::Path) -> tokio::net::UnixListener {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: under nextest this test owns its process, so no other thread
        // is reading the environment here. See the module comment above.
        unsafe { std::env::set_var("DOT_AGENT_DECK_ATTACH_SOCKET", socket) };
        let listener = tokio::net::UnixListener::bind(socket).expect("bind the scripted deck");
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
            .expect("restate 0o600 on the socket inode");
        listener
    }

    /// A short socket path in a fresh temp dir. Deliberately short — a Unix
    /// socket path is capped near 100 bytes and a long temp root silently
    /// fails to bind.
    #[cfg(unix)]
    fn scratch_socket(tag: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix(tag)
            .tempdir_in("/tmp")
            .expect("a scratch dir for the socket");
        let socket = dir.path().join("s");
        (dir, socket)
    }

    /// An output channel whose sends go nowhere.
    ///
    /// [`establish`] reads only [`Channel::id`] from it — the frames travel to
    /// the webview from the stream task [`attach`] spawns, which is on the
    /// other side of the split this milestone made. So a channel that drops
    /// what it is handed is not a stub standing in for a tested thing; it is
    /// the whole of what this code path uses.
    #[cfg(unix)]
    fn fixture_channel() -> Channel<Response> {
        Channel::new(|_body: tauri::ipc::InvokeResponseBody| Ok(()))
    }

    /// A `Hello` reply this build classifies as `Connected`, so
    /// `require_compatible()` passes.
    #[cfg(unix)]
    fn matching_hello() -> dot_agent_deck::daemon_protocol::AttachResponse {
        use dot_agent_deck::daemon_protocol::{AttachResponse, PROTOCOL_VERSION};
        let mut reply = AttachResponse::hello(PROTOCOL_VERSION)
            .with_running_agents(dot_agent_deck::daemon_protocol::RunningAgentsSummary::default());
        reply.build_version = Some(dot_agent_deck::build_id::local_build_id());
        reply
    }

    /// Answer one connection with one reply, after `hold` resolves.
    ///
    /// One request per connection is not a simplification — the real daemon's
    /// `handle_connection` reads exactly ONE frame, dispatches it and returns.
    /// `hold` is what lets a test stand inside the window between a request
    /// arriving and its answer, which is where both defects live.
    #[cfg(unix)]
    async fn answer_one(
        listener: &tokio::net::UnixListener,
        reply: dot_agent_deck::daemon_protocol::AttachResponse,
        arrived: Option<tokio::sync::oneshot::Sender<()>>,
        hold: Option<tokio::sync::oneshot::Receiver<()>>,
    ) -> (
        Vec<u8>,
        dot_agent_deck::platform::transport::TransportWriteHalf,
    ) {
        use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP};
        let (stream, _peer) = listener.accept().await.expect("accept one client");
        let (reader, writer) = stream.into_split();
        let mut reader = dot_agent_deck::platform::transport::TransportReadHalf::new(reader);
        let mut writer = TransportWriteHalf::new(writer);
        let (kind, payload) = read_frame(&mut reader)
            .await
            .expect("read the request frame")
            .expect("the client sent a frame");
        assert_eq!(kind, KIND_REQ, "the desktop opens with a request frame");
        if let Some(arrived) = arrived {
            let _ = arrived.send(());
        }
        if let Some(hold) = hold {
            let _ = hold.await;
        }
        let encoded = serde_json::to_vec(&reply).expect("serialize the reply");
        write_frame(&mut writer, KIND_RESP, &encoded)
            .await
            .expect("answer the client");
        (payload, writer)
    }

    /// Issue #1116 BLOCKER 1, at the caller.
    ///
    /// Scenario: two decks each run an agent called `planner` and both have a
    /// live terminal session. Stop Agent is invoked on deck A (the local,
    /// scripted deck); while the daemon is holding the request, the user
    /// changes the deck selection to deck B. When the stop completes, A's
    /// session must be gone and **B's must still be there**.
    ///
    /// # What this fails against, and why the existing green test does not
    /// catch it
    ///
    /// `stop_agent_action` read `selected_endpoint()` a second time, after the
    /// await, to pick the deck whose session to detach. So the daemon stopped
    /// A's `planner` and the cleanup detached **B's** — a terminal closed on a
    /// machine the action never touched. Agent ids are per-daemon monotonic
    /// integers, so two decks minting `planner` is the ordinary case.
    ///
    /// `detaching_one_decks_agent_leaves_the_other_decks_namesake` is green
    /// either way: it proves `detach_agent_on` honours its deck argument, and
    /// the defect was in the argument.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_selection_move_during_a_stop_detaches_the_stopped_decks_session() {
        let (_dir, socket) = scratch_socket("dad-stop-race");
        let listener = bind_local_deck(&socket);

        // A fleet: the local (scripted) deck A plus one remote row B.
        let fleet = fleet_settings(&["build-box.example.com"]);
        crate::dto::apply_settings_selection(&fleet);
        let deck_a = crate::dto::selected_endpoint();
        let deck_b = row_endpoint(&["build-box.example.com"], 0);
        assert_ne!(
            deck_a.identity(),
            deck_b.identity(),
            "the two decks must be distinguishable for this test to mean anything"
        );

        let lease = fixture_lease().await;
        let state = DesktopState::default();
        state
            .insert_unique_session(
                "terminal-a".into(),
                fixture_session_on(deck_a.clone(), "planner", 1, Arc::clone(&lease)),
            )
            .unwrap();
        state
            .insert_unique_session(
                "terminal-b".into(),
                fixture_session_on(deck_b.clone(), "planner", 2, Arc::clone(&lease)),
            )
            .unwrap();

        let (stop_arrived_tx, stop_arrived_rx) = tokio::sync::oneshot::channel();
        let (release_stop_tx, release_stop_rx) = tokio::sync::oneshot::channel();
        let deck = tokio::spawn(async move {
            // The handshake, answered immediately.
            let _hello = answer_one(&listener, matching_hello(), None, None).await;
            // The stop, held open so the test can move the selection while it
            // is in flight. This is the whole point of the fixture.
            let (payload, _writer) = answer_one(
                &listener,
                dot_agent_deck::daemon_protocol::AttachResponse {
                    ok: true,
                    ..Default::default()
                },
                Some(stop_arrived_tx),
                Some(release_stop_rx),
            )
            .await;
            payload
        });

        // `join!` rather than `spawn`: the action borrows `state`, and the two
        // halves have to interleave inside one task anyway — the whole
        // condition is "the selection moves while the stop is in flight".
        let stopping = crate::stop_agent_action(&state, "planner");
        let moving = async {
            stop_arrived_rx
                .await
                .expect("the scripted deck received the stop request");
            // The user picks deck B while the daemon is still holding the stop.
            crate::dto::apply_settings_selection(&fleet_selecting(
                &["build-box.example.com"],
                crate::settings::Selection::One(
                    crate::settings::EndpointId::parse(&row_id(0)).expect("a valid id"),
                ),
            ));
            assert_eq!(
                crate::dto::selected_endpoint().identity(),
                deck_b.identity(),
                "the fixture must actually have moved the selection"
            );
            let _ = release_stop_tx.send(());
        };
        let (stopped, ()) = tokio::join!(stopping, moving);
        stopped.expect("the scripted deck accepted the stop");

        let request = deck.await.expect("the scripted deck must finish");
        let request: serde_json::Value =
            serde_json::from_slice(&request).expect("the request must be JSON");
        assert_eq!(
            request["op"], "stop-agent",
            "the action must have reached the daemon it captured"
        );

        let sessions = state.sessions().unwrap();
        assert!(
            !sessions.contains_key("terminal-a"),
            "the stopped deck's session must be detached"
        );
        assert!(
            sessions.contains_key("terminal-b"),
            "the deck that merely became SELECTED mid-stop keeps its terminal — its \
             agent was never stopped"
        );
    }

    /// Issue #1116 BLOCKER 2, at the caller — the publication boundary.
    ///
    /// Scenario: an attach for the local deck resolves its endpoint,
    /// handshakes and opens its PTY stream. While the daemon is still holding
    /// the attach-stream request, the user removes that deck from the fleet, so
    /// `retarget_selection` invalidates its link, releases its tunnel and ends
    /// its watcher — finding no session, because there is not one yet. The
    /// attach then completes. It must publish **nothing**.
    ///
    /// # What this fails against
    ///
    /// `establish` validated `deck_id` against the observed set once, before a
    /// process-wide gate, and then performed the handshake and the stream
    /// attach with no further check — so the session went into the registry
    /// after the user removed the deck, streaming from a daemon the app is no
    /// longer meant to be talking to and holding the `ssh` child alive through
    /// its own transport lease. The tunnel/link epoch does not close it: the
    /// lease still reaches the caller and `TerminalSession::_transport` keeps
    /// it alive.
    ///
    /// # Why the assertion is on the registry and not on the error
    ///
    /// The refusal's wording is `DeckScope::revalidate`'s business. What the
    /// audit requires is that nothing durable is installed, and an empty
    /// registry says that whatever the message.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_attach_whose_deck_is_removed_mid_handshake_publishes_no_session() {
        let (_dir, socket) = scratch_socket("dad-attach-race");
        let listener = bind_local_deck(&socket);

        let fleet = fleet_settings(&["build-box.example.com"]);
        crate::dto::apply_settings_selection(&fleet);
        let deck_a = crate::dto::selected_endpoint();
        let wire_a = crate::dto::deck_wire_id(&deck_a);

        let state = DesktopState::default();
        let (attach_arrived_tx, attach_arrived_rx) = tokio::sync::oneshot::channel();
        let (release_attach_tx, release_attach_rx) = tokio::sync::oneshot::channel();
        let deck = tokio::spawn(async move {
            let _hello = answer_one(&listener, matching_hello(), None, None).await;
            // The attach stream, held open. `into_split` on the desktop side
            // keeps this connection as the stream, so the reply has to look
            // like a real one.
            let mut reply = dot_agent_deck::daemon_protocol::AttachResponse {
                ok: true,
                ..Default::default()
            };
            reply.viewer = Some("viewer-1".into());
            let (payload, writer) = answer_one(
                &listener,
                reply,
                Some(attach_arrived_tx),
                Some(release_attach_rx),
            )
            .await;
            // Hold the write half so the stream does not EOF, which would let
            // this test pass for the wrong reason.
            (payload, writer)
        });

        let channel = fixture_channel();
        let attaching = establish(&state, Some(wire_a), "planner".into(), &channel, None);
        let removing = async {
            attach_arrived_rx
                .await
                .expect("the scripted deck received the attach-stream request");
            // The user removes the local deck from the fleet by selecting only
            // the remote row. This is the real teardown, not a stand-in: it
            // invalidates links, retains tunnels and ends watchers, and finds
            // no session for the attach that is still in flight.
            let moved = crate::retarget_selection(
                &state,
                &fleet_selecting(
                    &["build-box.example.com"],
                    crate::settings::Selection::One(
                        crate::settings::EndpointId::parse(&row_id(0)).expect("a valid id"),
                    ),
                ),
            )
            .await;
            assert!(moved, "the fixture must actually have moved the deck");
            assert!(
                !crate::dto::deck_is_observed(&deck_a),
                "the deck being attached must genuinely have left the observed set"
            );
            let _ = release_attach_tx.send(());
        };
        let (attached, ()) = tokio::join!(attaching, removing);

        attached.expect_err("an attach for a removed deck must not succeed");
        assert!(
            state.sessions().unwrap().is_empty(),
            "no session may be published for a deck the user removed while the attach \
             was in flight"
        );
        deck.abort();
    }

    /// Issue #1116 BLOCKER 2, at the caller — the queued half.
    ///
    /// Scenario: an attach for deck D is queued behind a slower attach holding
    /// the process-wide attach gate. While it waits, the user removes D. When
    /// the gate opens, the queued attach must refuse **without connecting** —
    /// so the scripted deck accepts no connection at all.
    ///
    /// # Why the assertion is a connection count
    ///
    /// Both the fixed and the unfixed code return `Err` here, so asserting on
    /// the result proves nothing. What separates them is whether a credential
    /// was presented to a host the user had already removed: the unfixed code
    /// opens the handshake connection, and the fixed code never reaches
    /// `DaemonLinks::trusted`. `try_accept` after the fact is the discriminator,
    /// and it needs no message matching.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_attach_queued_behind_the_gate_never_reaches_a_deck_removed_while_it_waited() {
        let (_dir, socket) = scratch_socket("dad-gate-race");
        let listener = bind_local_deck(&socket);

        let fleet = fleet_settings(&["build-box.example.com"]);
        crate::dto::apply_settings_selection(&fleet);
        let deck_a = crate::dto::selected_endpoint();
        let wire_a = crate::dto::deck_wire_id(&deck_a);

        let state = DesktopState::default();
        // Stand in for the slower attach ahead of this one in the queue.
        let ahead = state.attach_gate.lock().await;

        let channel = fixture_channel();
        let attaching = establish(&state, Some(wire_a), "planner".into(), &channel, None);
        let removing = async {
            // The attach is now parked on the gate with its scope captured.
            crate::dto::apply_settings_selection(&fleet_selecting(
                &["build-box.example.com"],
                crate::settings::Selection::One(
                    crate::settings::EndpointId::parse(&row_id(0)).expect("a valid id"),
                ),
            ));
            assert!(
                !crate::dto::deck_is_observed(&deck_a),
                "the queued attach's deck must genuinely have left the observed set"
            );
            drop(ahead);
        };
        let (attached, ()) = tokio::join!(attaching, removing);

        attached.expect_err("an attach for a deck removed while it queued must not succeed");
        // Non-blocking rather than a timeout, and that is a correctness point
        // rather than a saving: `attached` has already resolved, and the
        // unfixed code cannot fail without first connecting — so any connection
        // it opened is already sitting in this listener's backlog. There is
        // nothing left to wait for, and a timeout would only add a way for the
        // mutation check to pass for the wrong reason under load.
        let raw = listener.into_std().expect("take the std listener back");
        raw.set_nonblocking(true).expect("ask for WouldBlock");
        assert!(
            matches!(
                raw.accept(),
                Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock
            ),
            "the refusal must land BEFORE the handshake, so no credential is presented \
             to a host the user has removed"
        );
        assert!(state.sessions().unwrap().is_empty());
    }

    // -----------------------------------------------------------------------
    // PRD #1105 M11 step 4 — the desktop identifies itself and claims focus.
    //
    // Against the PRODUCTION attach server wherever the assertion is about what
    // a daemon recorded (its registry's `focused_client` and each viewer's
    // `client_id`), and against a scripted older daemon where it is about what
    // was never sent.
    // -----------------------------------------------------------------------

    /// One real daemon over a scratch socket, bound by production code.
    #[cfg(unix)]
    struct FocusDeck {
        _dir: tempfile::TempDir,
        socket: std::path::PathBuf,
        endpoint: Endpoint,
        registry: Arc<dot_agent_deck::agent_pty::AgentPtyRegistry>,
        server: tokio::task::JoinHandle<()>,
    }

    #[cfg(unix)]
    impl FocusDeck {
        fn start(tag: &str) -> Self {
            use dot_agent_deck::daemon_client::LocalEndpoint;
            use dot_agent_deck::daemon_protocol::{bind_attach_listener, serve_attach};
            let (dir, socket) = scratch_socket(tag);
            let registry = Arc::new(dot_agent_deck::agent_pty::AgentPtyRegistry::new());
            let listener = bind_attach_listener(&socket).expect("bind the real attach socket");
            let server = {
                let registry = Arc::clone(&registry);
                tokio::spawn(async move {
                    let (events, _) = tokio::sync::broadcast::channel(16);
                    let _ = serve_attach(listener, registry, events).await;
                })
            };
            Self {
                _dir: dir,
                endpoint: Endpoint::Local(LocalEndpoint::at(&socket)),
                socket,
                registry,
                server,
            }
        }

        /// A `cat` under this daemon, which emits nothing and stays alive.
        fn spawn_agent(&self, pane_id: &str) -> String {
            use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_PANE_ID, SpawnOptions};
            self.registry
                .spawn_agent(SpawnOptions {
                    command: Some("cat"),
                    env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string())],
                    ..SpawnOptions::default()
                })
                .expect("spawn a real PTY agent")
        }

        /// The client each of `agent_id`'s viewers attached as.
        fn viewer_clients(&self, agent_id: &str) -> Vec<Option<String>> {
            self.registry
                .viewers_of(agent_id)
                .into_values()
                .map(|viewer| viewer.client_id)
                .collect()
        }
    }

    #[cfg(unix)]
    impl Drop for FocusDeck {
        fn drop(&mut self) {
            self.server.abort();
            self.registry.shutdown_all();
        }
    }

    /// A daemon predating `focus-gained`: it classifies as `Connected`,
    /// advertises `advertised`, and records the op of every request it is sent.
    #[cfg(unix)]
    struct OlderDeck {
        _dir: tempfile::TempDir,
        endpoint: Endpoint,
        ops: Arc<Mutex<Vec<String>>>,
        server: tokio::task::JoinHandle<()>,
    }

    #[cfg(unix)]
    impl OlderDeck {
        fn start(tag: &str, advertised: Option<Vec<&'static str>>) -> Self {
            use dot_agent_deck::daemon_client::LocalEndpoint;
            use dot_agent_deck::daemon_protocol::{AttachResponse, KIND_REQ, KIND_RESP};
            use std::os::unix::fs::PermissionsExt;
            let (dir, socket) = scratch_socket(tag);
            let listener = tokio::net::UnixListener::bind(&socket).expect("bind the older deck");
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
                .expect("restate 0o600 on the socket inode");
            let ops = Arc::new(Mutex::new(Vec::new()));
            let server = {
                let ops = Arc::clone(&ops);
                tokio::spawn(async move {
                    while let Ok((stream, _)) = listener.accept().await {
                        let (reader, writer) = stream.into_split();
                        let mut reader =
                            dot_agent_deck::platform::transport::TransportReadHalf::new(reader);
                        let mut writer = TransportWriteHalf::new(writer);
                        let Ok(Some((KIND_REQ, payload))) = read_frame(&mut reader).await else {
                            continue;
                        };
                        let request: serde_json::Value =
                            serde_json::from_slice(&payload).expect("decode the request");
                        let op = request["op"].as_str().unwrap_or_default().to_string();
                        ops.lock().unwrap().push(op.clone());
                        let reply = if op == "hello" {
                            let mut hello = matching_hello();
                            hello.capabilities = advertised
                                .clone()
                                .map(|caps| caps.into_iter().map(String::from).collect());
                            hello
                        } else {
                            AttachResponse::err(format!(
                                "malformed request: unknown variant `{op}`"
                            ))
                        };
                        let encoded = serde_json::to_vec(&reply).expect("serialize the reply");
                        let _ = write_frame(&mut writer, KIND_RESP, &encoded).await;
                    }
                })
            };
            Self {
                _dir: dir,
                endpoint: Endpoint::Local(LocalEndpoint::at(&socket)),
                ops,
                server,
            }
        }
    }

    #[cfg(unix)]
    impl Drop for OlderDeck {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    /// A session on `deck` that registered a viewer, as a desktop attach does
    /// against a daemon with the PRD #882 size policy.
    #[cfg(unix)]
    fn viewer_session_on(
        deck: &Endpoint,
        agent_id: &str,
        generation: u64,
        lease: Arc<crate::endpoint_tunnels::TunnelLease>,
    ) -> TerminalSession {
        let mut session = fixture_session_on(deck.clone(), agent_id, generation, lease);
        session.viewer = Some(format!("viewer-{generation}"));
        session
    }

    /// Point the selected deck at `deck`'s socket, so [`establish`] with no
    /// deck id attaches there — the production path a pane takes.
    #[cfg(unix)]
    fn select_local_deck(deck: &FocusDeck) {
        // SAFETY: under nextest this test owns its process, so no other thread
        // is reading the environment here. See `bind_local_deck`.
        unsafe { std::env::set_var("DOT_AGENT_DECK_ATTACH_SOCKET", &deck.socket) };
        crate::dto::apply_settings_selection(&crate::settings::DesktopSettings::default());
        assert_eq!(
            crate::dto::selected_endpoint().identity(),
            deck.endpoint.identity(),
            "fixture: the selected deck is the real daemon under test"
        );
    }

    /// Scenario: the desktop attaches a pane on deck A through the production
    /// attach, a second agent's terminal on deck B through B's link, and a
    /// third after every link was dropped and re-established. All three
    /// viewers carry the one client id this process generated, and asking for
    /// that id again returns the same one.
    #[cfg(unix)]
    #[tokio::test]
    async fn every_attach_on_every_deck_names_the_one_desktop_client() {
        let desktop = crate::daemon_bridge::desktop_client_id();
        assert_eq!(desktop, crate::daemon_bridge::desktop_client_id());
        assert!(dot_agent_deck::daemon_protocol::is_valid_client_id(desktop));

        let deck_a = FocusDeck::start("dad-focus-id-a");
        let deck_b = FocusDeck::start("dad-focus-id-b");
        let (agent_a, agent_b) = (deck_a.spawn_agent("pane-a"), deck_b.spawn_agent("pane-b"));
        select_local_deck(&deck_a);
        let state = DesktopState::default();

        let channel = fixture_channel();
        establish(&state, None, agent_a.clone(), &channel, Some((22, 153)))
            .await
            .expect("attach a pane on deck A");
        let link_b = state
            .daemon
            .trusted(&deck_b.endpoint)
            .await
            .expect("link B");
        let _on_b = link_b
            .client
            .attach_as_viewer(&agent_b, Some((30, 100)))
            .await
            .expect("attach on deck B");

        let link_a = state
            .daemon
            .trusted(&deck_a.endpoint)
            .await
            .expect("link A");
        state.daemon.invalidate_all().await;
        let relinked = state
            .daemon
            .trusted(&deck_a.endpoint)
            .await
            .expect("relink A");
        assert!(
            !Arc::ptr_eq(&relinked, &link_a),
            "fixture: a fresh link, so a fresh client handle"
        );
        let _again = relinked
            .client
            .attach_as_viewer(&agent_a, Some((40, 120)))
            .await
            .expect("attach again on deck A");

        let mut clients = deck_a.viewer_clients(&agent_a);
        clients.extend(deck_b.viewer_clients(&agent_b));
        assert_eq!(clients.len(), 3, "one viewer per attach: {clients:?}");
        assert!(
            clients
                .iter()
                .all(|client| client.as_deref() == Some(desktop)),
            "every attach, on every deck and across a re-established link, names the \
             process's one client: {clients:?}"
        );
    }

    /// Scenario: the desktop holds viewers on decks A (two panes) and B, and a
    /// session with no viewer on deck C. The window gains focus: A and B each
    /// record the desktop as their last-focused client, once each, and C is
    /// sent nothing. Another client then claims A, and the window LOSING focus
    /// claims nothing back.
    #[cfg(unix)]
    #[tokio::test]
    async fn gaining_window_focus_claims_focus_on_every_deck_with_a_viewer() {
        use dot_agent_deck::daemon_client::{DaemonClient, generate_client_id};
        let desktop = crate::daemon_bridge::desktop_client_id();
        let (deck_a, deck_b, deck_c) = (
            FocusDeck::start("dad-focus-a"),
            FocusDeck::start("dad-focus-b"),
            FocusDeck::start("dad-focus-c"),
        );
        let lease = fixture_lease().await;
        let state = DesktopState::default();
        for (id, session) in [
            (
                "t-a1",
                viewer_session_on(&deck_a.endpoint, "1", 1, Arc::clone(&lease)),
            ),
            (
                "t-a2",
                viewer_session_on(&deck_a.endpoint, "2", 2, Arc::clone(&lease)),
            ),
            (
                "t-b",
                viewer_session_on(&deck_b.endpoint, "1", 3, Arc::clone(&lease)),
            ),
            (
                "t-c",
                fixture_session_on(deck_c.endpoint.clone(), "1", 4, Arc::clone(&lease)),
            ),
        ] {
            state.insert_unique_session(id.into(), session).unwrap();
        }

        let outcomes = window_focus_changed(&state, true).await;
        assert_eq!(
            outcomes.len(),
            2,
            "one claim per deck with a viewer, however many panes it has: {outcomes:?}"
        );
        let outcomes: HashMap<_, _> = outcomes.into_iter().collect();
        for deck in [&deck_a, &deck_b] {
            assert_eq!(
                outcomes.get(&deck.endpoint.identity()),
                Some(&Ok(FocusReport::Recorded))
            );
            assert_eq!(deck.registry.focused_client().as_deref(), Some(desktop));
        }
        assert_eq!(
            deck_c.registry.focused_client(),
            None,
            "a deck the desktop has no viewer on is not claimed"
        );

        let tui = DaemonClient::new(deck_a.socket.clone()).with_client_id(generate_client_id());
        tui.focus_gained()
            .await
            .expect("another client claims deck A");
        assert!(
            window_focus_changed(&state, false).await.is_empty(),
            "losing focus claims nothing"
        );
        assert_eq!(
            deck_a.registry.focused_client().as_deref(),
            tui.client_id(),
            "and moves nothing"
        );
    }

    /// Scenario: the desktop holds viewers on a current deck and on an older
    /// one — first one advertising no capabilities, then one advertising only
    /// PRD #819's verbs, as every release through v0.40.2 does. The window
    /// gains focus: the current deck records the claim, and the older deck is
    /// sent nothing but the link's handshake.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_deck_that_does_not_advertise_focus_gained_is_never_sent_the_claim() {
        use dot_agent_deck::daemon_protocol::CAP_LIST_PROJECTS;
        let current = FocusDeck::start("dad-focus-cur");
        let lease = fixture_lease().await;
        for advertised in [None, Some(vec![CAP_LIST_PROJECTS])] {
            let older = OlderDeck::start("dad-focus-old", advertised.clone());
            let state = DesktopState::default();
            state
                .insert_unique_session(
                    "t-current".into(),
                    viewer_session_on(&current.endpoint, "1", 1, Arc::clone(&lease)),
                )
                .unwrap();
            state
                .insert_unique_session(
                    "t-older".into(),
                    viewer_session_on(&older.endpoint, "1", 2, Arc::clone(&lease)),
                )
                .unwrap();

            let outcomes: HashMap<_, _> = window_focus_changed(&state, true)
                .await
                .into_iter()
                .collect();
            assert_eq!(
                outcomes.get(&current.endpoint.identity()),
                Some(&Ok(FocusReport::Recorded)),
                "advertised {advertised:?}: {outcomes:?}"
            );
            assert_eq!(
                outcomes.get(&older.endpoint.identity()),
                Some(&Ok(FocusReport::Withheld)),
                "advertised {advertised:?}: {outcomes:?}"
            );
            assert_eq!(
                *older.ops.lock().unwrap(),
                vec!["hello".to_string()],
                "advertised {advertised:?}: the older deck is sent the handshake and nothing else"
            );
        }
    }

    /// Scenario: with the window unfocused, opening a pane claims nothing. The
    /// pane is closed, the window gains focus with no viewer anywhere (so there
    /// is nothing to claim), and the pane is opened again: that attach claims
    /// focus on its deck.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_pane_opened_while_the_window_is_focused_claims_focus_on_its_deck() {
        let desktop = crate::daemon_bridge::desktop_client_id();
        let deck = FocusDeck::start("dad-focus-open");
        let agent = deck.spawn_agent("pane-open");
        select_local_deck(&deck);
        let state = DesktopState::default();
        let channel = fixture_channel();

        let unfocused = establish(&state, None, agent.clone(), &channel, Some((22, 153)))
            .await
            .expect("attach while unfocused");
        assert!(
            unfocused.focus_claim.is_none(),
            "an unfocused window claims nothing"
        );
        assert!(detach(&state, &unfocused.result.session_id).await.unwrap());

        assert!(
            window_focus_changed(&state, true).await.is_empty(),
            "fixture: no viewer anywhere, so focus-in itself claims nothing"
        );
        assert_eq!(deck.registry.focused_client(), None);

        let focused = establish(&state, None, agent, &channel, Some((22, 153)))
            .await
            .expect("attach while focused");
        let claim = focused
            .focus_claim
            .expect("a pane opened in a focused window claims focus on its deck");
        assert_eq!(claim.await.expect("claim task"), Ok(FocusReport::Recorded));
        assert_eq!(deck.registry.focused_client().as_deref(), Some(desktop));
    }
}
