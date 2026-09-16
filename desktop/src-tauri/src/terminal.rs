use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use dot_agent_deck::daemon_client::{Endpoint, EndpointIdentity};
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
    /// It is also the registry's key: [`DesktopState::insert_unique_session`]
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
        }
    }
}

impl DesktopState {
    fn sessions(&self) -> Result<MutexGuard<'_, HashMap<String, TerminalSession>>, String> {
        self.sessions
            .lock()
            .map_err(|_| "desktop terminal session registry lock was poisoned".to_string())
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
    /// on the same deck**.
    ///
    /// The deck half is PRD #1105's cross-deck pane. It read
    /// `existing.agent_id != session.agent_id`, which is a statement about a
    /// name that is unique only within a daemon — so attaching `planner` on
    /// build-box evicted the live `planner` session on the local deck, closing
    /// a pane the user was watching and leaving its webview channel talking to
    /// a torn-down stream. One session per agent per deck is the invariant the
    /// single-deck version was reaching for.
    fn insert_unique_session(
        &self,
        session_id: String,
        session: TerminalSession,
    ) -> Result<(), String> {
        let deck = session.endpoint.identity();
        let mut sessions = self.sessions()?;
        sessions.retain(|_, existing| {
            existing.agent_id != session.agent_id || existing.endpoint.identity() != deck
        });
        sessions.insert(session_id, session);
        Ok(())
    }
}

/// The endpoint one wire deck id names, or the selected deck when the caller
/// named none.
///
/// # Why this exists instead of `trusted_daemon`
///
/// `trusted_daemon` resolves `selected_endpoint()` — the process-global applied
/// selection, read at the instant the call runs. Every terminal verb went
/// through it, so an attach declared for an agent on build-box reached whatever
/// deck happened to be selected when the command was dispatched, and agent ids
/// are per-daemon monotonic integers: it found a `planner` there and streamed
/// it. That is issue
/// [#1116](https://github.com/vfarcic/dot-agent-deck/issues/1116)'s whole shape
/// — identity read from mutable current selection at use time — at the one
/// layer where it decides which machine the bytes come from.
///
/// # The resolution is against the OBSERVED set, and that is a security
/// boundary rather than a lookup detail
///
/// A deck id from the webview is untrusted input. Matching it against
/// `observed_decks()` means the only endpoints reachable are the ones the
/// applied settings document already tells this app to connect to, so a
/// malformed or stale id yields a refusal rather than a connection: there is no
/// path here by which a value from the webview becomes an address.
///
/// `None` keeps the previous behaviour for a caller that names no deck. Nothing
/// in this tree sends one today — the webview always names the deck it is
/// attaching to — and it is kept because the parameter is optional on the IPC
/// boundary, so absence must mean something defined rather than an error the
/// user cannot act on.
fn endpoint_for_deck(deck_id: Option<&str>) -> Result<Endpoint, String> {
    let Some(deck_id) = deck_id else {
        return Ok(crate::dto::selected_endpoint());
    };
    crate::dto::observed_decks()
        .into_iter()
        .find(|endpoint| crate::dto::deck_wire_id(endpoint) == deck_id)
        .ok_or_else(|| {
            format!(
                "that deck is not one this app is observing: {}",
                safe_message(deck_id)
            )
        })
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

pub(crate) async fn attach(
    app: &AppHandle,
    state: &DesktopState,
    // PRD #1105 — which deck's `agent_id` this is. `None` means the selected
    // deck; see [`endpoint_for_deck`] for why that fallback exists and why the
    // resolution is against the observed set.
    deck_id: Option<String>,
    agent_id: String,
    on_output: Channel<Response>,
    // PRD #882 — the geometry this tile can draw the agent at, measured by
    // `FitAddon` in the webview. Declaring it registers the tile as a viewer, so
    // the daemon sizes the agent to the smallest pane among every client
    // watching it and tells this tile whenever that changes.
    viewport: Option<(u16, u16)>,
) -> Result<TerminalAttachResult, String> {
    validate_agent_id(&agent_id)?;
    // Resolved BEFORE the gate, so a bad deck id is refused without queueing
    // behind somebody else's handshake.
    let endpoint = endpoint_for_deck(deck_id.as_deref())?;
    let deck = endpoint.identity();
    let _attach_guard = state.attach_gate.lock().await;
    let channel_id = on_output.id();
    // The deck is part of the reuse check for the same reason it is part of the
    // registry key: `(agent_id, channel_id)` alone would answer "you already
    // have this" for another deck's namesake.
    if let Some((session_id, session)) = state.sessions()?.iter().find(|(_, session)| {
        session.agent_id == agent_id
            && session.channel_id == channel_id
            && session.endpoint.identity() == deck
    }) {
        return Ok(TerminalAttachResult {
            session_id: session_id.clone(),
            agent_id,
            generation: session.generation,
            reused: true,
            // A reused session keeps whatever geometry it already has; the
            // frontend's grid is already sized to it and no attach happened.
            applied_rows: None,
            applied_cols: None,
        });
    }
    // This deck's earlier session for this agent, never another deck's.
    detach_agent_on(state, &deck, &agent_id).await;
    let daemon = state.daemon.trusted(&endpoint).await?;
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
    let (mut reader, writer) = connection.into_split();
    let generation = state.next_generation.fetch_add(1, Ordering::Relaxed);
    let session_id = session_id(generation);
    state.insert_unique_session(
        session_id.clone(),
        TerminalSession {
            agent_id: agent_id.clone(),
            endpoint,
            channel_id,
            generation,
            writer: Arc::new(AsyncMutex::new(writer)),
            viewer,
            _transport: transport,
        },
    )?;

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

    Ok(TerminalAttachResult {
        session_id,
        agent_id,
        generation,
        reused: false,
        // PRD #882: the geometry in force at attach time, resolved under the
        // same daemon lock as the scrollback replay that is about to arrive on
        // the channel. The frontend sizes its grid from this before writing
        // those bytes, so the replay is parsed at the geometry it was written
        // at rather than at whatever the tile happened to measure.
        applied_rows: applied.map(|(rows, _)| rows),
        applied_cols: applied.map(|(_, cols)| cols),
    })
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
/// same-id agent. Under the daemon's smallest-viewer policy that is not a
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
            let resolved = endpoint_for_deck(Some(&wire)).expect("an observed deck resolves");
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

        let error = endpoint_for_deck(Some("deck-ffffffffffffffff"))
            .expect_err("an unknown deck id must not resolve");
        assert!(error.contains("observing"), "the refusal says why: {error}");

        // `None` is the documented "the selected deck" case, and stays that way.
        let selected = endpoint_for_deck(None).expect("no deck id means the selected deck");
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
    /// smallest-viewer policy that reflows its PTY and every other client
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
}
