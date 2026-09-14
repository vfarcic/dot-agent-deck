use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use dot_agent_deck::daemon_client::EndpointIdentity;
use dot_agent_deck::daemon_protocol::{
    KIND_DETACH, KIND_GEOMETRY, KIND_STREAM_END, KIND_STREAM_IN, KIND_STREAM_OUT,
    KIND_STREAM_REJECT, parse_geometry_frame, read_frame, write_frame,
};
use dot_agent_deck::platform::transport::TransportWriteHalf;
use tauri::ipc::{Channel, Response};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Mutex as AsyncMutex;

use crate::daemon_bridge::{DaemonLinks, trusted_daemon};
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

    fn insert_unique_session(
        &self,
        session_id: String,
        session: TerminalSession,
    ) -> Result<(), String> {
        let mut sessions = self.sessions()?;
        sessions.retain(|_, existing| existing.agent_id != session.agent_id);
        sessions.insert(session_id, session);
        Ok(())
    }
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
    agent_id: String,
    on_output: Channel<Response>,
    // PRD #882 — the geometry this tile can draw the agent at, measured by
    // `FitAddon` in the webview. Declaring it registers the tile as a viewer, so
    // the daemon sizes the agent to the smallest pane among every client
    // watching it and tells this tile whenever that changes.
    viewport: Option<(u16, u16)>,
) -> Result<TerminalAttachResult, String> {
    validate_agent_id(&agent_id)?;
    let _attach_guard = state.attach_gate.lock().await;
    let channel_id = on_output.id();
    if let Some((session_id, session)) = state
        .sessions()?
        .iter()
        .find(|(_, session)| session.agent_id == agent_id && session.channel_id == channel_id)
    {
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
    detach_agent(state, &agent_id).await;
    let daemon = trusted_daemon(&state.daemon).await?;
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

pub(crate) async fn resize(
    state: &DesktopState,
    session_id: &str,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let (rows, cols) = validate_dimensions(rows, cols)?;
    let (agent_id, viewer) = state
        .sessions()?
        .get(session_id)
        .map(|session| (session.agent_id.clone(), session.viewer.clone()))
        .ok_or_else(|| format!("terminal session not found: {}", safe_message(session_id)))?;
    let daemon = trusted_daemon(&state.daemon).await?;
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

pub(crate) async fn detach_agent(state: &DesktopState, agent_id: &str) {
    let session_ids = match state.sessions() {
        Ok(sessions) => sessions
            .iter()
            .filter(|(_, session)| session.agent_id == agent_id)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>(),
        Err(_) => return,
    };
    for session_id in session_ids {
        let _ = detach(state, &session_id).await;
    }
}

pub(crate) async fn detach_all(state: &DesktopState) {
    let session_ids = match state.sessions() {
        Ok(sessions) => sessions.keys().cloned().collect::<Vec<_>>(),
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

    #[cfg(unix)]
    fn fixture_session(
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
