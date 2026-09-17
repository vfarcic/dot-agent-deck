use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use dot_agent_deck::daemon_attach::{
    DAEMON_START_POLL_TIMEOUT, ensure_daemon_running, spawn_daemon_serve_detached_with_exe,
};
use dot_agent_deck::daemon_client::{DaemonClient, Endpoint, EndpointIdentity, issue_command};
#[cfg(test)]
use dot_agent_deck::daemon_protocol::RunningAgentsSummary;
use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, ContractComparison, PROTOCOL_VERSION, compare_contract_breaks,
};
use dot_agent_deck::platform::ipc::IpcStream;
use tokio::sync::Mutex as AsyncMutex;

use crate::agent_view::AgentView;
use crate::dto::{
    BootstrapOptions, ConnectionStatus, DesktopConnection, DesktopSnapshot, deck_path_text,
    deck_wire_id, disconnected_snapshot, map_agent, observed_fleet, observed_fleet_decks,
    safe_message, selected_endpoint, selection_fields, unconfigured_fleet,
};
use crate::endpoint_tunnels::{EndpointTunnels, TunnelLease};

const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub(crate) struct HandshakeInfo {
    pub(crate) status: ConnectionStatus,
    pub(crate) error: Option<String>,
    pub(crate) server_protocol_version: Option<u32>,
    pub(crate) daemon_build_version: Option<String>,
    pub(crate) daemon_version: Option<String>,
    pub(crate) running_agent_count: Option<usize>,
    /// The protocol agreed and something ABOVE the wire did not, so an override
    /// is legitimate. Never set when the wire itself is incompatible.
    ///
    /// **Named for what used to set it.** Until issue #801 that was a build-stamp
    /// difference the two builds' release digits did not excuse. A build stamp
    /// now sets this — and refuses — never; what sets it is a declared contract
    /// break ([`contract_refusal`]). The name is kept because it is the key the
    /// webview switches `Connect anyway` on, and renaming it across the bridge
    /// would buy nothing this doc comment does not.
    pub(crate) build_stamp_mismatch_only: bool,
    /// Why the project-aware surfaces are unavailable against this daemon, or
    /// `None` when they are available (PRD #741 M8).
    ///
    /// Derived from what the `Hello` reply **advertised**, never from a version
    /// digit or a build stamp — see [`project_actions_reason`].
    pub(crate) project_actions_reason: Option<String>,
}

/// An established link to one deck: the handshake that classified it, and a
/// client that will issue requests against it.
///
/// **PRD #741 M4(a): this is now HELD rather than rebuilt per call**, which is
/// the whole milestone. Before it, every `trusted_daemon()` ran the trust
/// `stat`, opened a connection, exchanged a `Hello`, dropped that connection and
/// minted a throwaway [`DaemonClient`] — so a `get_snapshot()` cost **two**
/// connections and the capability set captured from the handshake was discarded
/// a few microseconds later. See [`DaemonLinks`] for what is held and for what
/// is deliberately NOT held.
#[derive(Debug)]
pub(crate) struct TrustedDaemon {
    /// Shared, so the capability cache inside it (`Arc<Mutex<…>>` since PRD
    /// #819 M5) actually survives from one call to the next instead of being
    /// seeded and thrown away on every one.
    pub(crate) client: Arc<DaemonClient>,
    connection: DesktopConnection,
    /// The transport this link was established over (PRD #741 M7).
    ///
    /// Held, not borrowed, and that is rule 2 of `endpoint_tunnels`: a lease is
    /// an `Arc`, so the `ssh` child survives being dropped from the tunnel map
    /// until its last holder lets go. Without this field a selection change
    /// would tear the transport out from under a link that is still classified
    /// `Connected` and still serving requests — the handshake would describe a
    /// deck whose tunnel had already gone.
    ///
    /// Read only by [`Self::transport`], and otherwise held for its `Drop`
    /// order — which is why it keeps the leading underscore.
    _transport: Arc<TunnelLease>,
    /// When the handshake behind [`Self::connection`] was taken. Read by
    /// [`DaemonLinks::trusted`] against [`HANDSHAKE_REVALIDATE_INTERVAL`].
    established: Instant,
}

impl TrustedDaemon {
    pub(crate) fn require_compatible(&self) -> Result<(), String> {
        if self.connection.status == ConnectionStatus::Connected {
            Ok(())
        } else {
            Err(self
                .connection
                .error
                .clone()
                .unwrap_or_else(|| "the deck is not protocol-compatible".into()))
        }
    }

    /// The classified handshake, for a caller assembling a snapshot from it.
    pub(crate) fn connection(&self) -> DesktopConnection {
        self.connection.clone()
    }

    /// A lease on the transport this link was established over (PRD #741 M9).
    ///
    /// # Why a caller may need one of its own
    ///
    /// A link is re-established every [`HANDSHAKE_REVALIDATE_INTERVAL`] and
    /// dropped outright on a selection change, so "the link held the tunnel
    /// while I was borrowing it" is an argument about timing rather than a
    /// guarantee. Anything that opens a **long-lived stream** over the
    /// transport — a terminal attach, whose write half outlives the request
    /// that created it by the life of the tile — has to hold the lease itself,
    /// or a selection change tears the `ssh` child out from under a session
    /// that is still streaming.
    ///
    /// PRD #741 M7 named that as a residual and left the argument standing; M9
    /// is the milestone that puts several tiles on a remote deck, so it becomes
    /// a type here. `terminal::TerminalSession` is the one holder.
    pub(crate) fn transport(&self) -> Arc<TunnelLease> {
        Arc::clone(&self._transport)
    }

    fn is_fresh(&self, now: Instant) -> bool {
        now.duration_since(self.established) < HANDSHAKE_REVALIDATE_INTERVAL
    }
}

/// How long a held handshake is trusted before it is taken again.
///
/// **A backstop, not the primary mechanism.** What actually detects a daemon
/// being replaced is the desktop's own persistent event subscription: a daemon
/// cannot be replaced without the old process dying, and its death breaks that
/// socket, which drops the watcher into its reconnect leg — and that leg calls
/// [`DaemonLinks::invalidate_all`] before it re-subscribes. Every route to a
/// replaced daemon passes through that death, whether the replacement came from
/// this app's Replace button, a `dot-agent-deck daemon restart` in a terminal,
/// or a crash plus some other client's lazy-spawn.
///
/// This interval exists because that argument depends on the watcher running
/// and on every invalidation site being wired, and neither is something a
/// reader should have to take on trust. With it, the strongest claim needed is
/// the bounded one: **the classification is never more than five seconds old**.
///
/// Five seconds against the watcher's 150 ms coalesce floor means the handshake
/// stops being ~6.7 connections/second and becomes 0.2 — a 97% cut — and it can
/// hold stale only fields that change when the daemon is replaced. The two that
/// would have been user-visible are both excluded rather than argued about:
/// `running_agent_count` is refreshed from the `ListAgents` reply that
/// `get_snapshot` already fetches (see there), and a **refused** classification
/// is not held at all (see [`DaemonLinks::trusted`]).
pub(crate) const HANDSHAKE_REVALIDATE_INTERVAL: Duration = Duration::from_secs(5);

/// The established links, keyed by endpoint — PRD #741 M4(a), held in
/// `DesktopState`.
///
/// # What is held, and what a connection here is NOT
///
/// **Not a socket.** The obvious reading of "hold the connection open" is a
/// request socket kept alive across calls, and that is not reachable from the
/// client at all: the daemon's `handle_connection` reads exactly ONE frame,
/// dispatches it, writes the reply and returns, so the connection closes.
/// Measured rather than read off the source — a second `KIND_REQ` on an
/// already-answered connection gets `EPIPE` on the write and EOF on the read,
/// with the daemon's own concurrent-client gauge already back at zero. A held
/// request socket therefore needs `handle_connection` to LOOP, which is a
/// same-wire/different-meaning protocol change and out of this milestone.
///
/// **So what is held is the handshake**, which is the part that was being
/// re-done per request batch rather than per connection — the thing a handshake
/// is for. Concretely, per endpoint: the classified [`DesktopConnection`], the
/// capability set captured from the `Hello` reply, and the trust `stat` that
/// precedes both. A `get_snapshot()` goes from **two** connections to **one**;
/// measured at ten refreshes in
/// `tests::ten_refreshes_cost_one_handshake_and_ten_listings`, which reports 11
/// connections against the 20 the same test reports with reuse disabled.
///
/// And only a **connected** classification is held — see [`Self::trusted`].
///
/// # Why keyed by endpoint when only one is selectable
///
/// PRD #741 M9 makes the endpoint a user choice and #742 shows several decks at
/// once. Keying now costs one `HashMap` and means neither of those is a
/// retrofit of this type; a single held link would have to be torn out.
///
/// # The multiplexing decision, which is issue #745's deliverable
///
/// **Decided: the two streaming structs are NOT multiplexed, and ssh is the
/// reason that is affordable.** `EventSubscription` and `AttachConnection` keep
/// one connection each, so a desktop showing N terminal tiles holds N+1
/// long-lived connections plus one short-lived one per refresh. Four things
/// decided it:
///
/// 1. **The population of one, which PRD #742 M3 makes a population of N — per
///    DECK, not per stream.** There is one `EventSubscription` per *observed
///    deck* (`DesktopState::start_watcher_once_for` guarantees one watcher per
///    deck, and `retain_watchers` ends a departed deck's), and each one is a
///    connection to a **different daemon**. Multiplexing is a thing you do to
///    several streams sharing one peer, so a set of one-per-peer is exactly as
///    un-muxable at N decks as it was at one; what grows is the connection count
///    this doc comment already prices below, not the muxing case.
///
/// 2. **Muxing attach streams is a WIRE change, not a refactor.** The frame
///    header is five bytes: one kind byte and a four-byte big-endian length
///    (`daemon_protocol::read_frame`). There is no stream id, so two attaches
///    sharing a socket could not be told apart. Adding one means a new header
///    shape, a `PROTOCOL_VERSION` bump and a compatibility break for every
///    client/daemon pair — categorically larger than the whole of M4(a).
///
/// 3. **It would need the daemon-side request loop anyway.** The same
///    `handle_connection` fact above applies: a connection is dedicated to one
///    attach for the life of that attach.
///
/// 4. **And the remote case, which is the one this PRD is for, already has
///    multiplexing underneath.** Under DECISION 1A a remote deck is reached
///    through `ssh -L` forwarding a Unix socket. Each connection to a forwarded
///    socket opens a new SSH *channel* on the existing SSH transport, not a new
///    TCP connection — the SSH protocol multiplexes channels over one connection
///    by construction. So N attach streams cost N channels over one TCP session
///    and one authentication, not N round trips of setup. Re-implementing
///    multiplexing at the attach-protocol layer would buy a channel count and
///    pay for it with a wire break.
///
/// **What that costs, stated rather than waved at.** A fleet view showing many
/// agents at once (#742) holds one connection per visible tile, and the daemon
/// holds one `handle_connection` task per tile with it. That is a file-descriptor
/// and task cost linear in tiles on both sides, and it is the number to watch if
/// #742 ever shows tens of live terminals. It is not a *latency* cost, which is
/// what M4 was opened about: the streams are established once and then carry
/// frames, so nothing about them is paid per refresh.
///
/// # Concurrency — one gate per deck, and the map lock held across nothing
///
/// **This inverted at PRD #742 M3, and the old comment said so in advance.** It
/// used to be one async mutex over the whole map, *held across establishment*,
/// and its own justification ended "a deck that is not answering queues the
/// other callers behind one connect timeout rather than giving each its own,
/// which is also the better of the two". That is true while the queued callers
/// are other users of the *same* deck. At N decks the callers queued behind an
/// unreachable deck's connect are **other decks**, and the desktop's whole fleet
/// stops updating because one machine is down — PRD #742 success criterion 2,
/// and the one risk the PRD says can make it fail its own headline.
///
/// So establishment is serialised **per deck** instead, by a gate keyed the same
/// way the map is, and the map's own lock is now held only for the two lookups
/// around it. Both halves of the original property survive:
///
/// - *Concurrent first-uses of ONE deck still collapse into ONE handshake.* The
///   second caller waits on that deck's gate, and by the time it gets in the
///   first caller has already published its link, so it takes the cache-hit path
///   rather than handshaking again.
/// - *Two callers on DIFFERENT decks never wait for each other*, because they
///   hold different gates and neither holds the map across a connect.
///
/// Pinned by [`tests::an_unresponsive_deck_does_not_queue_another_decks_handshake`]
/// and [`tests::a_held_link_is_reused_and_costs_one_handshake`] respectively.
///
/// The gate map is a **`std::sync::Mutex`** and is never held across an `await`:
/// it hands out an `Arc` and is released. That is what keeps `clippy::await_holding_lock`
/// honest here rather than allowed.
///
/// # Publishing — the half the split gave up, put back (PRD #742 M8)
///
/// The whole-map lock did a second job nobody replaced: it excluded
/// [`Self::invalidate_all`] *for the duration of an establishment*. The per-deck
/// gate does not — `invalidate_all` takes the map lock and nothing else — and a
/// deck being established has no entry to clear, so an invalidation landing
/// mid-handshake is followed by the establishment publishing the very link it
/// meant to forget. [`EndpointTunnels::acquire`] has the same shape and the same
/// remedy; `crate::generation` is the shared mechanism and carries the
/// reasoning.
///
/// **What the compare covers, stated exactly rather than generally.**
/// [`Self::trusted`] reads the epoch before establishing and compares it under
/// the same map lock it inserts under — so against `invalidate_all`, which bumps
/// under that lock, the two are serialised and the result is exact: either the
/// insert lands first and the `clear()` removes it, or the bump lands first and
/// the insert is refused. `EndpointTunnels`' own three teardowns bump under the
/// *tunnel* map's lock instead, so against those this compare is conservative
/// rather than atomic. It does not need to be: this map's teardown is
/// `invalidate_all`, and a link that outlives a tunnel `release` was already the
/// designed behaviour — [`TrustedDaemon`] holds its own lease exactly so a
/// classified link cannot describe a deck whose transport has gone. The path F1
/// is about is covered either way, because `retarget_selection` calls
/// `invalidate_all` and `EndpointTunnels::retain` in consecutive statements.
pub(crate) struct DaemonLinks {
    links: AsyncMutex<HashMap<EndpointIdentity, Arc<TrustedDaemon>>>,
    /// One establishment gate per deck — see this type's *Concurrency* section.
    ///
    /// Empty of state by design: the gate carries nothing, it only says whose
    /// turn it is to handshake for that deck. Which is why [`Self::gate`] may
    /// drop any gate nobody is holding without coordinating with anyone.
    gates: std::sync::Mutex<HashMap<EndpointIdentity, Arc<AsyncMutex<()>>>>,
    /// The live transports (PRD #741 M7).
    ///
    /// **This is a field of `DesktopState`, shared here by `Arc` — it is not a
    /// field of [`TrustedDaemon`], and that distinction is the whole of M5's
    /// deferred design decision.** A `TrustedDaemon` is rebuilt every
    /// [`HANDSHAKE_REVALIDATE_INTERVAL`], so a tunnel owned by one would
    /// re-authenticate ssh every five seconds; the map outlives every link in
    /// it. Establishment is simply where a transport happens to be needed, so
    /// this type holds a handle to the map rather than owning the tunnels'
    /// lifecycle — the teardown triggers are commands, and they reach the same
    /// map through `DesktopState::tunnels`.
    tunnels: Arc<EndpointTunnels>,
    /// Total handshakes performed, for tests and for the milestone's
    /// before/after measurement. Never read by production logic.
    handshakes: AtomicUsize,
}

impl Default for DaemonLinks {
    fn default() -> Self {
        Self {
            links: AsyncMutex::new(HashMap::new()),
            gates: std::sync::Mutex::new(HashMap::new()),
            tunnels: Arc::new(EndpointTunnels::default()),
            handshakes: AtomicUsize::new(0),
        }
    }
}

impl DaemonLinks {
    /// The link for `endpoint`, establishing one if there is none or the held
    /// one has aged past [`HANDSHAKE_REVALIDATE_INTERVAL`].
    ///
    /// A failed establishment removes any held link first, so a stale
    /// classification is never returned after the deck behind it stopped
    /// answering.
    ///
    /// PRD #742 M3: serialised **per deck** rather than over the whole map — see
    /// this type's *Concurrency* section for what that keeps and what it fixes.
    pub(crate) async fn trusted(&self, endpoint: &Endpoint) -> Result<Arc<TrustedDaemon>, String> {
        let key = endpoint.identity();
        // PRD #742 M8: read before the gate and before the lookup, so every
        // teardown that could make this deck unwanted lands inside the window
        // the publish below compares across. See this type's *Publishing*
        // section for which of them that compare is exact against.
        let wanted_at = self.tunnels.generation().current();
        // Held across the establishment below, and it is this deck's alone.
        let gate = self.gate(&key);
        let _establishing = gate.lock().await;
        {
            let mut links = self.links.lock().await;
            if let Some(held) = links.get(&key)
                && held.is_fresh(Instant::now())
            {
                return Ok(Arc::clone(held));
            }
            // Removed BEFORE the handshake and while nothing else can be
            // establishing for this deck, so a stale classification is never
            // readable during the window in which it is being replaced.
            links.remove(&key);
        }
        let established = Arc::new(establish(endpoint, &self.tunnels).await?);
        self.handshakes.fetch_add(1, Ordering::Relaxed);
        let mut links = self.links.lock().await;
        // **Only a CONNECTED classification is held.** A refusal is the one
        // verdict you want re-checked rather than cached, and there is a
        // user-visible reason as well as a principled one: on the incompatible
        // path the snapshot's `running_agent_count` comes from the handshake
        // (there is no `ListAgents` to read it off), and it is what gates the
        // **Replace daemon** button — the webview shows it only once the old
        // daemon reports zero live agents. Holding that number would make the
        // button appear up to `HANDSHAKE_REVALIDATE_INTERVAL` late instead of
        // within the watcher's 1 s retry, which is exactly the kind of
        // user-visible change PRD #741's M1–M4 are not supposed to make.
        //
        // Nothing is lost. The refusal paths refresh at the watcher's 1 s
        // `WATCH_RETRY_DELAY`, not at the 150 ms coalesce floor, so they were
        // never what this milestone was about; the connected path is the one
        // running 6.667 times a second and it is held in full.
        //
        // PRD #742 M8: and only while the deck is still wanted. `invalidate_all`
        // and `EndpointTunnels`' three teardowns bump the epoch read on the way
        // in; a link published behind one of those would hold a `DaemonClient`,
        // its captured capability set and — through `establish` — a lease on an
        // `ssh` child, for a deck nothing observes. Returning the link without
        // holding it is the right fallback: the caller gets its answer and
        // everything it stands on dies with the request.
        if established.connection.status == ConnectionStatus::Connected
            && self.tunnels.generation().current() == wanted_at
        {
            links.insert(key, Arc::clone(&established));
        }
        Ok(established)
    }

    /// This deck's establishment gate, minting one if it has none.
    ///
    /// A `std::sync::Mutex` held for exactly this lookup and released before the
    /// caller awaits on what it returns — the gate is the thing awaited, never
    /// the map that stores it.
    ///
    /// **It also forgets gates nobody is holding**, which is what keeps this map
    /// bounded by the decks currently in play rather than by every address a
    /// user has ever typed into the endpoints panel (each edit mints a new
    /// [`EndpointIdentity`]). Safe without coordinating with anyone, and for a
    /// reason that is a property rather than a hope: a gate whose `Arc` strong
    /// count is 1 is held by this map alone, so no caller is waiting on it or
    /// inside it — and no caller can be *taking* a clone concurrently, because
    /// cloning happens here, under this same lock.
    fn gate(&self, key: &EndpointIdentity) -> Arc<AsyncMutex<()>> {
        let mut gates = self
            .gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        gates.retain(|held, gate| held == key || Arc::strong_count(gate) > 1);
        Arc::clone(gates.entry(key.clone()).or_default())
    }

    /// The shared transport map, for `DesktopState` and for the commands that
    /// tear a tunnel down (PRD #741 M7).
    pub(crate) fn tunnels(&self) -> Arc<EndpointTunnels> {
        Arc::clone(&self.tunnels)
    }

    /// Forget the link for `endpoint`, so the next [`Self::trusted`] handshakes
    /// again.
    pub(crate) async fn invalidate(&self, endpoint: &Endpoint) {
        self.links.lock().await.remove(&endpoint.identity());
    }

    /// Forget every link. Used where the reason to distrust the held state is
    /// not specific to one deck — the watcher losing its event stream, and the
    /// in-app build-mismatch allowance, whose entire effect is that the
    /// handshake must be classified again.
    ///
    /// **It also bumps the live set's epoch** (PRD #742 M8), which is the half a
    /// `clear()` cannot do: a deck whose establishment is in flight has no entry
    /// to clear, so without this it would publish its link *after* the
    /// invalidation that was meant to forget it. The epoch lives on
    /// [`EndpointTunnels`] and is shared with it — see that type's field comment
    /// for why one counter serves both maps.
    pub(crate) async fn invalidate_all(&self) {
        let mut links = self.links.lock().await;
        self.tunnels.generation().bump();
        links.clear();
    }

    /// How many links this map holds. Test-only, like [`Self::handshake_count`]:
    /// PRD #742 M8 needs to assert that an establishment did NOT publish, and
    /// "the map is empty" is the direct statement of that, where a second
    /// `trusted()` re-handshaking is an inference from it.
    #[cfg(test)]
    pub(crate) async fn held(&self) -> usize {
        self.links.lock().await.len()
    }

    /// How many handshakes have been performed since this store was created.
    ///
    /// Test-only, and deliberately: it is how the reuse tests assert that a
    /// held link did not re-handshake, and no production path has a reason to
    /// ask. The counter itself is unconditional so the field's cost is the same
    /// in both builds.
    #[cfg(test)]
    pub(crate) fn handshake_count(&self) -> usize {
        self.handshakes.load(Ordering::Relaxed)
    }
}

/// How many break names a refusal sentence prints before it starts counting.
///
/// Four, because the sentence is read on a connection banner rather than in a
/// log: a build is normally one or two breaks behind, and a list long enough to
/// need scrolling has stopped telling the reader anything the count does not.
const MAX_NAMED_BREAKS: usize = 4;

/// The break names for one side of a divergence, bounded and charset-checked.
///
/// # Why a peer's entries are not rendered as they arrive
///
/// One of the two lists is peer-supplied. `this_build_lacks` is what the deck
/// declared and this build does not, so every string in it came off the wire —
/// and the connection banner scrubs its sentence with [`safe_message`], which
/// removes general category `Cc` and lets `Cf`, the **bidi** controls, through.
/// That is PRD #741 final audit F4's finding about `build_version`, which is
/// rendered into the same sentence; a new unvalidated wire string beside it
/// would re-open exactly that. A hostile or merely broken daemon can also send a
/// list of any length the 16 MiB frame cap allows, which is a banner nobody can
/// read.
///
/// So an entry is printed only when it matches the shape
/// [`dot_agent_deck::daemon_protocol::CONTRACT_BREAKS`] declares — ASCII digits,
/// lower-case letters and `-`, which cannot hold a control or a bidi byte — and
/// at most [`MAX_NAMED_BREAKS`] of them are printed. Everything else is reported
/// as a count, which is the honest thing to say about a name this app is not
/// willing to show.
///
/// **Applied to BOTH lists although only one needs it.** `peer_lacks` is
/// `CONTRACT_BREAKS.difference(peer)`, so it is a subset of this build's own
/// compiled-in strings and is well-formed by construction. Bounding it too costs
/// nothing and means an edit that later swaps which side is which cannot
/// silently re-open the hole.
fn named_breaks(breaks: &[String]) -> String {
    fn is_declared_shape(entry: &str) -> bool {
        !entry.is_empty()
            && entry.len() <= 64
            && entry
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    }

    let printable: Vec<&str> = breaks
        .iter()
        .map(String::as_str)
        .filter(|entry| is_declared_shape(entry))
        .take(MAX_NAMED_BREAKS)
        .collect();
    let unprinted = breaks.len() - printable.len();
    match (printable.is_empty(), unprinted) {
        (true, count) => format!("{count} break(s) it did not name in a readable form"),
        (false, 0) => printable.join(", "),
        (false, count) => format!("{}, and {count} more", printable.join(", ")),
    }
}

/// Why the build stamp no longer decides anything, and what does (issue #801).
///
/// # The three layers, kept apart
///
/// | question | mechanism | where |
/// |---|---|---|
/// | can we decode each other's frames at all? | [`PROTOCOL_VERSION`] | exact equality, first, never bypassable |
/// | do we agree what the fields MEAN? | the declared contract breaks | [`contract_refusal`] |
/// | may I use *this* verb? | the `Hello` reply's advertised capability set | [`project_actions_reason`] |
/// | are we the same build? | the git-describe stamp | **nothing — reported, never classified** |
///
/// # What the stamp was measuring, and why it had to stop
///
/// `git describe` reports the nearest tag **reachable from HEAD**, and a tag is
/// applied by the release workflow *after* the content it names has landed. So a
/// branch cut in between describes as the PREVIOUS release while being
/// functionally the new one, and the comparison reports a difference between two
/// trees that are byte-identical. Measured on issue #801: a branch whose
/// `git diff v0.39.0 HEAD -- src/` was **empty** was refused with
/// `build mismatch: desktop is 0.38.0-gfa02054-dirty, daemon is 0.39.0-g1ea0fe7`.
///
/// Reading the release digits off that stamp instead — which is what this module
/// did until now, via a `compatibility_key` over `0.MINOR` — is the same
/// measurement with fewer characters: `0.38` comes off the same `git describe`.
/// It narrowed the false positive to a minor boundary rather than removing it,
/// and a minor boundary is exactly where the interesting case lives.
///
/// # Nothing is lost by retiring it, and one thing is gained
///
/// While `0.x` only a **declared** break bumps the minor
/// (`docs/develop/versioning.md`), so the digit comparison could only ever
/// refuse across a break somebody had already written a `changelog.d/*.breaking.md`
/// fragment for. [`dot_agent_deck::daemon_protocol::CONTRACT_BREAKS`] is that
/// same declaration, read from the contract's own source instead of from a tag —
/// so it refuses the same pairs, minus the tag-timing error, and **plus** the
/// pairs a digit cannot reach at all: a break declared on an unreleased branch
/// has no minor to move yet, and used to read as compatible with the release it
/// was cut from.
///
/// # What is still NOT detectable, stated rather than implied
///
/// An **undeclared** semantic break. If the author does not write the fragment
/// and does not append the entry, no mechanism here sees it — the verb is still
/// advertised and still answers, and the wire shape is unchanged. The stamp did
/// not see it either (an undeclared break bumps only the patch, which the digit
/// comparison passed), so this is a residual carried over rather than one
/// introduced. What stands behind it is CLAUDE.md rule 12's cross-version manual
/// test, run before such a change merges, and the fragment its outcome demands.
///
/// # The deck kind no longer changes the answer
///
/// PRD #741 M8 split the policy by kind because a stamp difference against a
/// **remote** deck was noise the user could not act on — the remedy it offered,
/// Replace daemon, means terminating a daemon on a host you do not own, and that
/// button is disabled there anyway. That reasoning was right about the stamp and
/// is now moot: the stamp refuses nobody, of either kind. A **declared contract
/// break** is not noise — it is the one signal that says these two builds cannot
/// safely interoperate — so it refuses for both kinds, and `Connect anyway`
/// (PR #779) is the escape hatch it leaves, for both kinds.
///
/// # What this function returns
///
/// The refusal sentence for a deck a declared break separates this build from,
/// or `None` when there is nothing to refuse — which covers both "we agree" and
/// "this build predates the declaration and there is nothing to compare"
/// ([`ContractComparison::Undeclared`], whose own doc states what that costs).
///
/// The sentence names the breaks and the direction, because the two directions
/// call for different actions from whoever reads it: a deck that lacks breaks
/// this build has is the older of the two, and a deck that has breaks this build
/// lacks means the app is the stale one.
fn contract_refusal(response: &AttachResponse) -> Option<String> {
    let (peer_lacks, this_build_lacks) =
        match compare_contract_breaks(response.contract_breaks.as_deref()) {
            ContractComparison::Undeclared | ContractComparison::Agreed => return None,
            ContractComparison::Diverged {
                peer_lacks,
                this_build_lacks,
            } => (peer_lacks, this_build_lacks),
        };
    let mut sides = Vec::new();
    if !peer_lacks.is_empty() {
        sides.push(format!(
            "the deck is behind this app across {}",
            named_breaks(&peer_lacks)
        ));
    }
    if !this_build_lacks.is_empty() {
        sides.push(format!(
            "this app is behind the deck across {}",
            named_breaks(&this_build_lacks)
        ));
    }
    Some(format!(
        "contract mismatch: {}. Protocol {PROTOCOL_VERSION} matched on both sides, so the frames \
         decode — but a declared compatibility break sits between these two builds, so a field can \
         be read with the wrong meaning rather than failing outright",
        sides.join(", and ")
    ))
}

/// The verbs the desktop's project-aware surfaces need (PRD #819 M6).
///
/// All four, because the surface is one flow: list or resolve a project, prepare
/// its workflow, start the prepared agent. A daemon advertising three of them
/// can get a user as far as a launch that then fails, which is worse than
/// saying so first.
const DESKTOP_PROJECT_CAPABILITIES: [&str; 4] = [
    dot_agent_deck::daemon_protocol::CAP_LIST_PROJECTS,
    dot_agent_deck::daemon_protocol::CAP_RESOLVE_PROJECT,
    dot_agent_deck::daemon_protocol::CAP_PREPARE_WORKFLOW,
    dot_agent_deck::daemon_protocol::CAP_START_PREPARED_AGENT,
];

/// Why the project-aware surfaces are unavailable against this daemon, or
/// `None` when every verb they need was advertised (PRD #741 M8).
///
/// **This is the mechanism the build stamp is being demoted in favour of, and it
/// answers a different question.** A stamp asks "are we the same build"; this
/// asks "does this daemon do the thing I am about to ask it to do" — which is
/// the only one of the two a user can act on. It reads the `Hello` reply's
/// advertised set through [`dot_agent_deck::daemon_client::DaemonCapabilities`], the same capture
/// `establish()` hands the client, so the UI's verdict and the client's refusal
/// cannot disagree about the same daemon.
///
/// Absence is a withhold, not a grant: a daemon that advertises no set at all is
/// an older daemon, and [`dot_agent_deck::daemon_client::DaemonCapabilities::supports`] answers `false` for
/// every verb — which is what makes an old deck degrade to "you can watch the
/// agents that are running" instead of offering a launch that will fail.
///
/// The sentence is the DEGRADATION, so it says what still works. A deck whose
/// project verbs are missing is not a broken deck.
fn project_actions_reason(response: &AttachResponse) -> Option<String> {
    let capabilities = dot_agent_deck::daemon_client::DaemonCapabilities::from_hello(response);
    let missing: Vec<&str> = DESKTOP_PROJECT_CAPABILITIES
        .into_iter()
        .filter(|capability| !capabilities.supports(capability))
        .collect();
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "This deck does not advertise {}, so projects and workflows cannot be started from here. Agents already running on it stay visible and usable.",
        missing.join(", ")
    ))
}

/// Whether the build-stamp comparison is being relaxed, and by what.
///
/// The handshake refuses a daemon whose git-describe stamp differs from the
/// desktop's, which is right by default: two builds can share a wire format and
/// still disagree about what a field means, and the desktop may not recycle a
/// daemon that owns live agents. But the refusal on its own left the user with
/// nowhere to go — a released daemon never matches a branch build, an installed
/// CLI and a downloaded `.app` update on different cadences, and **Replace
/// daemon** is deliberately disabled while agents are live (issue #801).
///
/// Relaxing it ONLY relaxes the stamp comparison. The `PROTOCOL_VERSION` check
/// runs first and is never bypassed, so an actually-incompatible wire is still
/// refused; and the mismatch stays visible in the connection message rather
/// than being swallowed, for whichever of the two switches turned it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildMismatchAllowance {
    /// The stamp check applies: a difference refuses the connection.
    Refuse,
    /// Relaxed for the whole process by [`BUILD_MISMATCH_BYPASS_ENV`].
    Env,
    /// Relaxed for this app session by an explicit in-app confirmation.
    Session,
}

impl BuildMismatchAllowance {
    fn allows(self) -> bool {
        !matches!(self, Self::Refuse)
    }
}

/// Env var arming [`BuildMismatchAllowance::Env`].
const BUILD_MISMATCH_BYPASS_ENV: &str = "DOT_AGENT_DECK_DESKTOP_ALLOW_BUILD_MISMATCH";

/// The in-app allowance, armed by `DesktopAction::AllowBuildMismatch`.
///
/// Deliberately a process-global rather than anything in `DesktopState`: it is
/// SESSION-scoped, so it lives exactly as long as the app process and is never
/// written anywhere. Quitting the app re-arms the refusal, and the user
/// re-affirms the next time they meet the same daemon.
static SESSION_BUILD_MISMATCH_ALLOWED: AtomicBool = AtomicBool::new(false);

/// Arms [`BuildMismatchAllowance::Session`] for the rest of this app session.
pub(crate) fn allow_build_mismatch_this_session() {
    set_session_build_mismatch_allowance(true);
}

fn set_session_build_mismatch_allowance(allowed: bool) {
    SESSION_BUILD_MISMATCH_ALLOWED.store(allowed, Ordering::Relaxed);
}

/// Only `1` and `true` arm the env switch — see the test below.
fn env_allows_build_mismatch() -> bool {
    matches!(
        std::env::var(BUILD_MISMATCH_BYPASS_ENV).as_deref(),
        Ok("1") | Ok("true")
    )
}

/// The two switches are independent and either one is enough. The env var is
/// read first only so that its long-standing wording is what a developer who
/// set it sees; the in-app allowance is unreachable while it is set, because
/// the app never refuses and so never offers the button.
fn build_mismatch_allowance() -> BuildMismatchAllowance {
    if env_allows_build_mismatch() {
        BuildMismatchAllowance::Env
    } else if SESSION_BUILD_MISMATCH_ALLOWED.load(Ordering::Relaxed) {
        BuildMismatchAllowance::Session
    } else {
        BuildMismatchAllowance::Refuse
    }
}

/// A daemon reply that THIS build is guaranteed to classify as incompatible,
/// whatever this build happens to be stamped with.
///
/// # Why this is derived rather than a literal
///
/// A test driving the live handshake — [`hello`], and everything above it —
/// supplies only the *daemon* half of the comparison; the client half is this
/// build's own compiled-in [`dot_agent_deck::daemon_protocol::CONTRACT_BREAKS`].
/// A literal fixture would therefore encode an assumption about what that list
/// happens to hold today, and the previous incarnation of this helper — which
/// derived a *build stamp* — is the cautionary case: its literal predecessors
/// (`0.1.0-gdeadbee`) refused locally and, on a CI checkout with no tags, shared
/// the placeholder's release key with the client, so two tests asserting a
/// refusal quietly got `Connected` instead.
///
/// So the fixture is derived from the client's own list by taking a break the
/// client declares away from the daemon, which puts the pair on opposite sides
/// of a declared break by construction — at an empty list and at a long one
/// alike. The `assert!` is part of the point: a fixture that stops reaching the
/// contract branch fails HERE, naming the reason, rather than letting a caller's
/// `assert_eq!` pass for the wrong reason.
#[cfg(test)]
pub(crate) fn hello_from_a_deck_one_declared_break_behind() -> AttachResponse {
    let mut response = AttachResponse::hello(PROTOCOL_VERSION);
    let mut declared: Vec<String> = dot_agent_deck::daemon_protocol::CONTRACT_BREAKS
        .iter()
        .map(|entry| (*entry).to_string())
        .collect();
    // Dropping one is what an older deck looks like. With nothing to drop there
    // is no older deck to describe, so name a break this build does not have and
    // the divergence runs the other way — still a divergence, still the branch
    // under test.
    if declared.pop().is_none() {
        declared.push("0-a-break-this-build-does-not-declare".to_string());
    }
    assert!(
        matches!(
            compare_contract_breaks(Some(&declared)),
            ContractComparison::Diverged { .. }
        ),
        "the fixture must diverge from this build's own declared breaks, or the \
         contract branch is never entered: {declared:?}"
    );
    response.contract_breaks = Some(declared);
    response
}

fn classify_handshake(
    response: &AttachResponse,
    client_build: &str,
    allowance: BuildMismatchAllowance,
) -> HandshakeInfo {
    let server_protocol_version = response.server_version;
    let daemon_build_version = response.build_version.clone();
    let daemon_version = response.daemon_version.clone();
    let running_agent_count = response
        .running_agents
        .as_ref()
        .map(|summary| summary.count);

    // Set ONLY inside the contract branch, which the protocol check guards. A
    // rejected Hello and a protocol mismatch both return before it, so neither
    // can advertise an override that the bypass would refuse to honour anyway.
    let mut build_stamp_mismatch_only = false;
    let mut build_mismatch_was_bypassed = false;
    let error = if !response.ok {
        Some(
            response
                .error
                .clone()
                .unwrap_or_else(|| "the deck rejected Hello".into()),
        )
    } else if server_protocol_version != Some(PROTOCOL_VERSION) {
        Some(format!(
            "protocol mismatch: desktop expects {PROTOCOL_VERSION}, deck reports {}",
            server_protocol_version
                .map(|version| version.to_string())
                .unwrap_or_else(|| "no version".into())
        ))
    } else if let Some(contract) = contract_refusal(response) {
        // Reached only AFTER the protocol check above returned equal, so the
        // wire shape is already agreed and what is left is meaning. Issue #801:
        // the build stamps are named in the sentence but are not what was
        // compared — the comparison is over the two builds' declared contract
        // breaks, which live in the contract's own source and move with it
        // rather than with a tag.
        build_stamp_mismatch_only = true;
        let builds = format!(
            "Builds: desktop is {client_build}, deck is {}",
            daemon_build_version.as_deref().unwrap_or("unreported")
        );
        // Whichever switch is armed, the mismatch is kept in `error` (not
        // dropped) so the caveat stays on screen for the whole session rather
        // than being silently forgotten.
        build_mismatch_was_bypassed = allowance.allows();
        match allowance {
            BuildMismatchAllowance::Env => Some(format!(
                "{contract}. {builds}. Bypassed by {BUILD_MISMATCH_BYPASS_ENV}. Development only."
            )),
            BuildMismatchAllowance::Session => Some(format!(
                "{contract}. {builds}. Connected anyway for this session."
            )),
            BuildMismatchAllowance::Refuse => {
                let recovery = match running_agent_count {
                    Some(0) => "No live agents are reported; use Replace deck to start the matching bundled build, or Connect anyway to keep this one.".into(),
                    Some(count) => format!(
                        "The deck reports {count} live agent{}; stop them individually before replacing the deck, or Connect anyway to keep this one.",
                        if count == 1 { "" } else { "s" }
                    ),
                    None => "The deck could not report its live-agent count, so automatic replacement is disabled; Connect anyway keeps this one.".into(),
                };
                Some(format!("{contract}. {builds}. {recovery}"))
            }
        }
    } else {
        None
    };

    HandshakeInfo {
        status: if error.is_some() && !build_mismatch_was_bypassed {
            ConnectionStatus::Incompatible
        } else {
            ConnectionStatus::Connected
        },
        error: error.map(safe_message),
        server_protocol_version,
        daemon_build_version,
        daemon_version,
        running_agent_count,
        build_stamp_mismatch_only,
        // PRD #741 M8: read from the SAME reply, so the surface the UI offers
        // and the refusal `DaemonClient` would raise come from one capture.
        // Computed for every classification, including the refused ones —
        // `disconnected_snapshot` carries `None` and the webview reads an absent
        // reason as "nothing to say", which on a screen that is already telling
        // the user the deck is unreachable is the right amount to say.
        project_actions_reason: response
            .ok
            .then(|| project_actions_reason(response))
            .flatten(),
    }
}

/// [`classify_handshake`] with the process-wide allowance read, for
/// `endpoint_test`'s pure classification tests.
///
/// Test-only and deliberately narrow: `Test connection` must classify a
/// handshake exactly as the connection banner does, so it reuses this
/// classifier rather than growing a second one — and the tests that pin the
/// split it adds have to drive the same function the production path does.
#[cfg(test)]
pub(crate) fn classify_handshake_for_test(
    response: &AttachResponse,
    client_build: &str,
) -> HandshakeInfo {
    classify_handshake(response, client_build, build_mismatch_allowance())
}

/// The classified handshake as a [`DesktopConnection`], stamped with the deck it
/// was taken against.
///
/// **PRD #742 M3 added the endpoint, and that one parameter is the whole of
/// test-plan items 8 and 9.** `snapshot_with` has taken the deck as an argument
/// since #741 M4(a) and this function dropped it at the one place the identity
/// is written, reading `socket_path_text()` and `selection_fields()` off the
/// process-global selection instead. So every deck in a fleet was emitted under
/// the *selected* deck's name — and since the frontend derives `daemonId` from
/// `connection.socketPath` and keys agents by `(daemonId, agentId)`, two decks
/// arriving under one identity do not render an error, they render one fleet
/// where there were two.
fn connection_from_handshake(endpoint: &Endpoint, handshake: HandshakeInfo) -> DesktopConnection {
    let (deck_kind, local_only_reason, selection_fallback) = selection_fields(endpoint);
    DesktopConnection {
        status: handshake.status,
        socket_path: deck_path_text(endpoint),
        // PRD #742 M5: the KEY beside the label, from the same endpoint and in
        // the same breath — the two disagreeing is the whole defect, and the
        // only way to make them disagree now is to hand this function the wrong
        // deck, which is the mistake M3 already closed.
        deck_id: deck_wire_id(endpoint),
        deck_kind,
        local_only_reason,
        selection_fallback,
        error: handshake.error,
        client_protocol_version: PROTOCOL_VERSION,
        server_protocol_version: handshake.server_protocol_version,
        client_build_version: dot_agent_deck::build_id::local_build_id(),
        daemon_build_version: handshake.daemon_build_version,
        daemon_version: handshake.daemon_version,
        running_agent_count: handshake.running_agent_count,
        build_stamp_mismatch_only: handshake.build_stamp_mismatch_only,
        project_actions_reason: handshake.project_actions_reason,
    }
}

/// One handshake exchange, classified — and the reply it was classified from.
///
/// The reply is carried out rather than dropped because it is also where the
/// daemon advertises its capability set (PRD #819 M5), and `trusted_daemon()`
/// is exactly the site
/// [`DaemonClient::store_capabilities_from_hello`] was written for: capturing
/// the set here costs nothing, while letting `DaemonClient::capabilities()`
/// learn it on its own would spend a second `Hello` on a connection whose
/// handshake reply is already in hand.
pub(crate) async fn hello(socket_path: &Path) -> Result<(HandshakeInfo, AttachResponse), String> {
    bounded_reply("the handshake", hello_exchange(socket_path)).await
}

/// How long the desktop waits for ONE daemon reply before calling the deck not
/// answering (PRD #742 M14).
///
/// # Why a bound exists at all
///
/// Every request this crate sends is a `connect`, a write and a `read_response`,
/// and `read_response` waits for as long as the peer keeps the socket open. A
/// daemon that ACCEPTS a connection and never answers it is therefore not a
/// failure any caller can observe — it is a future that never completes. That is
/// a different thing from a deck being down, which is an immediate `ECONNREFUSED`
/// and has always worked.
///
/// It costs more than a stuck request. A deck's watcher emits its first snapshot
/// only *after* its first reply — a handshake, a subscription and a `ListAgents`
/// — so a peer that stalls any of the three leaves the webview with a deck it
/// has been told exists and has heard nothing about, forever. M14 renders that
/// as a pending group, which is right for the seconds an ssh tunnel takes and
/// wrong as a permanent state: a spinner that never resolves is worse than a
/// late appearance.
///
/// # What this bounds, and what it does not
///
/// The three replies above are the ones a first snapshot waits on, and each is
/// bounded here; the `ssh` forward before them already was, at
/// `FORWARD_READY_TIMEOUT`. What stays outside any clock is the watcher TASK
/// itself ending — a panic inside its loop — which leaves the deck with nothing
/// to emit for it at all. That is bounded by a user action rather than by time:
/// `WatcherClaim::watching` is what makes the webview's Reconnect start a
/// replacement, where before M14 the dead claim refused one for the life of the
/// process.
///
/// # Why fifteen seconds
///
/// A responsive daemon answers any of the three in sub-milliseconds — the reply
/// is built from an in-memory registry — so this is three to four orders of
/// magnitude of headroom rather than a tuned value, and nothing legitimate is
/// near it. It sits deliberately below the 30s
/// [`dot_agent_deck::remote_tunnel`] forward-ready timeout that precedes it, so
/// the two bounds add to something a user will wait through rather than
/// multiply; and above the watcher's 5s reconcile interval, so a deck that is
/// merely quiet is never mistaken for one that is stalled.
///
/// **Not applied to the event stream**, which is long-lived by design: this
/// bounds the RESP that confirms a subscription, and never the frames after it.
pub(crate) const DECK_REPLY_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound one daemon round trip at [`DECK_REPLY_TIMEOUT`].
///
/// The elapsed case is reported as what it is — a deck that took the connection
/// and did not answer — rather than folded into the transport error beside it,
/// because the two send a reader to different places: one says the deck is not
/// there and the other says it is there and wedged.
pub(crate) async fn bounded_reply<T, E: std::fmt::Display>(
    what: &str,
    reply: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, String> {
    match tokio::time::timeout(DECK_REPLY_TIMEOUT, reply).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(safe_message(error.to_string())),
        Err(_) => Err(safe_message(format!(
            "the deck took the connection but did not answer {what} within {}s",
            DECK_REPLY_TIMEOUT.as_secs()
        ))),
    }
}

async fn hello_exchange(socket_path: &Path) -> Result<(HandshakeInfo, AttachResponse), String> {
    let client_build = dot_agent_deck::build_id::local_build_id();
    let stream = IpcStream::connect(socket_path)
        .await
        .map_err(|error| safe_message(error.to_string()))?;
    let (mut reader, mut writer) = stream.into_split();
    let response = issue_command(
        &mut reader,
        &mut writer,
        &AttachRequest::Hello {
            client_version: PROTOCOL_VERSION,
            client_build_version: Some(client_build.clone()),
        },
    )
    .await
    .map_err(|error| safe_message(error.to_string()))?;
    let info = classify_handshake(&response, &client_build, build_mismatch_allowance());
    Ok((info, response))
}

/// Take one handshake against `endpoint` and build the link behind it.
///
/// PRD #741 M4(a): this is the **establishment** path, reached once per
/// endpoint per [`HANDSHAKE_REVALIDATE_INTERVAL`] rather than once per request
/// batch. Everything in it is per-connection work that was being paid
/// per-refresh — including the blocking `std::fs::metadata` in the trust check,
/// which is the "incidental win" PRD #741's `#745` answer names.
async fn establish(
    endpoint: &Endpoint,
    tunnels: &EndpointTunnels,
) -> Result<TrustedDaemon, String> {
    // PRD #741 M2: the trust check stays exactly where it was — out of band and
    // BEFORE the first connect, on the inode itself. It is LOCAL-only, and that
    // is a statement about what it can prove rather than an omission: uid +
    // 0o600 on an inode says nothing about a daemon on another machine, whose
    // trust rests on ssh host-key and user authentication instead (M5). Moving
    // it inside connect would change the Unix semantics it exists for.
    //
    // M4(a) moves it off the per-refresh path by moving the whole of this
    // function there — it is still the first thing that happens, and still
    // happens before anything connects, which is the property M2 pinned. What
    // changed is only how often: once per establishment rather than once per
    // `get_snapshot()`.
    if let Some(local) = endpoint.as_local() {
        dot_agent_deck::platform::fsperm::verify_endpoint_trusted(local.path()).map_err(
            |reason| {
                safe_message(format!(
                    "refusing to connect to the deck at {}: {reason}",
                    local.path().to_string_lossy()
                ))
            },
        )?;
    }
    // PRD #741 M7: the address comes from a LEASE on the transport, not from
    // the endpoint. `Endpoint::connect_address` errors for the `Remote` arm by
    // design — an endpoint alone has no address until a tunnel exists — so this
    // is the line that makes a remote deck reachable at all, and the lease is
    // held on the link below so the tunnel cannot be closed under it.
    let transport = tunnels
        .acquire(endpoint)
        .await
        .map_err(|error| safe_message(error.to_string()))?;
    // PRD #741 M3: `hello()` still takes the raw address and still connects with
    // an `IpcStream`, deliberately. Under DECISION 1A a remote deck is reached
    // through a forwarded Unix socket, so the handshake needs no transport of
    // its own — M5 supplies the address, not a different way of opening it.
    let (info, response) = hello(transport.address()).await?;
    let connection = connection_from_handshake(endpoint, info);
    // Built from the TRANSPORT rather than the address, so the client carries
    // what a `stat` of that address is allowed to mean. `DaemonClient::new`
    // would stamp a tunnel's own socket `LocalInode` and put `exists()`-as-health
    // back on exactly the inode M3's `Elsewhere` protects.
    let client = transport.client().map_err(safe_message)?;
    // PRD #819 M5/M6: capture the advertised set from THIS reply.
    //
    // Its invalidation rule used to be satisfied structurally by accident —
    // every `trusted_daemon()` built a fresh client, so the capture could not
    // outlive the connection it described because the client did not either.
    // PRD #741 M4(a) holds the client, so the rule is now satisfied
    // deliberately instead: the capture and the handshake it came from are the
    // same object, and [`DaemonLinks::invalidate`] drops both together. There
    // is no path that replaces one without the other.
    client.store_capabilities_from_hello(&response);
    Ok(TrustedDaemon {
        client: Arc::new(client),
        connection,
        _transport: transport,
        established: Instant::now(),
    })
}

/// The link for the selected deck, establishing one if needed.
///
/// PRD #741 M4(a): takes the store because the link is HELD in `DesktopState`
/// rather than rebuilt here. The endpoint is still
/// [`selected_endpoint`]'s — M9 is what makes that a user choice — so this
/// remains the one function every desktop call site goes through.
pub(crate) async fn trusted_daemon(links: &DaemonLinks) -> Result<Arc<TrustedDaemon>, String> {
    links.trusted(&selected_endpoint()).await
}

pub(crate) async fn get_snapshot(links: &DaemonLinks) -> DesktopSnapshot {
    snapshot_of(&selected_endpoint(), links).await
}

/// [`get_snapshot`] against a named deck rather than the selected one.
///
/// Split out at M4(a) so the snapshot path can be driven against a scripted
/// socket without reaching for the process-global
/// `DOT_AGENT_DECK_ATTACH_SOCKET`, and because PRD #741 M9 makes the selection a
/// parameter in earnest.
async fn snapshot_of(endpoint: &Endpoint, links: &DaemonLinks) -> DesktopSnapshot {
    snapshot_with(endpoint, links, None).await
}

/// [`get_snapshot`] answered from the watcher's incremental view where it can be
/// (PRD #741 M4(b)).
///
/// The split is deliberately narrow: **only the watcher passes a view**, so
/// every other caller — `desktop_get_snapshot`, `bootstrap`, and the
/// `refresh_and_emit` that tails every `DesktopAction` — still fetches the whole
/// list exactly as it did at M4(a). Those are user-initiated and rare, they
/// follow acts that just changed the registry, and making them cached would have
/// traded the milestone's own no-user-visible-change rule for a saving of
/// nothing.
///
/// The view decides; this function only obeys. When it asks for a fetch the
/// reply is installed into it and rendered from it, so the fetch path and the
/// cached path render through the same code and cannot disagree about the
/// mapping.
pub(crate) async fn snapshot_with(
    endpoint: &Endpoint,
    links: &DaemonLinks,
    view: Option<&mut AgentView>,
) -> DesktopSnapshot {
    let daemon = match links.trusted(endpoint).await {
        Ok(daemon) => daemon,
        Err(error) => return disconnected_snapshot(endpoint, error),
    };
    let connection = daemon.connection();
    if connection.status != ConnectionStatus::Connected {
        // Unchanged, and deliberately never cached: on this path
        // `running_agent_count` comes from the handshake and gates the
        // **Replace daemon** button. M4(a) kept it off the held-link path for
        // that reason and M4(b) keeps it off the held-*list* path for the same
        // one.
        return DesktopSnapshot {
            connection,
            agents: Vec::new(),
            // Issue #887: no `ListAgents` was issued on this path, so no
            // revision was reported. Same rule as `running_agent_count` above.
            schedule_revision: None,
            protocol_version: PROTOCOL_VERSION,
            source: "daemon",
            fleet: observed_fleet(),
            unconfigured: unconfigured_fleet(),
            observed: observed_fleet_decks(),
        };
    }

    if let Some(view) = view {
        if view.needs_fetch(tokio::time::Instant::now()).is_none() {
            let records = view.records();
            return connected_snapshot(connection, records, view.schedule_revision());
        }
        return match bounded_reply("ListAgents", daemon.client.list_agents_detailed()).await {
            Ok(listing) => {
                view.install(listing, tokio::time::Instant::now());
                connected_snapshot(connection, view.records(), view.schedule_revision())
            }
            Err(error) => {
                // The fetch failed, so the view's demand stands: it was never
                // cleared, and the next refresh will try again rather than
                // promoting whatever it was holding into an answer.
                links.invalidate(endpoint).await;
                disconnected_snapshot(endpoint, error)
            }
        };
    }

    // PRD #742 M14: bounded, like the handshake above it. This is the call that
    // produces a deck's FIRST snapshot, so a peer that stalls here is a deck the
    // webview never hears from rather than a request that is merely slow.
    match bounded_reply("ListAgents", daemon.client.list_agents_detailed()).await {
        Ok(listing) => connected_snapshot(connection, listing.records, listing.schedule_revision),
        Err(error) => {
            // The held link just failed to carry a request. Whatever is at the
            // other end is not the daemon this handshake classified, so the
            // classification goes with the connection — the next call
            // handshakes again rather than reporting a verdict it can no longer
            // support.
            links.invalidate(endpoint).await;
            disconnected_snapshot(endpoint, error)
        }
    }
}

/// The connected snapshot for a set of agent records.
///
/// PRD #741 M4(a): the count comes from THESE RECORDS rather than from the held
/// handshake, and it is the same number from the same source — the daemon
/// answers `Hello`'s `running_agents` with
/// `RunningAgentsSummary::from_records(&registry.agent_records())` and answers
/// `ListAgents` with `registry.agent_records()`, so the count is `records.len()`
/// either way. Reading it here rather than off the handshake is what keeps the
/// one user-visible number the handshake carries at least as fresh as it was
/// before the link was held — it feeds the "the daemon reports N live agents"
/// line in the Stop-daemon confirmation.
///
/// **M4(b) makes `records` sometimes the cached list rather than a fresh reply,
/// and the count stays derived from it on purpose.** The alternative — a fresh
/// count beside a cached list — would let the banner and the tiles disagree,
/// which is a worse failure than both being up to
/// [`crate::agent_view::RECONCILE_INTERVAL`] old together. The number is
/// advisory in any case: the refusal that actually protects a live orchestration
/// is made daemon-side by `run_daemon_stop` (issue #770), not by this figure,
/// and every `DesktopAction` — Stop included — tails a full `refresh_and_emit`.
fn connected_snapshot(
    connection: DesktopConnection,
    records: Vec<dot_agent_deck::daemon_client::AgentRecord>,
    schedule_revision: Option<u64>,
) -> DesktopSnapshot {
    DesktopSnapshot {
        connection: DesktopConnection {
            running_agent_count: Some(records.len()),
            ..connection
        },
        agents: records.into_iter().map(map_agent).collect(),
        // Issue #887: carried beside the records it arrived with — including on
        // the cached path, where the view replays the revision from the reply
        // that installed those records. A fresh revision beside a cached list
        // would make the picker re-list against a list it cannot yet see.
        schedule_revision,
        protocol_version: PROTOCOL_VERSION,
        source: "daemon",
        // The applied document's observed set, not this deck's anything — so
        // every deck in a fleet carries the same list and a webview may prune
        // its map on whichever snapshot happens to land first (PRD #742 M5).
        fleet: observed_fleet(),
        unconfigured: unconfigured_fleet(),
        observed: observed_fleet_decks(),
    }
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        true
    }
}

fn daemon_binary_name() -> &'static str {
    if cfg!(windows) {
        "dot-agent-deck.exe"
    } else {
        "dot-agent-deck"
    }
}

fn resolve_daemon_executable() -> Result<PathBuf, String> {
    if let Some(raw) = std::env::var_os("DOT_AGENT_DECK_BINARY") {
        let path = PathBuf::from(raw);
        if is_executable_file(&path) {
            return Ok(path);
        }
        return Err(format!(
            "DOT_AGENT_DECK_BINARY is not an executable file: {}",
            path.to_string_lossy()
        ));
    }

    if let Ok(current_exe) = std::env::current_exe()
        && let Some(parent) = current_exe.parent()
    {
        let sibling = parent.join(daemon_binary_name());
        if sibling != current_exe && is_executable_file(&sibling) {
            return Ok(sibling);
        }

        for ancestor in parent.ancestors() {
            let candidate = ancestor.join(daemon_binary_name());
            if candidate != current_exe && is_executable_file(&candidate) {
                return Ok(candidate);
            }
        }
    }

    if let Some(path_env) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path_env) {
            let candidate = directory.join(daemon_binary_name());
            if is_executable_file(&candidate) {
                return Ok(candidate);
            }
        }
    }

    Err(
        "dot-agent-deck CLI was not found; build/install it, place it next to the desktop binary, or set DOT_AGENT_DECK_BINARY before launching the desktop app"
            .into(),
    )
}

pub(crate) async fn bootstrap(options: &BootstrapOptions, links: &DaemonLinks) -> DesktopSnapshot {
    let current = get_snapshot(links).await;
    if current.connection.status != ConnectionStatus::Disconnected || !options.start_if_missing {
        return current;
    }

    // PRD #741 M2: lazy-spawn starts a daemon process on THIS machine, so it is
    // reachable only from a local endpoint. A remote deck that is not answering
    // is reported as such — starting a local daemon in its place is the exact
    // silently-wrong outcome the endpoint split exists to prevent.
    let endpoint = selected_endpoint();
    let Some(local) = endpoint.as_local() else {
        return current;
    };
    let state_dir = dot_agent_deck::config::state_dir();
    let state_dir_for_spawn = state_dir.clone();
    let start_result = ensure_daemon_running(
        local,
        &state_dir,
        move || {
            let executable = resolve_daemon_executable()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            spawn_daemon_serve_detached_with_exe(&state_dir_for_spawn, &executable).map(|_| ())
        },
        DAEMON_POLL_INTERVAL,
        DAEMON_START_POLL_TIMEOUT,
    )
    .await;

    match start_result {
        Ok(()) => {
            // A daemon process was just started at this address, so nothing
            // held about the one that was not answering a moment ago describes
            // it. Drop the link before the snapshot that will re-establish it.
            links.invalidate(&endpoint).await;
            get_snapshot(links).await
        }
        Err(error) => disconnected_snapshot(&endpoint, error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dot_agent_deck::daemon_client::LocalEndpoint;
    use std::sync::{Mutex, MutexGuard};

    /// The env var and the session flag are both process-global, so the tests
    /// that write either one take this first. Under nextest each test owns its
    /// own process and the lock is free; under a plain `cargo test` the whole
    /// module shares one process and without it a test that sets the env var
    /// would be read by a test asserting it is unset.
    static ALLOWANCE_LOCK: Mutex<()> = Mutex::new(());

    /// Holds the lock and restores BOTH switches on drop, including on a panic,
    /// so one failing assertion cannot leave the rest of the module armed.
    struct AllowanceGuard(#[allow(dead_code)] MutexGuard<'static, ()>);

    impl AllowanceGuard {
        fn acquire() -> Self {
            let guard = ALLOWANCE_LOCK
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            clear_allowance_switches();
            Self(guard)
        }
    }

    impl Drop for AllowanceGuard {
        fn drop(&mut self) {
            clear_allowance_switches();
        }
    }

    fn clear_allowance_switches() {
        // SAFETY: every test that touches the variable holds `ALLOWANCE_LOCK`,
        // so no other thread in this process is reading the environment here.
        unsafe { std::env::remove_var(BUILD_MISMATCH_BYPASS_ENV) };
        set_session_build_mismatch_allowance(false);
    }

    /// A Hello that agreed on the protocol and reports `stamp` as its build.
    /// The zero-agent summary is there so the refusal path renders its full
    /// recovery sentence rather than the could-not-report one.
    fn hello_with_build(stamp: Option<&str>) -> AttachResponse {
        let mut response = AttachResponse::hello(PROTOCOL_VERSION)
            .with_running_agents(RunningAgentsSummary::default());
        response.build_version = stamp.map(str::to_string);
        response
    }

    fn set_bypass_env(value: Option<&str>) {
        // SAFETY: as above — guarded by `ALLOWANCE_LOCK`.
        unsafe {
            match value {
                Some(value) => std::env::set_var(BUILD_MISMATCH_BYPASS_ENV, value),
                None => std::env::remove_var(BUILD_MISMATCH_BYPASS_ENV),
            }
        }
    }

    /// A deck one declared contract break behind this build, stamped however you
    /// like.
    ///
    /// Two things had to be separable to test issue #801 at all, and this is the
    /// half that carries the contract: the stamp is now an argument to the
    /// *other* helper, because it no longer decides anything.
    fn hello_one_break_behind(stamp: Option<&str>) -> AttachResponse {
        let mut response = hello_from_a_deck_one_declared_break_behind()
            .with_running_agents(RunningAgentsSummary::default());
        response.build_version = stamp.map(str::to_string);
        response
    }

    /// Issue #801, the measured false positive: two builds that agree about the
    /// contract connect however far apart their git-describe stamps are.
    ///
    /// The pair is the one from the issue — a branch cut after v0.39.0's content
    /// landed but before its tag was applied, so `git describe` named `0.38.0`
    /// while `git diff v0.39.0 HEAD -- src/` was **empty**. Under the old
    /// classification the release keys `(0, 38)` and `(0, 39)` differed and the
    /// connection was refused; the contract is identical, so it is not.
    ///
    /// The `-dirty` half matters too: a stamp is dirty for an unsaved buffer, and
    /// a rebuild of an unchanged tree is a different stamp again.
    #[test]
    fn stamps_a_release_apart_connect_when_the_contract_agrees() {
        let _guard = AllowanceGuard::acquire();
        let info = classify_handshake(
            &hello_with_build(Some("0.39.0-g1ea0fe7")),
            "0.38.0-gfa02054-dirty",
            BuildMismatchAllowance::Refuse,
        );
        assert_eq!(
            info.status,
            ConnectionStatus::Connected,
            "two identical contracts must not be refused for where a tag sits: {:?}",
            info.error
        );
        assert!(info.error.is_none(), "{:?}", info.error);
        assert!(!info.build_stamp_mismatch_only);
    }

    /// And the half that stops this being a classifier that only ever says yes:
    /// a declared contract break refuses even when the two stamps are a *patch*
    /// apart — the case the release-key comparison passed silently.
    ///
    /// This is the shape issue #801 names as the one nothing could see. The wire
    /// is identical, both sides report the same `PROTOCOL_VERSION`, and the
    /// digits the old check read are equal; the only difference is a break
    /// somebody declared.
    #[test]
    fn a_declared_break_refuses_even_within_one_release() {
        let _guard = AllowanceGuard::acquire();
        let info = classify_handshake(
            &hello_one_break_behind(Some("0.39.0-g1111111")),
            "0.39.0-g2222222",
            BuildMismatchAllowance::Refuse,
        );
        assert_eq!(info.status, ConnectionStatus::Incompatible);
        let error = info.error.expect("a refusal says why");
        assert!(error.contains("contract mismatch"), "{error}");
        // `last()`, not `[0]`: `hello_from_a_deck_one_declared_break_behind`
        // makes its older deck by `pop()`ing the tail, so the break it withholds
        // is the LAST declared one. Those were the same element while
        // `CONTRACT_BREAKS` held a single entry, and stopped being the same at
        // the second — asserting on `[0]` was reading the fixture's intent off a
        // coincidence.
        let withheld = dot_agent_deck::daemon_protocol::CONTRACT_BREAKS
            .last()
            .expect("the fixture withholds a declared break, so there is one");
        assert!(
            error.contains(withheld),
            "the sentence names the withheld break {withheld}: {error}"
        );
        assert!(
            info.build_stamp_mismatch_only,
            "the protocol agreed, so `Connect anyway` stays available"
        );
    }

    /// The deck's KIND no longer changes the verdict (issue #801, superseding
    /// PRD #741 M8).
    ///
    /// M8 let a remote deck connect through a stamp difference because the
    /// remedy a refusal offered — Replace daemon — is not available on a host
    /// you do not own. That reasoning was about the *stamp*, which now refuses
    /// nobody of either kind. What is left refusing is a declared break, which
    /// is not noise for either kind, so the answer is one answer.
    #[test]
    fn a_declared_break_refuses_whatever_the_deck_kind() {
        let _guard = AllowanceGuard::acquire();
        // One classifier, one reply, one verdict — and `classify_handshake` has
        // no endpoint parameter left to make it two.
        let info = classify_handshake(
            &hello_one_break_behind(Some("0.39.0-gdeadbee")),
            "0.39.0-gcafe123",
            BuildMismatchAllowance::Refuse,
        );
        assert_eq!(info.status, ConnectionStatus::Incompatible);
        let error = info.error.expect("the refusal says why");
        assert!(error.contains("0.39.0-gdeadbee"), "{error}");
        assert!(error.contains("0.39.0-gcafe123"), "{error}");
        assert!(
            error.contains("read with the wrong meaning"),
            "what a decoding wire cannot rule out is stated rather than implied: {error}"
        );
        assert!(error.contains("Connect anyway"), "{error}");
    }

    /// The direction is named, because the two directions call for different
    /// actions from whoever reads the sentence.
    #[test]
    fn the_refusal_names_which_side_is_behind() {
        let _guard = AllowanceGuard::acquire();
        let mut ahead = hello_with_build(Some("0.39.0-gdeadbee"));
        ahead.contract_breaks = Some(vec!["999-a-break-this-build-predates".to_string()]);
        let error = classify_handshake(&ahead, "0.39.0-gcafe123", BuildMismatchAllowance::Refuse)
            .error
            .expect("a refusal says why");
        assert!(
            error.contains("this app is behind the deck"),
            "a deck ahead of the app must say so: {error}"
        );

        let error = classify_handshake(
            &hello_one_break_behind(Some("0.39.0-gdeadbee")),
            "0.39.0-gcafe123",
            BuildMismatchAllowance::Refuse,
        )
        .error
        .expect("a refusal says why");
        assert!(
            error.contains("the deck is behind this app"),
            "a deck behind the app must say so: {error}"
        );
    }

    /// The reply a REAL released daemon sends, classified.
    ///
    /// Captured verbatim from `dot-agent-deck 0.40.2 daemon hello` on
    /// 2026-09-15 — the current release at the time issue #801 was fixed, and
    /// the daemon a user of this app is most likely to meet. It is a literal
    /// rather than a constructed fixture because the property under test is
    /// about a build this repo can no longer produce: it reports
    /// `server_version: 9` and carries no `contract_breaks` key at all.
    ///
    /// What it pins is that the reply still DESERIALIZES — a field added to
    /// `AttachResponse` must stay additive — and that the refusal it now earns
    /// comes from the PROTOCOL FLOOR rather than from this classifier.
    ///
    /// **It used to assert that the capture CONNECTS, and that assertion was
    /// correct until issue #1049 took `PROTOCOL_VERSION` from 9 to 10.** The
    /// capture reports 9, so every released daemon through `0.40.2` is now
    /// refused before the contract lists are ever compared. That is the version
    /// floor doing its job, not issue #801's false positive returning — and the
    /// distinction is the whole point of keeping this capture: the error must
    /// say `protocol mismatch`, because a `contract mismatch` here would mean
    /// the classifier had reached a peer it should never have got to.
    ///
    /// The property the old assertion protected — that omitting the field is
    /// not itself a refusal — moved to
    /// [`a_same_protocol_peer_that_omits_the_contract_list_connects`], which is
    /// where it can still be shown. Do not fold the two back together: this one
    /// is evidence about a build that exists, and that one is about a code path.
    #[test]
    fn the_real_v0_40_2_daemon_hello_is_refused_by_the_protocol_floor() {
        let _guard = AllowanceGuard::acquire();
        let captured = r#"{"ok":true,"server_version":9,"build_version":"0.40.2-g7e87d7f","daemon_version":"0.40.2"}"#;
        let response: AttachResponse =
            serde_json::from_str(captured).expect("a released daemon's reply must still decode");
        // The captured literal, NOT `PROTOCOL_VERSION`: pinning it to the
        // constant is what made this test fail the moment #1049 moved it, and
        // the capture's value is a fact about a released build that no later
        // bump can change.
        assert_eq!(response.server_version, Some(9));
        assert!(
            response.contract_breaks.is_none(),
            "the capture is only evidence while it predates the field"
        );

        let info = classify_handshake(
            &response,
            "0.40.1-ga41acea6-dirty",
            BuildMismatchAllowance::Refuse,
        );
        assert_eq!(info.status, ConnectionStatus::Incompatible);
        let error = info.error.expect("a protocol mismatch must be reported");
        assert!(
            error.contains("protocol mismatch"),
            "the floor must refuse this, not the contract classifier: {error}"
        );
        assert!(
            !error.contains("contract mismatch"),
            "the contract lists must never be compared across a protocol gap: {error}"
        );
    }

    /// Omitting `contract_breaks` is not itself a refusal.
    ///
    /// This is the half of the old capture test that survived issue #1049's
    /// `PROTOCOL_VERSION` bump, and it matters because
    /// [`ContractComparison::Unknown`] reads an absent list as "connect" — an
    /// assumption its own doc flags as an assumption. Every other fixture here
    /// goes through `AttachResponse::hello`, which POPULATES the list, so
    /// without this test nothing at the handshake layer exercises the absent
    /// case at all.
    ///
    /// Constructed rather than captured, deliberately: a real build old enough
    /// to omit the field is also old enough to fail the protocol floor, so the
    /// two properties can no longer be shown by the same peer.
    #[test]
    fn a_same_protocol_peer_that_omits_the_contract_list_connects() {
        let _guard = AllowanceGuard::acquire();
        let mut response = hello_with_build(Some("0.40.1-gdeadbee"));
        response.contract_breaks = None;

        let info = classify_handshake(
            &response,
            "0.40.1-ga41acea6-dirty",
            BuildMismatchAllowance::Refuse,
        );
        assert_eq!(
            info.status,
            ConnectionStatus::Connected,
            "a peer that declares nothing must not be refused for the silence: {:?}",
            info.error
        );
        assert!(info.error.is_none(), "{:?}", info.error);
    }

    /// A hostile deck cannot write whatever it likes into the connection banner.
    ///
    /// The break names on one side of a divergence are PEER-supplied — they are
    /// what the deck declared and this build did not — and the banner scrubs its
    /// sentence with `safe_message`, which removes general category `Cc` and lets
    /// `Cf` (the bidi controls) through. A right-to-left override in one would
    /// reverse everything printed after it, which is PRD #741 final audit F4's
    /// finding about `build_version` reached by a new route. Length is the other
    /// half: a peer may send as many entries as a 16 MiB frame holds.
    ///
    /// Both are bounded at the render, not hoped about — see [`named_breaks`].
    #[test]
    fn a_hostile_contract_list_reaches_the_banner_as_neither_bidi_nor_a_flood() {
        let _guard = AllowanceGuard::acquire();
        let mut hostile = hello_with_build(Some("0.39.0-gdeadbee"));
        let mut declared: Vec<String> = dot_agent_deck::daemon_protocol::CONTRACT_BREAKS
            .iter()
            .map(|entry| (*entry).to_string())
            .collect();
        declared.push("999-\u{202e}drowssap".to_string());
        declared.extend((0..500).map(|n| format!("{n}-flood-entry")));
        hostile.contract_breaks = Some(declared);

        let error = classify_handshake(&hostile, "0.39.0-gcafe123", BuildMismatchAllowance::Refuse)
            .error
            .expect("a divergence refuses");

        assert!(
            !error.contains('\u{202e}'),
            "the override reached the banner: {error:?}"
        );
        assert!(
            !error.contains("drowssap"),
            "a name this app will not vouch for must not be printed at all: {error:?}"
        );
        assert!(
            error.contains("and 497 more"),
            "the rest must be counted rather than listed: {error:?}"
        );
        assert!(
            error.len() < 600,
            "a banner sentence must stay readable, got {} bytes",
            error.len()
        );
    }

    /// And when nothing the peer sent is printable, the count is all that is
    /// said — never an empty list that reads as "no breaks".
    #[test]
    fn an_entirely_unprintable_contract_list_is_reported_as_a_count() {
        let _guard = AllowanceGuard::acquire();
        let mut hostile = hello_with_build(Some("0.39.0-gdeadbee"));
        let mut declared: Vec<String> = dot_agent_deck::daemon_protocol::CONTRACT_BREAKS
            .iter()
            .map(|entry| (*entry).to_string())
            .collect();
        declared.push("\u{202e}\u{0007}".to_string());
        declared.push("A".repeat(200));
        hostile.contract_breaks = Some(declared);

        let error = classify_handshake(&hostile, "0.39.0-gcafe123", BuildMismatchAllowance::Refuse)
            .error
            .expect("a divergence refuses");
        assert!(
            error.contains("2 break(s) it did not name in a readable form"),
            "{error:?}"
        );
        assert!(!error.contains('\u{202e}'), "{error:?}");
        assert!(!error.contains("AAAA"), "{error:?}");
    }

    /// A deck that predates the declaration connects, and that is deliberate.
    ///
    /// Every released daemon up to `v0.40.2` omits the field. Refusing them for
    /// saying nothing would relocate issue #801's false positive rather than
    /// remove it — see `ContractComparison::Undeclared`, which states the
    /// residual the arm costs.
    #[test]
    fn a_deck_that_declares_no_contract_at_all_connects() {
        let _guard = AllowanceGuard::acquire();
        let mut older = hello_with_build(Some("0.40.2-gfeedfac"));
        older.contract_breaks = None;
        let info = classify_handshake(&older, "0.41.0-gcafe123", BuildMismatchAllowance::Refuse);
        assert_eq!(info.status, ConnectionStatus::Connected);
        assert!(info.error.is_none(), "{:?}", info.error);
    }

    /// `PROTOCOL_VERSION` is the hard floor for **every** deck kind, and the
    /// remote demotion does not reach it (PRD #741 M8).
    ///
    /// This is the one thing no policy, switch or endpoint kind may bypass —
    /// the order in `classify_handshake` IS the security property.
    #[test]
    fn a_remote_deck_is_still_refused_on_a_protocol_mismatch() {
        let _guard = AllowanceGuard::acquire();
        set_bypass_env(Some("1"));
        let response = AttachResponse::hello(PROTOCOL_VERSION + 1);

        let info = classify_handshake(
            &response,
            response.build_version.as_deref().unwrap(),
            build_mismatch_allowance(),
        );

        assert_eq!(info.status, ConnectionStatus::Incompatible);
        assert!(
            !info.build_stamp_mismatch_only,
            "a wire mismatch must never advertise an override"
        );
        assert!(info.error.unwrap().contains("protocol mismatch"));
    }

    /// A daemon advertising every project verb offers the project surfaces; one
    /// advertising none withholds them with a named reason (PRD #741 M8).
    ///
    /// The withhold reason is derived from the ADVERTISED SET and not from a
    /// version digit or a stamp, which is the whole of issue #801's middle
    /// layer.
    #[test]
    fn project_actions_gate_on_the_advertised_capability_set() {
        let _guard = AllowanceGuard::acquire();
        // Built from `DESKTOP_PROJECT_CAPABILITIES` — the set this gate reads —
        // rather than from `AttachResponse::with_capabilities()`, which
        // advertises whatever the LOCAL platform's daemon can do.
        // `DAEMON_CAPABILITIES` is deliberately shorter on Windows (PRD #819
        // strikes `prepare-workflow` and `start-prepared-agent` there, for want
        // of a DACL implementation), so "the full set" and "everything the
        // desktop needs" are the same list on Unix and different lists on
        // Windows — and this test is about the second one. It failed on
        // `build-windows` for exactly that reason, and only became visible once
        // the crate compiled there again.
        let mut full = AttachResponse::hello(PROTOCOL_VERSION);
        full.capabilities = Some(
            DESKTOP_PROJECT_CAPABILITIES
                .iter()
                .map(|capability| capability.to_string())
                .collect(),
        );
        assert_eq!(
            classify_handshake(
                &full,
                full.build_version.as_deref().unwrap(),
                BuildMismatchAllowance::Refuse,
            )
            .project_actions_reason,
            None,
            "a daemon advertising the full set has nothing to explain"
        );

        let bare = AttachResponse::hello(PROTOCOL_VERSION);
        let reason = classify_handshake(
            &bare,
            bare.build_version.as_deref().unwrap(),
            BuildMismatchAllowance::Refuse,
        )
        .project_actions_reason
        .expect("an unadvertised daemon withholds every verb");
        assert!(
            reason.contains(dot_agent_deck::daemon_protocol::CAP_LIST_PROJECTS),
            "the reason names what is missing: {reason}"
        );
        assert!(
            reason.contains("stay visible and usable"),
            "a degraded deck is not a broken deck: {reason}"
        );
    }

    /// A PARTIAL set is withheld too, and names only what is absent.
    ///
    /// The four verbs are one flow. A daemon with three of them can get a user
    /// as far as a launch that then fails, which is worse than saying so first.
    #[test]
    fn a_partly_advertised_daemon_withholds_the_project_surfaces() {
        let _guard = AllowanceGuard::acquire();
        let mut partial = AttachResponse::hello(PROTOCOL_VERSION);
        partial.capabilities = Some(vec![
            dot_agent_deck::daemon_protocol::CAP_LIST_PROJECTS.to_string(),
            dot_agent_deck::daemon_protocol::CAP_RESOLVE_PROJECT.to_string(),
        ]);

        let reason = classify_handshake(
            &partial,
            partial.build_version.as_deref().unwrap(),
            BuildMismatchAllowance::Refuse,
        )
        .project_actions_reason
        .expect("three of four is not four");

        assert!(
            reason.contains(dot_agent_deck::daemon_protocol::CAP_PREPARE_WORKFLOW),
            "{reason}"
        );
        assert!(
            !reason.contains(dot_agent_deck::daemon_protocol::CAP_LIST_PROJECTS),
            "what IS advertised is not listed as missing: {reason}"
        );
    }

    #[test]
    fn matching_hello_is_connected() {
        let response = AttachResponse::hello(PROTOCOL_VERSION);
        let info = classify_handshake(
            &response,
            response.build_version.as_deref().unwrap(),
            BuildMismatchAllowance::Refuse,
        );
        assert_eq!(info.status, ConnectionStatus::Connected);
        assert!(info.error.is_none());
        assert!(!info.build_stamp_mismatch_only);
    }

    #[test]
    fn protocol_mismatch_is_visible_and_never_treated_as_disconnected() {
        let response = AttachResponse::hello(PROTOCOL_VERSION + 1);
        let info = classify_handshake(
            &response,
            response.build_version.as_deref().unwrap(),
            BuildMismatchAllowance::Refuse,
        );
        assert_eq!(info.status, ConnectionStatus::Incompatible);
        assert!(info.error.unwrap().contains("protocol mismatch"));
    }

    #[test]
    fn zero_agent_contract_mismatch_points_to_safe_replacement() {
        let _guard = AllowanceGuard::acquire();
        let info = classify_handshake(
            &hello_one_break_behind(Some("0.39.0-gdeadbee")),
            "desktop-other-build",
            BuildMismatchAllowance::Refuse,
        );
        assert_eq!(info.status, ConnectionStatus::Incompatible);
        let error = info.error.unwrap();
        assert!(error.contains("contract mismatch"));
        assert!(error.contains("use Replace deck"));
    }

    #[test]
    fn live_agent_contract_mismatch_blocks_replacement() {
        let _guard = AllowanceGuard::acquire();
        let mut response = hello_from_a_deck_one_declared_break_behind();
        response.running_agents = Some(RunningAgentsSummary {
            count: 2,
            names: vec!["coder".into(), "tester".into()],
        });
        let info = classify_handshake(
            &response,
            "desktop-other-build",
            BuildMismatchAllowance::Refuse,
        );
        assert_eq!(info.status, ConnectionStatus::Incompatible);
        let error = info.error.unwrap();
        assert!(
            error.contains("stop them individually before replacing"),
            "{error}"
        );
        // Issue #801: replacement is refused while agents are live, and that is
        // correct — but it used to be the ONLY thing offered, which left a user
        // with nine running agents no way into the app at all.
        assert!(error.contains("Connect anyway"), "{error}");
    }

    /// A declared break is downgraded to a warning, not silence: the deck
    /// connects, but the connection message still names the break and both
    /// builds so the caveat survives for the whole session.
    #[test]
    fn bypassed_contract_mismatch_connects_and_keeps_the_warning_visible() {
        let _guard = AllowanceGuard::acquire();
        let info = classify_handshake(
            &hello_one_break_behind(Some("0.39.0-gdeadbee")),
            "desktop-other-build",
            BuildMismatchAllowance::Env,
        );
        assert_eq!(info.status, ConnectionStatus::Connected);
        let error = info.error.expect("bypass must not swallow the mismatch");
        assert!(error.contains("contract mismatch"), "{error}");
        assert!(error.contains(BUILD_MISMATCH_BYPASS_ENV), "{error}");
    }

    /// The in-app override says so in its own words. Naming the env var here
    /// would tell a user who pressed a button to go looking for a shell
    /// variable they never set — and a `.app` launched from Finder could not
    /// have received one anyway (issue #801).
    #[test]
    fn session_override_connects_and_names_itself_rather_than_the_env_var() {
        let _guard = AllowanceGuard::acquire();
        let mut response = hello_from_a_deck_one_declared_break_behind();
        response.running_agents = Some(RunningAgentsSummary {
            count: 9,
            names: vec!["coder".into()],
        });
        let info = classify_handshake(
            &response,
            "desktop-other-build",
            BuildMismatchAllowance::Session,
        );
        assert_eq!(info.status, ConnectionStatus::Connected);
        let error = info
            .error
            .expect("the override must not swallow the mismatch");
        assert!(error.contains("contract mismatch"), "{error}");
        assert!(
            error.contains("Connected anyway for this session"),
            "{error}"
        );
        assert!(error.contains("read with the wrong meaning"), "{error}");
        assert!(!error.contains(BUILD_MISMATCH_BYPASS_ENV), "{error}");
    }

    /// The load-bearing one. The bypass exists to relax a *stamp* comparison
    /// once the wire is known to agree; it must never let an actually
    /// incompatible protocol through, because that is the check that keeps a
    /// newer client from misreading an older daemon's frames.
    #[test]
    fn bypass_never_rescues_a_protocol_mismatch() {
        let response = AttachResponse::hello(PROTOCOL_VERSION + 1);
        for allowance in [BuildMismatchAllowance::Env, BuildMismatchAllowance::Session] {
            let info = classify_handshake(&response, "desktop-other-build", allowance);
            assert_eq!(info.status, ConnectionStatus::Incompatible, "{allowance:?}");
            assert!(
                info.error.unwrap().contains("protocol mismatch"),
                "{allowance:?}"
            );
        }
    }

    /// The other half of the same property, and the one the UI reads: a
    /// protocol mismatch must not ADVERTISE an override either. Offering
    /// Connect anyway there would put a button on screen that cannot work —
    /// pressing it re-runs the handshake and gets refused again — and would
    /// teach the user that the wire check is negotiable. It is not.
    #[test]
    fn protocol_mismatch_never_advertises_an_override() {
        let response = AttachResponse::hello(PROTOCOL_VERSION + 1);
        for allowance in [
            BuildMismatchAllowance::Refuse,
            BuildMismatchAllowance::Env,
            BuildMismatchAllowance::Session,
        ] {
            let info = classify_handshake(&response, "desktop-other-build", allowance);
            assert!(!info.build_stamp_mismatch_only, "{allowance:?}");
        }
    }

    /// A Hello the daemon itself rejected is not a stamp problem either, so it
    /// carries no override — the stamp branch is never even reached.
    #[test]
    fn rejected_hello_never_advertises_an_override() {
        let mut response = AttachResponse::hello(PROTOCOL_VERSION);
        response.ok = false;
        response.error = Some("daemon is shutting down".into());
        let info = classify_handshake(
            &response,
            response.build_version.as_deref().unwrap(),
            BuildMismatchAllowance::Refuse,
        );
        assert_eq!(info.status, ConnectionStatus::Incompatible);
        assert!(!info.build_stamp_mismatch_only);
    }

    /// What the UI switches on: the protocol agreed and the disagreement is
    /// above the wire, so an override is legitimate. True whether or not one is
    /// already armed — the flag describes the mismatch, not the response to it.
    ///
    /// **The field is still spelled `build_stamp_mismatch_only`** and is named
    /// for what used to set it. Since issue #801 a build-stamp difference sets
    /// nothing at all and this is set by a declared contract break; the name is
    /// kept because it is the key the webview switches `Connect anyway` on.
    #[test]
    fn contract_mismatch_advertises_an_overridable_refusal() {
        let _guard = AllowanceGuard::acquire();
        let response = hello_one_break_behind(Some("0.39.0-gdeadbee"));
        for allowance in [
            BuildMismatchAllowance::Refuse,
            BuildMismatchAllowance::Env,
            BuildMismatchAllowance::Session,
        ] {
            let info = classify_handshake(&response, "desktop-other-build", allowance);
            assert!(info.build_stamp_mismatch_only, "{allowance:?}");
        }
    }

    /// A daemon that reports no build stamp at all is still refused on the
    /// contract, and the bypass covers it the same way — otherwise the escape
    /// hatch would have a hole exactly where the least is known about the peer.
    ///
    /// The absent stamp is now only a hole in the *sentence*, which says
    /// `unreported` rather than pretending to a value. It is no longer a hole in
    /// the classification: the contract is what was compared.
    #[test]
    fn bypass_covers_an_unreported_daemon_stamp() {
        let _guard = AllowanceGuard::acquire();
        let response = hello_one_break_behind(None);
        let info = classify_handshake(&response, "desktop-build", BuildMismatchAllowance::Env);
        assert_eq!(info.status, ConnectionStatus::Connected);
        assert!(info.build_stamp_mismatch_only);
        assert!(info.error.unwrap().contains("unreported"));
    }

    /// Only `1` and `true` arm it. An unset variable, an empty string, or a
    /// stray value leaves the refusal in place: this is a switch someone turns
    /// on deliberately, not one they trip over.
    #[test]
    fn bypass_env_is_opt_in_and_ignores_stray_values() {
        let _guard = AllowanceGuard::acquire();
        for (value, expected) in [
            (None, false),
            (Some(""), false),
            (Some("0"), false),
            (Some("yes"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            set_bypass_env(value);
            assert_eq!(env_allows_build_mismatch(), expected, "value {value:?}");
        }
    }

    /// The runtime allowance is honoured on its own, with no environment at
    /// all — which is the entire point: a `.app` launched from Finder inherits
    /// no shell environment, so the env var was never a remedy a user could
    /// reach (issue #801).
    #[test]
    fn session_allowance_is_honoured_without_the_env_var() {
        let _guard = AllowanceGuard::acquire();
        assert_eq!(build_mismatch_allowance(), BuildMismatchAllowance::Refuse);

        allow_build_mismatch_this_session();

        assert_eq!(build_mismatch_allowance(), BuildMismatchAllowance::Session);
        let response = AttachResponse::hello(PROTOCOL_VERSION)
            .with_running_agents(RunningAgentsSummary::default());
        let info = classify_handshake(&response, "desktop-other-build", build_mismatch_allowance());
        assert_eq!(info.status, ConnectionStatus::Connected);
    }

    /// The two switches are independent: neither one clears or requires the
    /// other, and either alone is enough. A developer's env var keeps working
    /// exactly as before, and a user's in-app confirmation does not depend on
    /// it.
    #[test]
    fn env_and_session_switches_are_independent() {
        let _guard = AllowanceGuard::acquire();
        assert_eq!(build_mismatch_allowance(), BuildMismatchAllowance::Refuse);

        set_bypass_env(Some("1"));
        assert_eq!(build_mismatch_allowance(), BuildMismatchAllowance::Env);

        set_bypass_env(None);
        assert_eq!(build_mismatch_allowance(), BuildMismatchAllowance::Refuse);

        set_session_build_mismatch_allowance(true);
        assert_eq!(build_mismatch_allowance(), BuildMismatchAllowance::Session);

        // Both armed is still allowed, and still names one reason rather than
        // inventing a third.
        set_bypass_env(Some("true"));
        assert!(build_mismatch_allowance().allows());

        set_session_build_mismatch_allowance(false);
        assert_eq!(build_mismatch_allowance(), BuildMismatchAllowance::Env);
    }

    /// The session allowance survives into the NEXT handshake rather than the
    /// one that was refused: nothing caches a verdict, so classifying the same
    /// response again after arming it connects.
    #[test]
    fn the_next_handshake_after_arming_the_session_allowance_connects() {
        let _guard = AllowanceGuard::acquire();
        let response = hello_one_break_behind(Some("0.39.0-gdeadbee"));

        let refused =
            classify_handshake(&response, "desktop-other-build", build_mismatch_allowance());
        assert_eq!(refused.status, ConnectionStatus::Incompatible);
        assert!(refused.build_stamp_mismatch_only);

        allow_build_mismatch_this_session();

        let retried =
            classify_handshake(&response, "desktop-other-build", build_mismatch_allowance());
        assert_eq!(retried.status, ConnectionStatus::Connected);
        assert!(
            retried
                .error
                .expect("the caveat must survive the override")
                .contains("contract mismatch")
        );
    }

    /// The case issue #801 was filed about, at every distance a stamp can sit.
    ///
    /// A released daemon and a branch desktop, agreeing about the contract:
    /// connect, silently — under EVERY allowance, because this must be the
    /// ordinary verdict and not something an override rescues. The list runs
    /// from a rebuild of one commit through a whole MINOR apart, which is the
    /// distance the old release-key comparison refused and the distance issue
    /// #801 measured a false positive at.
    #[test]
    fn a_stamp_difference_at_any_distance_connects_silently() {
        for (desktop, daemon) in [
            ("0.39.0-ga0165f8", "0.39.0-g1ea0fe7"),
            ("0.39.0-49-ga0165f8", "0.39.0-g1ea0fe7"),
            ("0.39.0-49-ga0165f8-dirty", "0.39.0-g1ea0fe7"),
            ("0.39.2-ga0165f8", "0.39.0-g1ea0fe7"),
            // The issue's own pair: a branch cut before v0.39.0's tag was
            // applied, against the daemon that tag names.
            ("0.38.0-gfa02054-dirty", "0.39.0-g1ea0fe7"),
            // And the other direction, plus a major boundary for good measure.
            ("0.40.0-g1ea0fe7", "0.39.0-ga0165f8"),
            ("1.9.2-ga0165f8", "2.0.0-g1ea0fe7"),
        ] {
            for allowance in [
                BuildMismatchAllowance::Refuse,
                BuildMismatchAllowance::Env,
                BuildMismatchAllowance::Session,
            ] {
                let case = format!("{desktop} vs {daemon} under {allowance:?}");
                let info = classify_handshake(&hello_with_build(Some(daemon)), desktop, allowance);
                assert_eq!(info.status, ConnectionStatus::Connected, "{case}");
                assert!(!info.build_stamp_mismatch_only, "{case}");
                assert!(info.error.is_none(), "{case}: {:?}", info.error);
            }
        }
    }

    /// The declared break is what must keep prompting — and must keep offering
    /// the override, because the wire itself still agreed.
    ///
    /// This replaces a test that asserted the same thing of a differing MINOR
    /// digit. The digit was a proxy for "somebody declared a break", since only
    /// a declared break bumps it while `0.x`; this reads the declaration
    /// itself, so it holds on a branch whose tag has not been applied yet and on
    /// a build with no tags at all.
    #[test]
    fn a_declared_break_still_prompts_with_an_override() {
        let _guard = AllowanceGuard::acquire();
        let info = classify_handshake(
            &hello_one_break_behind(Some("0.39.0-g1ea0fe7")),
            "0.39.0-ga0165f8",
            BuildMismatchAllowance::Refuse,
        );
        assert_eq!(info.status, ConnectionStatus::Incompatible);
        assert!(info.build_stamp_mismatch_only);
        let error = info.error.unwrap();
        assert!(error.contains("contract mismatch"), "{error}");
        assert!(error.contains("Connect anyway"), "{error}");
    }

    /// An unreadable stamp is no longer a refusal, on either side.
    ///
    /// It used to be, and the reason it used to be is instructive: the old
    /// comparison needed to parse release digits out of both stamps, and could
    /// only connect on positive evidence that both had parsed and matched — so
    /// anything it could not read fell back to the prompt. Nothing parses a
    /// stamp any more, so a nightly build, a truncated stamp, or a deck that
    /// reports no stamp at all is simply a deck whose contract this build can
    /// still compare.
    ///
    /// Worth keeping as a test rather than deleting with the parser: these are
    /// the exact inputs that produced a refusal a user could not act on, and
    /// `None` in particular is what a deck built from a `.git`-less tarball
    /// reports.
    #[test]
    fn an_unreadable_stamp_no_longer_refuses_on_either_side() {
        let _guard = AllowanceGuard::acquire();
        for (desktop, daemon) in [
            ("0.39.0-ga0165f8", Some("nightly")),
            ("nightly", Some("0.39.0-g1ea0fe7")),
            ("0.39-ga0165f8", Some("0.39.0-g1ea0fe7")),
            ("0.39.0.1-ga0165f8", Some("0.39.0-g1ea0fe7")),
            ("", Some("0.39.0-g1ea0fe7")),
            ("0.39.0-ga0165f8", None),
        ] {
            let case = format!("{desktop} vs {daemon:?}");
            let info = classify_handshake(
                &hello_with_build(daemon),
                desktop,
                BuildMismatchAllowance::Refuse,
            );
            assert_eq!(info.status, ConnectionStatus::Connected, "{case}");
            assert!(!info.build_stamp_mismatch_only, "{case}");
            assert!(info.error.is_none(), "{case}: {:?}", info.error);
        }
    }

    /// And the same inputs still refuse when a declared break separates them:
    /// an unreadable stamp does not become a way *through* the contract check.
    #[test]
    fn an_unreadable_stamp_is_not_a_way_past_a_declared_break() {
        let _guard = AllowanceGuard::acquire();
        for daemon in [Some("nightly"), None] {
            let info = classify_handshake(
                &hello_one_break_behind(daemon),
                "nightly",
                BuildMismatchAllowance::Refuse,
            );
            assert_eq!(
                info.status,
                ConnectionStatus::Incompatible,
                "{daemon:?} must still be refused on the contract"
            );
        }
    }

    // -----------------------------------------------------------------------
    // The LIVE connect path, over a scripted socket.
    //
    // Every test above hands `classify_handshake` a fabricated `AttachResponse`,
    // which pins the classifier and says nothing about whether `hello()` — the
    // function that actually connects, frames a `Hello` and decodes the reply —
    // routes a real daemon's answer through it. `hello()` is a private
    // `async fn` and `trusted_daemon()` is `pub(crate)`, so this can only be an
    // in-crate unit test; there is no Tauri e2e harness to reach it from
    // outside (PRD #819, *Testing: what rule 4 means here*).
    //
    // Every item in this section is `#[cfg(unix)]`, helpers included: the
    // scripted daemon is a `tokio::net::UnixListener`, which does not exist on
    // Windows, and `scratch_socket` has no caller once the tests are gated off.
    // The live connect path is therefore unverified on Windows: `hello()` is
    // not itself gated, and the `IpcStream::connect` it calls is a **named
    // pipe** there rather than a Unix socket, so what is missing on Windows is
    // the test, not the code.
    // -----------------------------------------------------------------------

    /// A one-shot daemon: accept one connection, read one request frame, answer
    /// with `response`. Returns the request bytes it was sent, so the test can
    /// assert what the desktop actually asked for rather than only what it did
    /// with the answer.
    ///
    /// It binds a plain `tokio::net::UnixListener` rather than
    /// `platform::ipc::IpcListener`, deliberately: that helper flips the
    /// **process umask** around its `bind(2)` to create the inode owner-only,
    /// and a process-global umask is exactly the wrong thing under a plain
    /// `cargo test`, where the whole module shares one process. Measured: with
    /// the umask at `0o177`, a sibling test's `create_dir_all` produced a
    /// directory with no execute bit and the next `bind` in it failed `EACCES`.
    /// Nothing here needs the owner-only inode — the socket lives in a
    /// per-test temp directory and no daemon is trusting it.
    #[cfg(unix)]
    async fn scripted_daemon(
        listener: tokio::net::UnixListener,
        response: AttachResponse,
    ) -> Vec<u8> {
        use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP, read_frame, write_frame};
        let (stream, _peer) = listener.accept().await.expect("accept one client");
        let (mut reader, mut writer) = stream.into_split();
        let (kind, payload) = read_frame(&mut reader)
            .await
            .expect("read the request frame")
            .expect("the client sent a frame");
        assert_eq!(kind, KIND_REQ, "the desktop must open with a request frame");
        let encoded = serde_json::to_vec(&response).expect("serialize the reply");
        write_frame(&mut writer, KIND_RESP, &encoded)
            .await
            .expect("answer the client");
        payload
    }

    /// A socket path inside a fresh temp directory, plus the directory itself so
    /// the caller can clean it up. Deliberately short: a Unix socket path is
    /// capped near 100 bytes, and a long temp root silently fails to bind.
    #[cfg(unix)]
    fn scratch_socket(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("dad-{tag}-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        let socket = dir.join("s");
        (dir, socket)
    }

    /// The happy path, end to end over a real socket: `hello()` connects, sends
    /// a `Hello` carrying this build's own protocol version and stamp, and hands
    /// the daemon's reply to the classifier.
    #[cfg(unix)]
    #[tokio::test]
    async fn hello_frames_a_real_request_and_classifies_the_reply_it_gets_back() {
        let (dir, socket) = scratch_socket("hello-ok");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
        let client_build = dot_agent_deck::build_id::local_build_id();
        let mut reply = AttachResponse::hello(PROTOCOL_VERSION)
            .with_running_agents(RunningAgentsSummary::default());
        reply.build_version = Some(client_build.clone());
        // PRD #819 M5/M6: the reply is also where the capability set is
        // advertised, which is why `hello()` carries the response out rather
        // than dropping it after classification.
        reply.capabilities = Some(vec![
            dot_agent_deck::daemon_protocol::CAP_LIST_PROJECTS.to_string(),
            dot_agent_deck::daemon_protocol::CAP_RESOLVE_PROJECT.to_string(),
            dot_agent_deck::daemon_protocol::CAP_PREPARE_WORKFLOW.to_string(),
        ]);
        let daemon = tokio::spawn(scripted_daemon(listener, reply));

        let (info, response) = hello(&socket).await.expect("the handshake must complete");

        assert_eq!(info.status, ConnectionStatus::Connected);
        assert!(info.error.is_none(), "{:?}", info.error);
        assert_eq!(info.server_protocol_version, Some(PROTOCOL_VERSION));

        // What the desktop actually put on the wire.
        let request = daemon.await.expect("the scripted daemon must finish");
        let request: serde_json::Value =
            serde_json::from_slice(&request).expect("the request must be JSON");
        assert_eq!(request["op"], "hello");
        assert_eq!(request["client_version"], PROTOCOL_VERSION);
        assert_eq!(request["client_build_version"], client_build);

        // And the reply reaches the caller intact, so `trusted_daemon()` can
        // seed the client's capability cache from THIS handshake instead of
        // spending a second `Hello` re-learning it.
        let capabilities = dot_agent_deck::daemon_client::DaemonCapabilities::from_hello(&response);
        assert!(capabilities.is_advertised());
        assert!(capabilities.supports(dot_agent_deck::daemon_protocol::CAP_PREPARE_WORKFLOW));

        std::fs::remove_dir_all(dir).ok();
    }

    /// The refusal path over the same socket: a daemon speaking a different
    /// protocol version is refused by the live path, not merely by the
    /// classifier in isolation — and it advertises no override, because the wire
    /// check is not negotiable.
    #[cfg(unix)]
    #[tokio::test]
    async fn hello_refuses_a_real_daemon_on_a_different_protocol_version() {
        let (dir, socket) = scratch_socket("hello-proto");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
        let daemon = tokio::spawn(scripted_daemon(
            listener,
            AttachResponse::hello(PROTOCOL_VERSION + 1),
        ));

        let (info, _response) = hello(&socket).await.expect("the exchange still completes");

        assert_eq!(info.status, ConnectionStatus::Incompatible);
        let error = info.error.expect("a refusal must say why");
        assert!(error.contains("protocol mismatch"), "{error}");
        assert!(
            error.contains(&(PROTOCOL_VERSION + 1).to_string()),
            "the refusal must name the version the daemon reported: {error}"
        );
        assert!(!info.build_stamp_mismatch_only);

        daemon.await.expect("the scripted daemon must finish");
        std::fs::remove_dir_all(dir).ok();
    }

    /// No daemon at all is a transport error rather than a classification, and
    /// it must surface as `Err` instead of being dressed up as some verdict —
    /// `get_snapshot` turns it into the disconnected snapshot.
    #[cfg(unix)]
    #[tokio::test]
    async fn hello_reports_a_missing_daemon_as_a_transport_failure() {
        let (dir, socket) = scratch_socket("hello-gone");
        let error = hello(&socket).await.expect_err("nothing is listening");
        assert!(!error.is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    /// The load-bearing property, re-pinned at the new boundary: relaxing the
    /// STAMP comparison must not relax the WIRE check. A pair the release check
    /// would wave through is still refused, with no override advertised, for
    /// every allowance value.
    #[test]
    fn protocol_mismatch_still_refuses_a_release_compatible_pair() {
        let mut response = hello_with_build(Some("0.39.0-g1ea0fe7"));
        response.server_version = Some(PROTOCOL_VERSION + 1);
        for allowance in [
            BuildMismatchAllowance::Refuse,
            BuildMismatchAllowance::Env,
            BuildMismatchAllowance::Session,
        ] {
            let info = classify_handshake(&response, "0.39.0-ga0165f8", allowance);
            assert_eq!(info.status, ConnectionStatus::Incompatible, "{allowance:?}");
            assert!(!info.build_stamp_mismatch_only, "{allowance:?}");
            assert!(
                info.error.unwrap().contains("protocol mismatch"),
                "{allowance:?}"
            );
        }
    }
    // -----------------------------------------------------------------------
    // PRD #741 M4(a) — the held link
    // -----------------------------------------------------------------------

    /// Bind a scripted daemon's socket at a mode the trust check accepts.
    ///
    /// `establish()` runs `verify_endpoint_trusted` on the inode before it
    /// connects (M2's property 2, which M4(a) moves off the per-refresh path
    /// without moving it out of the way), so a fixture socket left at the
    /// ambient umask would be refused before any handshake happened. Restating
    /// 0o600 afterwards rather than borrowing `IpcListener::bind`'s
    /// umask-before-bind dance, for the reason `scripted_daemon` already gives:
    /// that dance flips a PROCESS-global umask.
    #[cfg(unix)]
    fn bind_trusted(socket: &std::path::Path) -> tokio::net::UnixListener {
        use std::os::unix::fs::PermissionsExt;
        let listener = tokio::net::UnixListener::bind(socket).expect("bind the scripted daemon");
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
            .expect("restate 0o600 on the socket inode");
        listener
    }

    /// A scripted daemon that answers `replies.len()` connections, one request
    /// each, in order — and then returns how many it accepted.
    ///
    /// One request each is not a simplification: the real daemon's
    /// `handle_connection` reads exactly ONE frame, dispatches it and returns,
    /// which is the finding that decided this milestone's shape. A helper that
    /// looped on one connection would be modelling a daemon that does not
    /// exist.
    #[cfg(unix)]
    async fn scripted_daemon_sequence(
        listener: tokio::net::UnixListener,
        replies: Vec<AttachResponse>,
    ) -> usize {
        use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP, read_frame, write_frame};
        let mut accepted = 0usize;
        for reply in replies {
            let (stream, _peer) = listener.accept().await.expect("accept one client");
            accepted += 1;
            let (mut reader, mut writer) = stream.into_split();
            let (kind, _payload) = read_frame(&mut reader)
                .await
                .expect("read the request frame")
                .expect("the client sent a frame");
            assert_eq!(kind, KIND_REQ);
            let encoded = serde_json::to_vec(&reply).expect("serialize the reply");
            write_frame(&mut writer, KIND_RESP, &encoded)
                .await
                .expect("answer the client");
        }
        accepted
    }

    /// One row in a `ListAgents` reply. Only the fields the snapshot mapping
    /// reads are populated; the rest is what a freshly-spawned agent carries.
    #[cfg(unix)]
    fn listed_agent(id: &str, pane_id: &str) -> dot_agent_deck::daemon_client::AgentRecord {
        dot_agent_deck::daemon_client::AgentRecord {
            id: id.into(),
            pane_id_env: Some(pane_id.into()),
            display_name: None,
            cwd: None,
            tab_membership: None,
            agent_type: None,
            rows: 24,
            cols: 80,
            live: None,
            spawned_at_ms: None,
            cli_name: None,
            crashed: None,
        }
    }

    /// A `Hello` reply this build classifies as `Connected`.
    #[cfg(unix)]
    fn matching_hello() -> AttachResponse {
        let mut reply = AttachResponse::hello(PROTOCOL_VERSION)
            .with_running_agents(RunningAgentsSummary::default());
        reply.build_version = Some(dot_agent_deck::build_id::local_build_id());
        reply
    }

    /// **The milestone.** Two `trusted()` calls against one endpoint take ONE
    /// handshake and open ONE connection — before M4(a) they took two of each,
    /// and the watcher took them up to 6.667 times a second.
    ///
    /// The scripted daemon is told to answer exactly one connection, so a
    /// regression that re-handshakes does not merely fail a counter: the second
    /// `trusted()` finds nothing accepting and reports a transport error.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_held_link_is_reused_and_costs_one_handshake() {
        let (dir, socket) = scratch_socket("m4a-reuse");
        let listener = bind_trusted(&socket);
        let daemon = tokio::spawn(scripted_daemon_sequence(listener, vec![matching_hello()]));

        let links = DaemonLinks::default();
        let endpoint = Endpoint::Local(LocalEndpoint::at(&socket));

        let first = links.trusted(&endpoint).await.expect("first establishment");
        let second = links.trusted(&endpoint).await.expect("the held link");

        assert!(
            Arc::ptr_eq(&first, &second),
            "the second call must hand back the SAME link, not an equal one"
        );
        assert_eq!(
            links.handshake_count(),
            1,
            "a held link must not re-handshake"
        );
        assert_eq!(
            daemon.await.expect("the scripted daemon must not panic"),
            1,
            "exactly one connection must have reached the daemon"
        );
        first.require_compatible().expect("classified as connected");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The property the whole-map lock existed for, kept after PRD #742 M3 split
    /// it: **two callers racing on the SAME deck still cost one handshake.**
    ///
    /// Its sibling above drives the two calls sequentially, so it measures the
    /// held-link cache and not the collapse — it would pass just as well with no
    /// serialisation at all. This one is the half M3 could break: the map lock is
    /// no longer held across establishment, so what stops a concurrent first-use
    /// from opening a second connection is the per-deck gate and nothing else.
    /// Without it both callers would miss the empty map and handshake, which is
    /// N ssh authentications for N tiles arriving at once on a remote deck.
    ///
    /// The script offers **two** replies while the assertion demands one
    /// connection, so a broken collapse fails on the count rather than hanging;
    /// a working one leaves the daemon waiting for a second client that never
    /// comes, which is why it is aborted rather than awaited.
    ///
    /// The cross-deck half — two callers on DIFFERENT decks never waiting for
    /// each other — is
    /// [`tests::an_unresponsive_deck_does_not_queue_another_decks_handshake`].
    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_first_uses_of_one_deck_still_collapse_into_one_handshake() {
        let (dir, socket) = scratch_socket("m3-collapse");
        let listener = bind_trusted(&socket);
        let daemon = tokio::spawn(scripted_daemon_sequence(
            listener,
            vec![matching_hello(), matching_hello()],
        ));

        let links = DaemonLinks::default();
        let endpoint = Endpoint::Local(LocalEndpoint::at(&socket));

        let (first, second) = tokio::join!(links.trusted(&endpoint), links.trusted(&endpoint));
        let first = first.expect("the racing caller that established");
        let second = second.expect("the racing caller that waited");

        daemon.abort();
        let _ = std::fs::remove_dir_all(dir);

        assert!(
            Arc::ptr_eq(&first, &second),
            "both racers must hold the SAME link, not two equal ones"
        );
        assert_eq!(
            links.handshake_count(),
            1,
            "concurrent first-uses of one deck must collapse into ONE handshake \
             — the per-deck gate is all that keeps this true now that the map \
             lock is not held across establishment"
        );
    }

    /// Invalidation is the other half: after it the next call handshakes again,
    /// against whatever is at the address now. This is what every replacement
    /// route goes through — the watcher losing its event stream, Stop, Replace,
    /// and the in-app build-mismatch allowance.
    #[cfg(unix)]
    #[tokio::test]
    async fn invalidating_a_link_makes_the_next_call_handshake_again() {
        let (dir, socket) = scratch_socket("m4a-invalidate");
        let listener = bind_trusted(&socket);
        let daemon = tokio::spawn(scripted_daemon_sequence(
            listener,
            vec![matching_hello(), matching_hello()],
        ));

        let links = DaemonLinks::default();
        let endpoint = Endpoint::Local(LocalEndpoint::at(&socket));

        let first = links.trusted(&endpoint).await.expect("first establishment");
        links.invalidate(&endpoint).await;
        let second = links
            .trusted(&endpoint)
            .await
            .expect("re-establishment after invalidation");

        assert!(
            !Arc::ptr_eq(&first, &second),
            "an invalidated link must not survive as the same object"
        );
        assert_eq!(links.handshake_count(), 2);
        assert_eq!(
            daemon.await.expect("the scripted daemon must not panic"),
            2,
            "the re-establishment must really have reconnected"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `invalidate_all` is the whole-store form, used where the reason to
    /// distrust what is held is not specific to one deck.
    #[cfg(unix)]
    #[tokio::test]
    async fn invalidate_all_drops_every_held_link() {
        let (dir, socket) = scratch_socket("m4a-invalidate-all");
        let listener = bind_trusted(&socket);
        let daemon = tokio::spawn(scripted_daemon_sequence(
            listener,
            vec![matching_hello(), matching_hello()],
        ));

        let links = DaemonLinks::default();
        let endpoint = Endpoint::Local(LocalEndpoint::at(&socket));

        links.trusted(&endpoint).await.expect("establish");
        links.invalidate_all().await;
        links.trusted(&endpoint).await.expect("re-establish");

        assert_eq!(links.handshake_count(), 2);
        assert_eq!(daemon.await.expect("no panic"), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A failed re-establishment must not resurrect the link it was replacing.
    ///
    /// The failure direction matters more than the success one: a store that
    /// kept the old entry when the new handshake failed would keep reporting a
    /// deck as connected after it stopped answering, which is the one outcome a
    /// held classification must never produce.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_re_establishment_leaves_nothing_held() {
        let (dir, socket) = scratch_socket("m4a-fail");
        let listener = bind_trusted(&socket);
        let daemon = tokio::spawn(scripted_daemon_sequence(listener, vec![matching_hello()]));

        let links = DaemonLinks::default();
        let endpoint = Endpoint::Local(LocalEndpoint::at(&socket));
        links.trusted(&endpoint).await.expect("establish");
        assert_eq!(daemon.await.expect("no panic"), 1);

        // The daemon is gone and so is its socket: the trust check now fails on
        // the missing inode, before anything connects.
        std::fs::remove_file(&socket).expect("remove the endpoint");
        links.invalidate(&endpoint).await;
        let err = links
            .trusted(&endpoint)
            .await
            .expect_err("a deck that is not there must not establish");
        assert!(err.contains("refusing to connect"), "{err}");

        // And the next call must still try rather than serve a stale verdict.
        let err = links
            .trusted(&endpoint)
            .await
            .expect_err("nothing may be held after a failed establishment");
        assert!(err.contains("refusing to connect"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The revalidation backstop, at the predicate rather than through a
    /// five-second sleep. A link is fresh the instant it is taken and stale once
    /// [`HANDSHAKE_REVALIDATE_INTERVAL`] has passed, so a held classification is
    /// bounded in age even if every explicit invalidation site were removed.
    #[test]
    fn a_held_handshake_goes_stale_after_the_revalidate_interval() {
        let link = TrustedDaemon {
            client: Arc::new(DaemonClient::new("/tmp/attach.sock".into())),
            connection: connection_from_handshake(
                &Endpoint::Local(LocalEndpoint::at("/tmp/attach.sock")),
                HandshakeInfo {
                    status: ConnectionStatus::Connected,
                    error: None,
                    server_protocol_version: Some(PROTOCOL_VERSION),
                    daemon_build_version: None,
                    daemon_version: None,
                    running_agent_count: Some(0),
                    build_stamp_mismatch_only: false,
                    project_actions_reason: None,
                },
            ),
            _transport: tokio::runtime::Runtime::new()
                .expect("a runtime for the lease")
                .block_on(
                    EndpointTunnels::default()
                        .acquire(&Endpoint::Local(LocalEndpoint::at("/tmp/attach.sock"))),
                )
                .expect("a local lease needs no transport to establish"),
            established: Instant::now(),
        };
        let taken = link.established;

        assert!(link.is_fresh(taken), "fresh the instant it is taken");
        assert!(
            link.is_fresh(taken + HANDSHAKE_REVALIDATE_INTERVAL - Duration::from_millis(1)),
            "still fresh a millisecond before the interval elapses"
        );
        assert!(
            !link.is_fresh(taken + HANDSHAKE_REVALIDATE_INTERVAL),
            "stale once the interval has elapsed — the bound is what makes a \
             held classification defensible without trusting every invalidation site"
        );
        assert!(!link.is_fresh(taken + Duration::from_secs(60)));
    }

    /// The one user-visible number the handshake carries stays fresh, which is
    /// what keeps M4(a) inside the PRD's "M1–M4 change no user-visible
    /// behaviour" property.
    ///
    /// `running_agent_count` feeds the Stop-daemon confirmation's "the daemon
    /// reports N live agents" line, so a value held for up to five seconds would
    /// be a real regression. `get_snapshot` reads it off the `ListAgents` reply
    /// instead — the same `registry.agent_records()` the daemon answers `Hello`
    /// from, only taken later. Here the handshake claims 7 and the listing
    /// carries 2; the snapshot must say 2.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_snapshots_agent_count_comes_from_the_listing_not_the_held_handshake() {
        let (dir, socket) = scratch_socket("m4a-count");
        let listener = bind_trusted(&socket);
        let mut stale_hello = matching_hello();
        stale_hello.running_agents = Some(RunningAgentsSummary {
            count: 7,
            names: vec!["ghost".into(); 7],
        });
        let listing = AttachResponse::agent_records(vec![
            listed_agent("agent-a", "pane-a"),
            listed_agent("agent-b", "pane-b"),
        ]);
        let daemon = tokio::spawn(scripted_daemon_sequence(
            listener,
            vec![stale_hello, listing],
        ));

        let links = DaemonLinks::default();
        let snapshot = snapshot_of(&Endpoint::Local(LocalEndpoint::at(&socket)), &links).await;

        assert_eq!(snapshot.connection.status, ConnectionStatus::Connected);
        assert_eq!(snapshot.agents.len(), 2);
        assert_eq!(
            snapshot.connection.running_agent_count,
            Some(2),
            "the count must come from the listing that was just fetched, not \
             from the handshake that may be up to five seconds old"
        );
        assert_eq!(daemon.await.expect("no panic"), 2);
        let _ = std::fs::remove_dir_all(dir);
    }
    /// **The milestone's measurement, taken as a test so it also guards the
    /// property.**
    ///
    /// Ten refreshes through the real snapshot path against a daemon that
    /// counts connections and sorts them by request kind. Before M4(a) this was
    /// `hello` + `list_agents` every time — **20 connections, 2.0 per refresh**,
    /// which is the row in PRD #741's baseline table. Held, it is **11
    /// connections, 1.1 per refresh**: one handshake for the ten, and the
    /// listing that is the refresh itself.
    ///
    /// Ten fits inside [`HANDSHAKE_REVALIDATE_INTERVAL`] by a wide margin, so
    /// the single handshake is the establishment and not a revalidation. At the
    /// watcher's sustained 6.667 refreshes/second the backstop adds 0.2
    /// handshakes/second, i.e. 1.03 connections per refresh rather than 1.0 —
    /// stated here because a figure that ignored the revalidation would not be
    /// comparable to the baseline it is quoted against.
    ///
    /// What this does NOT claim is that the listing connection could also have
    /// been held. It could not: the daemon's `handle_connection` answers exactly
    /// one request per connection and then returns, so 1.0 is the floor a client
    /// can reach on its own.
    #[cfg(unix)]
    #[tokio::test]
    async fn ten_refreshes_cost_one_handshake_and_ten_listings() {
        use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP, read_frame, write_frame};

        const REFRESHES: usize = 10;

        let (dir, socket) = scratch_socket("m4a-measure");
        let listener = bind_trusted(&socket);
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();

        let daemon = tokio::spawn(async move {
            let (mut hellos, mut listings) = (0usize, 0usize);
            loop {
                tokio::select! {
                    _ = &mut stop_rx => return (hellos, listings),
                    accepted = listener.accept() => {
                        let (stream, _peer) = accepted.expect("accept one client");
                        let (mut rd, mut wr) = stream.into_split();
                        let (kind, payload) = read_frame(&mut rd)
                            .await
                            .expect("read the request frame")
                            .expect("the client sent a frame");
                        assert_eq!(kind, KIND_REQ);
                        let request: AttachRequest =
                            serde_json::from_slice(&payload).expect("decode the request");
                        let reply = match request {
                            AttachRequest::Hello { .. } => {
                                hellos += 1;
                                matching_hello()
                            }
                            AttachRequest::ListAgents => {
                                listings += 1;
                                AttachResponse::agent_records(vec![listed_agent("a", "pane-a")])
                            }
                            other => panic!("the snapshot path sent an unexpected request: {other:?}"),
                        };
                        let encoded = serde_json::to_vec(&reply).expect("serialize the reply");
                        write_frame(&mut wr, KIND_RESP, &encoded)
                            .await
                            .expect("answer the client");
                    }
                }
            }
        });

        let links = DaemonLinks::default();
        let endpoint = Endpoint::Local(LocalEndpoint::at(&socket));
        for refresh in 0..REFRESHES {
            let snapshot = snapshot_of(&endpoint, &links).await;
            assert_eq!(
                snapshot.connection.status,
                ConnectionStatus::Connected,
                "refresh {refresh} must be connected"
            );
            assert_eq!(snapshot.agents.len(), 1);
        }
        let _ = stop_tx.send(());
        let (hellos, listings) = daemon.await.expect("the scripted daemon must not panic");

        assert_eq!(
            listings, REFRESHES,
            "every refresh still fetches the agent list — holding the LISTING is \
             M4(b)'s incremental work, not this milestone's"
        );
        assert_eq!(
            hellos, 1,
            "the handshake must be paid ONCE for the whole run, not once per refresh"
        );
        assert_eq!(
            hellos + listings,
            REFRESHES + 1,
            "{REFRESHES} refreshes must cost {} connections, not {}",
            REFRESHES + 1,
            REFRESHES * 2
        );
        let _ = std::fs::remove_dir_all(dir);
    }
    /// A scripted daemon that answers `Hello` and `ListAgents` for as long as it
    /// is asked to, counting each.
    ///
    /// Unlike `scripted_daemon_sequence` it is not told in advance how many
    /// connections to expect, because the whole point of the M4(b) tests is that
    /// the number is no longer one per refresh.
    #[cfg(unix)]
    async fn counting_daemon(
        listener: tokio::net::UnixListener,
        records: Vec<dot_agent_deck::daemon_client::AgentRecord>,
        mut stop: tokio::sync::oneshot::Receiver<()>,
    ) -> (usize, usize) {
        use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP, read_frame, write_frame};
        let (mut hellos, mut listings) = (0usize, 0usize);
        loop {
            tokio::select! {
                _ = &mut stop => return (hellos, listings),
                accepted = listener.accept() => {
                    let (stream, _peer) = accepted.expect("accept one client");
                    let (mut rd, mut wr) = stream.into_split();
                    let (kind, payload) = read_frame(&mut rd)
                        .await
                        .expect("read the request frame")
                        .expect("the client sent a frame");
                    assert_eq!(kind, KIND_REQ);
                    let request: AttachRequest =
                        serde_json::from_slice(&payload).expect("decode the request");
                    let reply = match request {
                        AttachRequest::Hello { .. } => {
                            hellos += 1;
                            matching_hello()
                        }
                        AttachRequest::ListAgents => {
                            listings += 1;
                            AttachResponse::agent_records(records.clone())
                        }
                        other => panic!("the snapshot path sent an unexpected request: {other:?}"),
                    };
                    let encoded = serde_json::to_vec(&reply).expect("serialize the reply");
                    write_frame(&mut wr, KIND_RESP, &encoded)
                        .await
                        .expect("answer the client");
                }
            }
        }
    }

    /// One `ToolStart` for the agent the fixtures list.
    #[cfg(unix)]
    fn fixture_tool_event(n: usize) -> dot_agent_deck::event::BroadcastMsg {
        use dot_agent_deck::event::{AgentEvent, AgentType, BroadcastMsg, EventType};
        BroadcastMsg::Event(AgentEvent {
            session_id: "pane-a-session".into(),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::ToolStart,
            tool_name: Some(format!("Tool{n}")),
            tool_detail: None,
            cwd: None,
            timestamp: chrono::Utc::now(),
            user_prompt: None,
            metadata: std::collections::HashMap::new(),
            pane_id: Some("pane-a".into()),
            agent_id: Some("a".into()),
            agent_version: None,
            schema_version: None,
            live_target: None,
        })
    }

    /// **The M4(b) measurement, taken as a test so it also guards the property.**
    ///
    /// Ten refreshes, each driven by a folded daemon event exactly as the
    /// watcher drives them, cost **one** handshake and **one** listing — two
    /// connections for the whole run. The same ten cost 11 at M4(a) and 20
    /// before it.
    ///
    /// The scripted daemon counts; it is not told how many connections to
    /// expect, so a regression that re-fetches shows up as a number rather than
    /// as a hang.
    #[cfg(unix)]
    #[tokio::test]
    async fn ten_folded_refreshes_cost_one_handshake_and_one_listing() {
        const REFRESHES: usize = 10;

        let (dir, socket) = scratch_socket("m4b-measure");
        let listener = bind_trusted(&socket);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let daemon = tokio::spawn(counting_daemon(
            listener,
            vec![listed_agent("a", "pane-a")],
            stop_rx,
        ));

        let links = DaemonLinks::default();
        let endpoint = Endpoint::Local(LocalEndpoint::at(&socket));
        let mut view = AgentView::default();

        for refresh in 0..REFRESHES {
            let snapshot = snapshot_with(&endpoint, &links, Some(&mut view)).await;
            assert_eq!(
                snapshot.connection.status,
                ConnectionStatus::Connected,
                "refresh {refresh} must be connected"
            );
            assert_eq!(snapshot.agents.len(), 1);
            if refresh > 0 {
                assert_eq!(
                    snapshot.agents[0].status, "working",
                    "refresh {refresh} must render the FOLDED status, or the cache is saving connections by showing nothing"
                );
            }
            // The watcher's order: an event arrives, is folded, and THEN the
            // refresh runs. The first refresh is the one with nothing folded
            // yet, which is exactly why it is the one that fetches.
            view.apply(&fixture_tool_event(refresh));
        }

        let _ = stop_tx.send(());
        let (hellos, listings) = daemon.await.expect("the scripted daemon must not panic");

        assert_eq!(hellos, 1, "the handshake is still paid once for the run");
        assert_eq!(
            listings, 1,
            "only the FIRST refresh may fetch — the other nine are served from              the fold"
        );
        assert_eq!(
            hellos + listings,
            2,
            "{REFRESHES} refreshes must cost 2 connections, not {} (M4(a)) and              not {} (baseline)",
            REFRESHES + 1,
            REFRESHES * 2
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A `SessionStart` names an agent whose `display_name`, `tab_membership`,
    /// `rows`/`cols` and `spawned_at_ms` are registry facts no event carries, so
    /// it must cost a fetch **at once** rather than waiting for the floor.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_session_start_spends_a_connection_to_complete_the_new_row() {
        use dot_agent_deck::event::{AgentEvent, AgentType, BroadcastMsg, EventType};

        let (dir, socket) = scratch_socket("m4b-gap1");
        let listener = bind_trusted(&socket);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let daemon = tokio::spawn(counting_daemon(
            listener,
            vec![listed_agent("a", "pane-a")],
            stop_rx,
        ));

        let links = DaemonLinks::default();
        let endpoint = Endpoint::Local(LocalEndpoint::at(&socket));
        let mut view = AgentView::default();

        let _ = snapshot_with(&endpoint, &links, Some(&mut view)).await;
        // A folded tool event costs nothing...
        view.apply(&fixture_tool_event(0));
        let _ = snapshot_with(&endpoint, &links, Some(&mut view)).await;
        // ...and a SessionStart costs exactly one listing.
        view.apply(&BroadcastMsg::Event(AgentEvent {
            session_id: "pane-b-session".into(),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::SessionStart,
            tool_name: None,
            tool_detail: None,
            cwd: Some("/home/dev/project".into()),
            timestamp: chrono::Utc::now(),
            user_prompt: None,
            metadata: std::collections::HashMap::new(),
            pane_id: Some("pane-b".into()),
            agent_id: Some("b".into()),
            agent_version: None,
            schema_version: None,
            live_target: None,
        }));
        let _ = snapshot_with(&endpoint, &links, Some(&mut view)).await;

        let _ = stop_tx.send(());
        let (hellos, listings) = daemon.await.expect("no panic");
        assert_eq!(hellos, 1);
        assert_eq!(
            listings, 2,
            "the first refresh and the SessionStart fetch; the folded tool event              in between must not"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A subscription that ends must not leave a confidently-wrong list on
    /// screen: the view refuses to answer, and a refresh whose fetch then FAILS
    /// reports the failure rather than re-rendering what it was holding.
    ///
    /// This is the milestone's sharpest failure mode — the event stream is now
    /// the correctness spine, so "the stream died" has to fail closed.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_dead_subscription_can_never_leave_a_confident_list_on_screen() {
        let (dir, socket) = scratch_socket("m4b-spine");
        let listener = bind_trusted(&socket);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let daemon = tokio::spawn(counting_daemon(
            listener,
            vec![listed_agent("a", "pane-a")],
            stop_rx,
        ));

        let links = DaemonLinks::default();
        let endpoint = Endpoint::Local(LocalEndpoint::at(&socket));
        let mut view = AgentView::default();

        let first = snapshot_with(&endpoint, &links, Some(&mut view)).await;
        assert_eq!(first.agents.len(), 1, "the fixture must list one agent");

        // The stream ended. Take the daemon away at the same moment, which is
        // the realistic pairing: the process that was serving the subscription
        // is the one that just went.
        view.resubscribed();
        let _ = stop_tx.send(());
        let _ = daemon.await.expect("no panic");
        let _ = std::fs::remove_file(&socket);

        let after = snapshot_with(&endpoint, &links, Some(&mut view)).await;
        assert_ne!(
            after.connection.status,
            ConnectionStatus::Connected,
            "a failed re-fetch must report the failure"
        );
        assert!(
            after.agents.is_empty(),
            "the stale list must not be re-rendered as though it were current"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// On the cached path the banner's agent count and the rendered rows come
    /// from the same list, so they can be up to a reconciliation interval old
    /// TOGETHER but can never disagree with each other.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_cached_count_and_the_cached_rows_always_agree() {
        let (dir, socket) = scratch_socket("m4b-count");
        let listener = bind_trusted(&socket);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let daemon = tokio::spawn(counting_daemon(
            listener,
            vec![listed_agent("a", "pane-a"), listed_agent("b", "pane-b")],
            stop_rx,
        ));

        let links = DaemonLinks::default();
        let endpoint = Endpoint::Local(LocalEndpoint::at(&socket));
        let mut view = AgentView::default();

        for _ in 0..3 {
            view.apply(&fixture_tool_event(0));
            let snapshot = snapshot_with(&endpoint, &links, Some(&mut view)).await;
            assert_eq!(
                snapshot.connection.running_agent_count,
                Some(snapshot.agents.len()),
                "the count the Stop-daemon confirmation shows must describe the                  rows the deck shows"
            );
            assert_eq!(snapshot.agents.len(), 2);
        }

        let _ = stop_tx.send(());
        let (_, listings) = daemon.await.expect("no panic");
        assert_eq!(listings, 1, "three refreshes, one listing");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Passing no view is the unchanged path, and every caller but the watcher
    /// takes it: three refreshes, three listings, exactly as M4(a) left them.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_caller_with_no_view_still_fetches_every_time() {
        let (dir, socket) = scratch_socket("m4b-noview");
        let listener = bind_trusted(&socket);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let daemon = tokio::spawn(counting_daemon(
            listener,
            vec![listed_agent("a", "pane-a")],
            stop_rx,
        ));

        let links = DaemonLinks::default();
        let endpoint = Endpoint::Local(LocalEndpoint::at(&socket));
        for _ in 0..3 {
            let snapshot = snapshot_of(&endpoint, &links).await;
            assert_eq!(snapshot.agents.len(), 1);
        }

        let _ = stop_tx.send(());
        let (hellos, listings) = daemon.await.expect("no panic");
        assert_eq!(hellos, 1);
        assert_eq!(
            listings, 3,
            "`desktop_get_snapshot`, `bootstrap` and every action's              `refresh_and_emit` must be as fresh as they were at M4(a)"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The refusal paths are deliberately NOT held, so a daemon that is
    /// answering but incompatible is re-classified on every call exactly as it
    /// was before M4(a).
    ///
    /// This is the one user-visible thing a held classification could have
    /// broken. On that path the snapshot's `running_agent_count` comes from the
    /// handshake — there is no `ListAgents` to read it off — and the webview
    /// shows **Replace daemon** only once it reads zero. Here the daemon reports
    /// two live agents and then none; the second refresh must see the zero
    /// rather than a number held from the first.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_incompatible_daemon_is_reclassified_on_every_refresh() {
        let (dir, socket) = scratch_socket("m4a-incompat");
        let listener = bind_trusted(&socket);

        // Derived from this build's own declared breaks rather than written
        // out, so the refusal this test is about happens whatever this build's
        // list holds — see `hello_from_a_deck_one_declared_break_behind`.
        let mut busy = hello_from_a_deck_one_declared_break_behind();
        busy.running_agents = Some(RunningAgentsSummary {
            count: 2,
            names: vec!["a".into(), "b".into()],
        });
        let mut drained = busy.clone();
        drained.running_agents = Some(RunningAgentsSummary::default());
        let daemon = tokio::spawn(scripted_daemon_sequence(listener, vec![busy, drained]));

        let links = DaemonLinks::default();
        let endpoint = Endpoint::Local(LocalEndpoint::at(&socket));

        let first = snapshot_of(&endpoint, &links).await;
        assert_ne!(
            first.connection.status,
            ConnectionStatus::Connected,
            "the fixture must actually be refused, or this test proves nothing"
        );
        assert_eq!(first.connection.running_agent_count, Some(2));

        let second = snapshot_of(&endpoint, &links).await;
        assert_eq!(
            second.connection.running_agent_count,
            Some(0),
            "a refused classification must be taken again, not held — the \
             Replace-daemon button is gated on this number reaching zero"
        );
        assert_eq!(
            links.handshake_count(),
            2,
            "both refusals must have cost their own handshake"
        );
        assert_eq!(daemon.await.expect("no panic"), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    // -----------------------------------------------------------------------
    // PRD #742 M3 — the fleet: isolated folds, deck-stamped emits, and the
    // whole-map mutex.
    //
    // Everything above this line drives ONE deck, which is what the desktop has
    // always had. These drive two at once, which is the milestone: #741's stale
    // fold was a SEQUENTIAL hazard (the deck the user left), and with N decks
    // the subscriptions are concurrent, so "which deck is this from" stops
    // being answerable by "there is exactly one".
    // -----------------------------------------------------------------------

    /// One `ToolStart` naming a specific deck's agent, so a fold can be traced
    /// back to the deck that broadcast it.
    #[cfg(unix)]
    fn tool_event_for(
        pane_id: &str,
        agent_id: &str,
        tool: &str,
    ) -> dot_agent_deck::event::BroadcastMsg {
        use dot_agent_deck::event::{AgentEvent, AgentType, BroadcastMsg, EventType};
        BroadcastMsg::Event(AgentEvent {
            session_id: format!("{pane_id}-session"),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::ToolStart,
            tool_name: Some(tool.to_string()),
            tool_detail: None,
            cwd: None,
            timestamp: chrono::Utc::now(),
            user_prompt: None,
            metadata: std::collections::HashMap::new(),
            pane_id: Some(pane_id.into()),
            agent_id: Some(agent_id.into()),
            agent_version: None,
            schema_version: None,
            live_target: None,
        })
    }

    /// The active tool the snapshot shows for `id`, or `None` if that agent is
    /// not in it at all.
    #[cfg(unix)]
    fn active_tool_of(snapshot: &DesktopSnapshot, id: &str) -> Option<String> {
        snapshot
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .and_then(|agent| agent.active_tool.as_ref())
            .map(|tool| tool.name.clone())
    }

    /// **Test-plan item 8, and the headline of the milestone.** Scenario: two
    /// decks are observed and answered concurrently, each running its own agent,
    /// and deck A broadcasts a `ToolStart` for A's agent. Deck A's snapshot must
    /// show that agent working and must be labelled with deck A's own address;
    /// deck B's snapshot must show only B's agent, untouched by A's broadcast,
    /// and must be labelled with B's address.
    ///
    /// This is the N-deck generalisation of the bug PRD #741 already shipped and
    /// fixed once: its watcher went on folding the previous deck's broadcasts
    /// into the view answering the new deck's snapshots and re-emitting them
    /// under the new deck's name. #741's fix handled a sequential selection
    /// change; here both subscriptions are live at the same time, which is
    /// strictly harder because `BroadcastMsg` carries no deck id.
    ///
    /// **Asserted on the end state rather than on the routing.** Nothing here
    /// says how many watcher tasks there are or where a view lives — only that a
    /// snapshot answered FOR a deck carries that deck's agents and that deck's
    /// identity. Every emit goes through this function, so a merged watcher that
    /// mislabelled would fail here whatever its task topology.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_fold_for_one_deck_never_reaches_another_decks_snapshot() {
        let (dir_a, socket_a) = scratch_socket("m3-fold-a");
        let (dir_b, socket_b) = scratch_socket("m3-fold-b");
        let listener_a = bind_trusted(&socket_a);
        let listener_b = bind_trusted(&socket_b);
        let (stop_a, stop_a_rx) = tokio::sync::oneshot::channel::<()>();
        let (stop_b, stop_b_rx) = tokio::sync::oneshot::channel::<()>();
        let daemon_a = tokio::spawn(counting_daemon(
            listener_a,
            vec![listed_agent("agent-a", "pane-a")],
            stop_a_rx,
        ));
        let daemon_b = tokio::spawn(counting_daemon(
            listener_b,
            vec![listed_agent("agent-b", "pane-b")],
            stop_b_rx,
        ));

        let links = DaemonLinks::default();
        let deck_a = Endpoint::Local(LocalEndpoint::at(&socket_a));
        let deck_b = Endpoint::Local(LocalEndpoint::at(&socket_b));
        let mut view_a = AgentView::default();
        let mut view_b = AgentView::default();

        // Both decks answered at once, which is the fleet's shape rather than
        // the selection change #741 fixed.
        let (first_a, first_b) = tokio::join!(
            snapshot_with(&deck_a, &links, Some(&mut view_a)),
            snapshot_with(&deck_b, &links, Some(&mut view_b)),
        );
        assert_eq!(first_a.connection.status, ConnectionStatus::Connected);
        assert_eq!(first_b.connection.status, ConnectionStatus::Connected);

        // Deck A broadcasts. Nothing about the message says which deck it came
        // from, which is exactly why the isolation has to be structural.
        view_a.apply(&tool_event_for("pane-a", "agent-a", "Bash"));

        let (second_a, second_b) = tokio::join!(
            snapshot_with(&deck_a, &links, Some(&mut view_a)),
            snapshot_with(&deck_b, &links, Some(&mut view_b)),
        );

        let _ = stop_a.send(());
        let _ = stop_b.send(());
        daemon_a.abort();
        daemon_b.abort();

        // The fold reached the deck that broadcast it...
        assert_eq!(
            active_tool_of(&second_a, "agent-a"),
            Some("Bash".to_string()),
            "the broadcasting deck's own snapshot must show its agent working"
        );
        // ...and only that deck.
        assert_eq!(
            second_b
                .agents
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>(),
            vec!["agent-b"],
            "the other deck's snapshot must carry only its own agents"
        );
        assert_eq!(
            active_tool_of(&second_b, "agent-b"),
            None,
            "one deck's broadcast must not move another deck's agent"
        );

        // And each snapshot is LABELLED with the deck it was answered for. The
        // frontend derives `daemonId` from `connection.socketPath`
        // (`desktop/src/lib/bridge.ts`), so this string is the deck identity as
        // far as every screen is concerned.
        assert_eq!(
            second_a.connection.socket_path,
            deck_a.describe(),
            "a snapshot answered for deck A must name deck A"
        );
        assert_eq!(
            second_b.connection.socket_path,
            deck_b.describe(),
            "a snapshot answered for deck B must name deck B"
        );

        let _ = std::fs::remove_dir_all(dir_a);
        let _ = std::fs::remove_dir_all(dir_b);
    }

    /// **Test-plan item 9.** Scenario: two observed decks are each running an
    /// agent that reports the SAME registry id, and both are snapshotted. The
    /// two snapshots must carry different deck identities, so the frontend's
    /// `(daemonId, agentId)` composite key still tells the two agents apart.
    ///
    /// This is the failure that looks right on screen, which is why it needs a
    /// test rather than review. `AgentOverview.tsx` keys agents by
    /// `(daemonId, agentId)`; the DTO carries exactly ONE `connection`. So a
    /// merge that emits N decks' agents under one identity does not render an
    /// error — it renders one fleet where there were two, and a later action on
    /// such a row sends one deck's agent id to the other deck.
    ///
    /// **PRD #742 M5 moved `daemonId` off `connection.socketPath` and onto
    /// `connection.deckId`, and the assertions below follow it.** Two LOCAL
    /// decks differ in their describe string, so this pair was separable either
    /// way; what was not is two REMOTE rows differing only in socket path,
    /// identity file or jump host — `dto::tests::the_snapshot_fleet_is_the_observed_set_selected_first`
    /// is where that pair is pinned, because it needs a settings document
    /// rather than a listening socket.
    #[cfg(unix)]
    #[tokio::test]
    async fn two_decks_running_the_same_agent_id_never_share_one_deck_identity() {
        let (dir_a, socket_a) = scratch_socket("m3-ident-a");
        let (dir_b, socket_b) = scratch_socket("m3-ident-b");
        let listener_a = bind_trusted(&socket_a);
        let listener_b = bind_trusted(&socket_b);
        // The same id on both decks: a bare-id key collides, and only the deck
        // half of the composite key can separate them.
        let daemon_a = tokio::spawn(scripted_daemon_sequence(
            listener_a,
            vec![
                matching_hello(),
                AttachResponse::agent_records(vec![listed_agent("1", "pane-1")]),
            ],
        ));
        let daemon_b = tokio::spawn(scripted_daemon_sequence(
            listener_b,
            vec![
                matching_hello(),
                AttachResponse::agent_records(vec![listed_agent("1", "pane-1")]),
            ],
        ));

        let links = DaemonLinks::default();
        let deck_a = Endpoint::Local(LocalEndpoint::at(&socket_a));
        let deck_b = Endpoint::Local(LocalEndpoint::at(&socket_b));

        let (snapshot_a, snapshot_b) =
            tokio::join!(snapshot_of(&deck_a, &links), snapshot_of(&deck_b, &links),);

        daemon_a.abort();
        daemon_b.abort();

        assert_eq!(snapshot_a.connection.status, ConnectionStatus::Connected);
        assert_eq!(snapshot_b.connection.status, ConnectionStatus::Connected);
        assert_eq!(snapshot_a.agents.len(), 1);
        assert_eq!(snapshot_b.agents.len(), 1);
        assert_eq!(
            snapshot_a.agents[0].id, snapshot_b.agents[0].id,
            "the fixture must really put the same agent id on both decks, or \
             this test proves nothing"
        );

        assert_ne!(
            snapshot_a.connection.deck_id, snapshot_b.connection.deck_id,
            "two decks must never be emitted under one identity — the composite \
             key collapses and two fleets render as one"
        );
        assert_eq!(snapshot_a.connection.deck_id, deck_wire_id(&deck_a));
        assert_eq!(snapshot_b.connection.deck_id, deck_wire_id(&deck_b));
        assert_ne!(
            snapshot_a.connection.socket_path, snapshot_b.connection.socket_path,
            "and the LABEL beside it still names each deck for a reader"
        );
        assert_eq!(snapshot_a.connection.socket_path, deck_a.describe());
        assert_eq!(snapshot_b.connection.socket_path, deck_b.describe());

        let _ = std::fs::remove_dir_all(dir_a);
        let _ = std::fs::remove_dir_all(dir_b);
    }

    /// A deck that accepts a handshake and then never answers it.
    ///
    /// A **deterministic** stall the test controls rather than a real connect
    /// timeout waited out: [`hello`] has no deadline of its own, so a daemon
    /// that reads the request frame and does not reply parks the caller inside
    /// `establish()` for exactly as long as the test wants it there. Signals
    /// `accepted` once it is holding the connection and holds it until `release`
    /// fires — the write half is kept bound rather than dropped, because
    /// dropping it shuts the socket down and the client would see EOF instead of
    /// a stall.
    #[cfg(unix)]
    async fn unresponsive_daemon(
        listener: tokio::net::UnixListener,
        accepted: tokio::sync::oneshot::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
    ) {
        use dot_agent_deck::daemon_protocol::read_frame;
        let (stream, _peer) = listener.accept().await.expect("accept one client");
        let (mut reader, _writer) = stream.into_split();
        let _ = read_frame(&mut reader).await;
        let _ = accepted.send(());
        let _ = release.await;
    }

    /// A deck that accepts a handshake, stalls until `release`, and then answers
    /// it properly.
    ///
    /// [`unresponsive_daemon`]'s sibling, and the difference is the whole of
    /// what PRD #742 M8's F1 needs: that one parks a caller inside `establish()`
    /// forever, which proves another deck is not queued behind it; this one
    /// parks a caller and then lets it **succeed**, so the test can act on the
    /// map while an establishment is in flight and then watch what that
    /// establishment does with its result.
    #[cfg(unix)]
    async fn stalled_then_answering_daemon(
        listener: tokio::net::UnixListener,
        accepted: tokio::sync::oneshot::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
    ) {
        use dot_agent_deck::daemon_protocol::{KIND_RESP, read_frame, write_frame};
        let (stream, _peer) = listener.accept().await.expect("accept one client");
        let (mut reader, mut writer) = stream.into_split();
        let _ = read_frame(&mut reader).await;
        let _ = accepted.send(());
        let _ = release.await;
        let encoded = serde_json::to_vec(&matching_hello()).expect("serialize the reply");
        let _ = write_frame(&mut writer, KIND_RESP, &encoded).await;
    }

    /// **PRD #742 M8's F1, reached end to end rather than at a seam.**
    /// Scenario: a refresh is mid-handshake against deck `D` when the user
    /// changes the selection. `retarget_selection`'s `invalidate_all()` runs
    /// while `D`'s establishment is parked inside `hello()`; the deck then
    /// answers, and the establishment comes back with a perfectly good
    /// `Connected` link. It must hand that link to its caller and **not** put it
    /// in the map: `D` is a deck nothing observes any more, and a link held for
    /// it holds a `DaemonClient`, a captured capability set and a lease on an
    /// `ssh` child until the next settings save.
    ///
    /// Before M8 the map's own lock did this — it was held across establishment,
    /// so `invalidate_all` could not run in the middle. The per-deck gate M3
    /// replaced it with does not exclude `invalidate_all` at all, and `D` has no
    /// entry to clear while it is being established, so the clear was a no-op
    /// and the insert landed behind it.
    ///
    /// **What this proves:** the real `trusted()`, against a real socket, with
    /// the invalidation genuinely interleaved — the deck has taken the request
    /// frame (`accepted`) and has not answered it (`release` has not fired) when
    /// `invalidate_all` runs. No sleeps and no timing assumptions: both edges
    /// are `oneshot` channels the test fires itself.
    ///
    /// **What it does not prove:** the same property for
    /// `EndpointTunnels::acquire`, whose stall would have to be an `ssh` spawn.
    /// That one is pinned at its publish seam instead —
    /// `endpoint_tunnels::tests::a_transport_whose_deck_left_mid_establishment_is_not_published`.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_established_across_an_invalidation_is_not_published() {
        let (dir, socket) = scratch_socket("m8-strand");
        let listener = bind_trusted(&socket);
        let (accepted, accepted_rx) = tokio::sync::oneshot::channel::<()>();
        let (release, release_rx) = tokio::sync::oneshot::channel::<()>();
        let daemon = tokio::spawn(stalled_then_answering_daemon(
            listener, accepted, release_rx,
        ));

        let links = Arc::new(DaemonLinks::default());
        let deck = Endpoint::Local(LocalEndpoint::at(&socket));
        let establishing = {
            let links = Arc::clone(&links);
            let deck = deck.clone();
            tokio::spawn(async move { links.trusted(&deck).await })
        };

        accepted_rx
            .await
            .expect("the deck must have taken the handshake before the selection moves");
        // The user picked a different deck. This is the whole of what
        // `retarget_selection` does to this map, and `D` is not in it yet.
        links.invalidate_all().await;
        assert_eq!(links.held().await, 0);

        let _ = release.send(());
        let established = establishing
            .await
            .expect("the establishing task must finish")
            .expect("the deck answered, so the handshake itself succeeds");

        assert_eq!(
            established.connection().status,
            ConnectionStatus::Connected,
            "the caller still gets its answer — refusing the publish is not \
             refusing the request"
        );
        assert_eq!(
            links.held().await,
            0,
            "a link established for a deck invalidated mid-handshake must not be \
             held: nothing observes that deck, and the link holds a client, a \
             capability set and a lease on an ssh child"
        );

        let _ = daemon.await;
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The contrast the test above needs to be worth anything. Scenario: the
    /// same stalled-then-answering deck, with **no** invalidation while the
    /// handshake is out. The link must be held, so a second caller reuses it.
    ///
    /// Without this, a `trusted()` that never published anything would pass the
    /// test above.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_established_with_nothing_moving_under_it_is_published() {
        let (dir, socket) = scratch_socket("m8-keep");
        let listener = bind_trusted(&socket);
        let (accepted, accepted_rx) = tokio::sync::oneshot::channel::<()>();
        let (release, release_rx) = tokio::sync::oneshot::channel::<()>();
        let daemon = tokio::spawn(stalled_then_answering_daemon(
            listener, accepted, release_rx,
        ));

        let links = Arc::new(DaemonLinks::default());
        let deck = Endpoint::Local(LocalEndpoint::at(&socket));
        let establishing = {
            let links = Arc::clone(&links);
            let deck = deck.clone();
            tokio::spawn(async move { links.trusted(&deck).await })
        };

        accepted_rx.await.expect("the deck took the handshake");
        let _ = release.send(());
        let established = establishing
            .await
            .expect("the establishing task must finish")
            .expect("the deck answered");
        assert_eq!(established.connection().status, ConnectionStatus::Connected);

        assert_eq!(
            links.held().await,
            1,
            "nothing moved under this establishment, so its link is held"
        );
        assert_eq!(links.handshake_count(), 1);

        let _ = daemon.await;
        let _ = std::fs::remove_dir_all(dir);
    }

    /// **PRD #742 M14.** Scenario: a deck accepts the handshake connection and
    /// never answers it. `hello()` must give up and report that rather than
    /// waiting for a reply that is not coming.
    ///
    /// # What was unbounded, and what it cost
    ///
    /// `issue_command` is a write followed by a `read_response`, and
    /// `read_response` waits for as long as the peer keeps the socket open — so
    /// an accepting-but-silent deck was a future that never completed rather
    /// than an error any caller could observe. A deck that is simply DOWN was
    /// always fine: `connect` fails immediately.
    ///
    /// It is M14's boundedness question because a watcher emits its first
    /// snapshot only after its first reply. Until then the webview renders the
    /// deck as pending — right for the seconds an ssh tunnel takes, wrong
    /// forever — so without this the fleet view has a spinner with no terminal
    /// state, which is worse than the late appearance it replaced.
    ///
    /// # Paused time, not fifteen real seconds — and paused at a POINT
    ///
    /// The clock is stopped only after `accepted_rx` resolves, which is the
    /// peer confirming it took the connection and read the request. That
    /// ordering is the test rather than an optimisation: `start_paused` would
    /// auto-advance from the first moment the runtime had nothing ready to
    /// poll, so a clock that jumped while the connect was still in flight would
    /// produce the same error message for a scenario nobody meant to write.
    /// Pausing here leaves exactly one thing outstanding — a read against a
    /// peer that is never going to write — and the runtime advances to the
    /// timeout because that is the only deadline left. Real socket I/O, real
    /// silence, and none of the fifteen seconds spent.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_deck_that_takes_the_connection_and_never_answers_is_reported_rather_than_awaited() {
        let (dir, socket) = scratch_socket("m14-silent");
        let listener = bind_trusted(&socket);
        let (accepted, accepted_rx) = tokio::sync::oneshot::channel::<()>();
        let (release, release_rx) = tokio::sync::oneshot::channel::<()>();
        let silent = tokio::spawn(unresponsive_daemon(listener, accepted, release_rx));

        let asking = {
            let socket = socket.clone();
            tokio::spawn(async move { hello(&socket).await })
        };
        accepted_rx
            .await
            .expect("the deck must have taken the connection and read the request");
        // Everything outstanding is now parked: this deck's reply, which is
        // never coming, and the stall the test releases below.
        tokio::time::pause();
        /*
            The outer bound is what turns a REGRESSION into a failure instead of
            a hang. Measured: with the timeout taken back out of `hello()` this
            test does not fail, it never returns — which in CI is a job burning
            its whole budget with nothing to read. Under a paused clock tokio
            advances to the NEAREST deadline, so `hello`'s own fifteen seconds
            fire first while it has one, and this fires only when it does not.
            Neither costs wall clock.
        */
        let outcome = tokio::time::timeout(Duration::from_secs(600), asking)
            .await
            .expect("hello() must bound its own wait rather than await a reply that is not coming")
            .expect("the handshake task must not panic");

        let _ = release.send(());
        let _ = silent.await;
        let _ = std::fs::remove_dir_all(dir);

        let error = outcome.expect_err("a deck that never answers must not resolve as connected");
        assert!(
            error.contains("did not answer"),
            "the elapsed case must say the deck took the connection and stalled, \
             rather than reading as a transport failure: {error}"
        );
        assert!(
            error.contains(&DECK_REPLY_TIMEOUT.as_secs().to_string()),
            "and name the bound it exceeded: {error}"
        );
    }

    /// **Test-plan item 10, and the risk this PRD is most likely to fail on.**
    /// Scenario: one observed deck accepts a handshake and never answers it,
    /// while a second, healthy deck is asked for its own. The healthy deck's
    /// handshake must complete promptly rather than queueing behind the
    /// unresponsive one.
    ///
    /// [`DaemonLinks::trusted`] holds **one async mutex over the whole map,
    /// across establishment**, and its own doc calls that the better of the two
    /// — which it is, for one deck: concurrent first-uses of the same deck
    /// collapse into one handshake. At N decks it inverts, because the callers
    /// queued behind the timeout are now OTHER decks. That is success criterion
    /// 2 of PRD #742, stated as a measurement precisely because it fails
    /// silently.
    ///
    /// **The property, not the lock's shape.** Nothing here asserts that a mutex
    /// was split, or that establishment moved out of it; either fix passes. What
    /// is asserted is that a healthy deck's handshake completes while another
    /// deck is mid-connect.
    ///
    /// The bound is five seconds against a sub-millisecond local handshake —
    /// three orders of magnitude of headroom, because `.config/nextest.toml`
    /// keeps `retries = 0` and a flaky timing test here would be worse than no
    /// test at all.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unresponsive_deck_does_not_queue_another_decks_handshake() {
        let (dir_stalled, socket_stalled) = scratch_socket("m3-stall");
        let (dir_healthy, socket_healthy) = scratch_socket("m3-healthy");
        let stalled_listener = bind_trusted(&socket_stalled);
        let healthy_listener = bind_trusted(&socket_healthy);
        let (accepted, accepted_rx) = tokio::sync::oneshot::channel::<()>();
        let (release, release_rx) = tokio::sync::oneshot::channel::<()>();
        let stalled = tokio::spawn(unresponsive_daemon(stalled_listener, accepted, release_rx));
        let healthy = tokio::spawn(scripted_daemon_sequence(
            healthy_listener,
            vec![matching_hello()],
        ));

        let links = Arc::new(DaemonLinks::default());
        let unresponsive = Endpoint::Local(LocalEndpoint::at(&socket_stalled));
        let reachable = Endpoint::Local(LocalEndpoint::at(&socket_healthy));

        let blocked = {
            let links = Arc::clone(&links);
            tokio::spawn(async move {
                let _ = links.trusted(&unresponsive).await;
            })
        };
        accepted_rx
            .await
            .expect("the unresponsive deck must have taken the handshake");

        let outcome = tokio::time::timeout(Duration::from_secs(5), links.trusted(&reachable)).await;

        // Cleanup BEFORE the assertion, so the failing run still lets go of the
        // stalled connection and the scratch sockets.
        let _ = release.send(());
        let _ = stalled.await;
        blocked.abort();
        healthy.abort();
        let _ = std::fs::remove_dir_all(dir_stalled);
        let _ = std::fs::remove_dir_all(dir_healthy);

        let link = outcome
            .expect(
                "a healthy deck's handshake must not be queued behind an \
                 unreachable deck's connect — PRD #742 success criterion 2",
            )
            .expect("the healthy deck answered, so its link must establish");
        assert_eq!(link.connection().status, ConnectionStatus::Connected);
    }

    // -----------------------------------------------------------------------
    // PRD #742 M9 — the same fleet properties, against two REAL daemons
    //
    // Everything above this line answers a `scripted_daemon`: a hand-written
    // socket responder that replies with whatever the test handed it. Those are
    // genuine concurrency tests and they stay — they pin edge cases a real
    // server makes awkward (a daemon that accepts and never answers, a reply
    // sequence chosen per connection, a listing the test controls byte for
    // byte). What they cannot show is that the client folds and stamps
    // correctly against the thing it actually talks to.
    //
    // These run the PRODUCTION attach server in-process, twice, on two sockets.
    // What that adds over the scripted pair, item by item:
    //
    //   - `bind_attach_listener` creates the inode with the production
    //     umask-before-`bind(2)` dance, so `verify_endpoint_trusted`'s uid and
    //     exactly-`0o600` predicate runs against a socket a real server made
    //     rather than one `bind_trusted` restated the mode on afterwards;
    //   - the `Hello` reply, its `running_agents` summary and its capability
    //     advertisement are the daemon's own, not a fixture's;
    //   - `ListAgents` is the real handler joining a real `AgentPtyRegistry`
    //     holding real PTY children, so the records under test are minted by
    //     the registry rather than written by the test;
    //   - `SubscribeEvents` is a real push stream, so "a fold never crosses
    //     decks" can be asserted on the WIRE — deck B's subscription never
    //     carrying deck A's broadcast — rather than on two `AgentView` objects
    //     a test hand-fed.
    //
    // `examples/perf_baseline_probe.rs` is the worked precedent for binding the
    // production server over a Unix socket and driving real agents through it.
    // -----------------------------------------------------------------------

    /// One real daemon: the production attach server, bound by production code,
    /// serving a real registry over a real Unix socket.
    ///
    /// **The bind happens on the calling thread, not inside the spawned task**,
    /// which is why nothing here polls for the socket to appear:
    /// [`bind_attach_listener`] creates the inode before `start` returns and
    /// `serve_attach` is handed the listener it created. The perf probe's
    /// connect-until-it-works loop exists because it calls
    /// `run_attach_server_with_counter`, which binds inside the task; splitting
    /// the bind from the accept loop removes the race rather than waiting it
    /// out.
    ///
    /// [`bind_attach_listener`] flips the **process** umask around its
    /// `bind(2)`, which `scripted_daemon` above deliberately avoids. That is
    /// safe here and the difference is the test runner: `cargo test-fast` is
    /// nextest, which is process-per-test, so the flip is private to this test.
    /// Under a plain `cargo test` the whole module shares one process and the
    /// flip is momentary but global — and it is accepted rather than avoided,
    /// because a socket this test created some other way would not be the thing
    /// the trust check is supposed to be running against.
    ///
    /// "Momentary" is a property of the production helper rather than of this
    /// call site, and since PRD #742 M11 it holds on the unwind path too:
    /// `platform::fsperm::with_socket_umask` restores from a `Drop`, so a body
    /// that panicked could not leave the process at `0o177`. Nothing here needs
    /// a scope guard of its own — unlike `project/resolve/002`'s cwd, the flip
    /// is confined to one `bind(2)` inside production code and is already back
    /// before `start` returns.
    ///
    /// [`bind_attach_listener`]: dot_agent_deck::daemon_protocol::bind_attach_listener
    #[cfg(unix)]
    struct RealDeck {
        dir: std::path::PathBuf,
        endpoint: Endpoint,
        registry: Arc<dot_agent_deck::agent_pty::AgentPtyRegistry>,
        /// The daemon-wide broadcast every `SubscribeEvents` stream forwards.
        /// Holding the `Sender` is how a test makes a REAL daemon push a real
        /// event frame to its own subscribers and to nobody else's.
        events: tokio::sync::broadcast::Sender<dot_agent_deck::event::BroadcastMsg>,
        server: tokio::task::JoinHandle<()>,
    }

    #[cfg(unix)]
    impl RealDeck {
        fn start(tag: &str) -> Self {
            use dot_agent_deck::daemon_protocol::{bind_attach_listener, serve_attach};
            let (dir, socket) = scratch_socket(tag);
            let registry = Arc::new(dot_agent_deck::agent_pty::AgentPtyRegistry::new());
            // The initial receiver is dropped immediately: a broadcast channel
            // stays open with none, and every reader in these tests is a real
            // `SubscribeEvents` connection the daemon subscribes on its own.
            let (events, _initial) = tokio::sync::broadcast::channel(64);
            let listener = bind_attach_listener(&socket).expect("bind the real attach socket");
            let server = {
                let registry = Arc::clone(&registry);
                let events = events.clone();
                tokio::spawn(async move {
                    let _ = serve_attach(listener, registry, events).await;
                })
            };
            Self {
                dir,
                endpoint: Endpoint::Local(LocalEndpoint::at(&socket)),
                registry,
                events,
                server,
            }
        }

        /// A real PTY child under this daemon's registry, and the **registry's**
        /// own id for it.
        ///
        /// `cat` for the same reason the perf probe uses it: it reads its PTY
        /// and blocks, so the record stays live for the test's duration without
        /// any keep-alive of its own.
        ///
        /// Every registry mints ids from its own counter starting at 1, so the
        /// first agent on each of two decks is id `"1"` — which is the collision
        /// a bare-id key gets wrong, arrived at by the registry rather than
        /// staged by the test.
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

        /// Stop answering, and reap the PTY children — the socket inode is left
        /// in place, so a client meeting this deck afterwards gets
        /// `ECONNREFUSED` rather than a missing file. That is what a killed
        /// daemon looks like to a held link.
        fn kill_server(&self) {
            self.server.abort();
            self.registry.shutdown_all();
        }

        fn shutdown(self) {
            self.kill_server();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The real-daemon sibling of
    /// [`tests::two_decks_running_the_same_agent_id_never_share_one_deck_identity`].
    /// Scenario: two production attach servers are running on two sockets, each
    /// with one real PTY agent, and both are snapshotted concurrently. Both
    /// registries mint `"1"` for their first agent, so the two snapshots carry
    /// the same agent id and must still carry different deck identities.
    ///
    /// **What the real server adds.** The scripted version writes the colliding
    /// id into its own `ListAgents` reply; here the collision is produced by two
    /// independent `AgentPtyRegistry` id counters, which is where it comes from
    /// in production. The `Hello` each deck answers is the daemon's own, so the
    /// classification, the capability capture and the running-agent summary are
    /// all on the real path, and `verify_endpoint_trusted` runs against an inode
    /// `bind_attach_listener` created.
    ///
    /// **What it does not prove.** Two LOCAL decks differ in their describe
    /// string as well as in their identity, so this pair is separable either
    /// way; the pair that is *not* — two remote rows differing only in socket
    /// path, identity file or jump host — needs a settings document rather than
    /// a listening socket and stays at
    /// `dto::tests::the_snapshot_fleet_is_the_observed_set_selected_first`. It
    /// also says nothing about what the webview renders: no test in this
    /// repository can (#953).
    #[cfg(unix)]
    #[tokio::test]
    async fn two_real_daemons_serving_the_same_agent_id_never_share_one_deck_identity() {
        let deck_a = RealDeck::start("m9-ident-a");
        let deck_b = RealDeck::start("m9-ident-b");
        let id_a = deck_a.spawn_agent("pane-a");
        let id_b = deck_b.spawn_agent("pane-b");
        assert_eq!(
            id_a, id_b,
            "two registries must really mint the same first id, or this test \
             proves nothing"
        );

        let links = DaemonLinks::default();
        let (snapshot_a, snapshot_b) = tokio::join!(
            snapshot_of(&deck_a.endpoint, &links),
            snapshot_of(&deck_b.endpoint, &links),
        );

        let endpoint_a = deck_a.endpoint.clone();
        let endpoint_b = deck_b.endpoint.clone();
        deck_a.shutdown();
        deck_b.shutdown();

        assert_eq!(snapshot_a.connection.status, ConnectionStatus::Connected);
        assert_eq!(snapshot_b.connection.status, ConnectionStatus::Connected);
        assert_eq!(
            snapshot_a
                .agents
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            vec![id_a.as_str()],
        );
        assert_eq!(
            snapshot_b
                .agents
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            vec![id_b.as_str()],
        );

        assert_ne!(
            snapshot_a.connection.deck_id, snapshot_b.connection.deck_id,
            "two real daemons must never be emitted under one identity — the \
             frontend's (daemonId, agentId) key collapses and two fleets render \
             as one"
        );
        assert_eq!(snapshot_a.connection.deck_id, deck_wire_id(&endpoint_a));
        assert_eq!(snapshot_b.connection.deck_id, deck_wire_id(&endpoint_b));
        assert_ne!(
            snapshot_a.connection.socket_path, snapshot_b.connection.socket_path,
            "and the label beside the key still names each deck for a reader"
        );
    }

    /// The real-daemon sibling of
    /// [`tests::a_fold_for_one_deck_never_reaches_another_decks_snapshot`], and
    /// the one place the isolation is asserted on the **wire** rather than on
    /// two objects. Scenario: two production attach servers each run one agent
    /// that reports the same registry id `"1"` on the same pane id, the client
    /// holds a real `SubscribeEvents` stream to each, and deck A broadcasts a
    /// `ToolStart`. Deck A's stream must carry it, deck B's stream must not, and
    /// the two snapshots must show each deck's own tool.
    ///
    /// **What the real server adds.** The scripted version hand-applies the
    /// event to `view_a`, so all it can show is that two `AgentView`s are two
    /// objects — which M3's Work Log records as a property that "cannot fail".
    /// Here the event goes into deck A's real daemon-wide broadcast, is
    /// serialised by the real `handle_subscribe_events`, and is read back off a
    /// real socket; the assertion that deck B's subscription's **first** frame
    /// is B's own sentinel is what pins that A's broadcast never reached B's
    /// stream. A's was sent first, so anything that could cross would already be
    /// queued ahead of it.
    ///
    /// The two agents deliberately share **both** the registry id and the pane
    /// id, so nothing but per-deck isolation separates the two folds.
    ///
    /// **What it does not prove.** That the *watcher* stamps its own endpoint on
    /// what it emits — that needs an `AppHandle` and is the `R: Runtime` refactor
    /// deferred to #953, recorded as a residual at M3 and unchanged here.
    #[cfg(unix)]
    #[tokio::test]
    async fn one_real_decks_broadcast_never_reaches_another_real_decks_stream_or_fold() {
        let deck_a = RealDeck::start("m9-fold-a");
        let deck_b = RealDeck::start("m9-fold-b");
        let id_a = deck_a.spawn_agent("pane-1");
        let id_b = deck_b.spawn_agent("pane-1");
        assert_eq!(id_a, id_b, "both decks must really run the same agent id");

        let links = DaemonLinks::default();
        let mut view_a = AgentView::default();
        let mut view_b = AgentView::default();
        let (first_a, first_b) = tokio::join!(
            snapshot_with(&deck_a.endpoint, &links, Some(&mut view_a)),
            snapshot_with(&deck_b.endpoint, &links, Some(&mut view_b)),
        );
        assert_eq!(first_a.connection.status, ConnectionStatus::Connected);
        assert_eq!(first_b.connection.status, ConnectionStatus::Connected);

        // Two real subscriptions, held at once — the fleet's shape, and the
        // thing #741 never had.
        let mut stream_a = links
            .trusted(&deck_a.endpoint)
            .await
            .expect("deck A is up")
            .client
            .subscribe_events()
            .await
            .expect("subscribe to deck A");
        let mut stream_b = links
            .trusted(&deck_b.endpoint)
            .await
            .expect("deck B is up")
            .client
            .subscribe_events()
            .await
            .expect("subscribe to deck B");

        // Deck A broadcasts FIRST, so anything able to cross has a head start.
        deck_a
            .events
            .send(tool_event_for("pane-1", &id_a, "Bash"))
            .expect("deck A's subscriber is registered");
        let from_a = stream_a
            .next_event()
            .await
            .expect("deck A's stream is healthy")
            .expect("deck A pushed its own broadcast");
        view_a.apply(&from_a);

        deck_b
            .events
            .send(tool_event_for("pane-1", &id_b, "Grep"))
            .expect("deck B's subscriber is registered");
        let from_b = stream_b
            .next_event()
            .await
            .expect("deck B's stream is healthy")
            .expect("deck B pushed its own broadcast");
        view_b.apply(&from_b);

        let (second_a, second_b) = tokio::join!(
            snapshot_with(&deck_a.endpoint, &links, Some(&mut view_a)),
            snapshot_with(&deck_b.endpoint, &links, Some(&mut view_b)),
        );

        drop(stream_a);
        drop(stream_b);
        deck_a.shutdown();
        deck_b.shutdown();

        // The wire half: B's first frame was B's own, so A's broadcast was
        // never on B's stream.
        assert_eq!(
            tool_name_of(&from_b),
            Some("Grep".to_string()),
            "deck B's subscription must never carry deck A's broadcast"
        );
        assert_eq!(tool_name_of(&from_a), Some("Bash".to_string()));

        // The fold half: same agent id, same pane id, different decks.
        assert_eq!(
            active_tool_of(&second_a, &id_a),
            Some("Bash".to_string()),
            "the broadcasting deck's own snapshot must show its agent working"
        );
        assert_eq!(
            active_tool_of(&second_b, &id_b),
            Some("Grep".to_string()),
            "and the other deck's snapshot must show ITS agent's tool, not A's"
        );
    }

    /// The tool name a broadcast names, for the wire-level half of the fold
    /// test — `active_tool_of` reads a rendered snapshot, this reads the frame
    /// that produced it.
    #[cfg(unix)]
    fn tool_name_of(msg: &dot_agent_deck::event::BroadcastMsg) -> Option<String> {
        match msg {
            dot_agent_deck::event::BroadcastMsg::Event(event) => event.tool_name.clone(),
            _ => None,
        }
    }

    /// **PRD #742 success criterion 2, half one: a deck that STOPPED answering.**
    /// Scenario: two production attach servers are up and both have been
    /// snapshotted, so the client holds a link to each; one server is then
    /// killed and both decks are refreshed concurrently. The dead deck must
    /// degrade on its own — a disconnected snapshot carrying its own name — and
    /// the survivor must answer in full, with its own agent, inside a bound.
    ///
    /// **What the real server adds.** The scripted decks cannot be killed in a
    /// way that resembles a killed daemon: a scripted responder either answers
    /// or was told in advance not to. Here the accept loop is aborted with the
    /// socket inode left in place, which is exactly what a `daemon stop` leaves
    /// behind, so the survivor's refresh races a held link whose client is about
    /// to meet `ECONNREFUSED` — the path `snapshot_with` invalidates on and the
    /// one that decides whether a dead deck's failure is per-deck or fleet-wide.
    ///
    /// **What it does not prove.** That an unreachable deck does not *queue*
    /// another deck's handshake: a killed daemon fails fast, so the timeout here
    /// bounds a degradation rather than a stall. The stall is the sibling test
    /// below, and the deterministic scripted version is
    /// [`tests::an_unresponsive_deck_does_not_queue_another_decks_handshake`].
    #[cfg(unix)]
    #[tokio::test]
    async fn a_killed_real_deck_degrades_alone_and_the_survivor_answers_in_full() {
        let survivor = RealDeck::start("m9-alive");
        let doomed = RealDeck::start("m9-doomed");
        let survivor_agent = survivor.spawn_agent("pane-alive");
        doomed.spawn_agent("pane-doomed");

        let links = DaemonLinks::default();
        let (before_survivor, before_doomed) = tokio::join!(
            snapshot_of(&survivor.endpoint, &links),
            snapshot_of(&doomed.endpoint, &links),
        );
        assert_eq!(
            before_survivor.connection.status,
            ConnectionStatus::Connected
        );
        assert_eq!(before_doomed.connection.status, ConnectionStatus::Connected);
        assert_eq!(links.held().await, 2, "both decks are held");

        // The daemon goes away; its socket inode does not, which is what a held
        // link meets in production.
        doomed.kill_server();

        let outcome = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                snapshot_of(&survivor.endpoint, &links),
                snapshot_of(&doomed.endpoint, &links),
            )
        })
        .await;

        let survivor_endpoint = survivor.endpoint.clone();
        let doomed_endpoint = doomed.endpoint.clone();
        survivor.shutdown();
        doomed.shutdown();

        let (after_survivor, after_doomed) = outcome.expect(
            "one dead deck must not stall another deck's refresh — PRD #742 \
             success criterion 2",
        );
        assert_eq!(
            after_survivor.connection.status,
            ConnectionStatus::Connected,
            "the surviving deck must keep answering: {:?}",
            after_survivor.connection.error
        );
        assert_eq!(
            after_survivor
                .agents
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            vec![survivor_agent.as_str()],
            "and must still carry its own agent"
        );
        assert_eq!(
            after_survivor.connection.deck_id,
            deck_wire_id(&survivor_endpoint)
        );

        assert_eq!(
            after_doomed.connection.status,
            ConnectionStatus::Disconnected,
            "the dead deck must report its own failure"
        );
        assert!(after_doomed.connection.error.is_some(), "and must say why");
        assert!(after_doomed.agents.is_empty());
        assert_eq!(
            after_doomed.connection.deck_id,
            deck_wire_id(&doomed_endpoint),
            "a degraded deck is still named as itself, or the fleet cannot show \
             which group went down"
        );
    }

    /// **PRD #742 success criterion 2, half two: a deck that ACCEPTS and never
    /// answers.** Scenario: one deck's socket is bound by the production binder
    /// and a connection on it is accepted and then held without a reply, so a
    /// `trusted()` against it is parked inside `hello()` for as long as the test
    /// wants; a second, fully real deck is then asked for a complete snapshot.
    /// That snapshot must come back inside a bound, with the real deck's own
    /// agent in it.
    ///
    /// **What the real server adds over the scripted sibling.** The scripted
    /// version's healthy deck answers exactly one canned `Hello` frame, so it
    /// pins `DaemonLinks::trusted` and stops there. Here the healthy side is a
    /// production attach server and the assertion is a whole `snapshot_of` —
    /// handshake, capability capture and a real `ListAgents` join — so a
    /// regression that unqueued the handshake and requeued the listing would be
    /// caught. The stalled side's inode is created by `bind_attach_listener`,
    /// so the trust check it passes on the way in is the production one.
    ///
    /// **What is scripted here, stated rather than implied.** Only the
    /// *withholding of the reply*. A real daemon always answers, so a
    /// deterministic stall cannot be built out of one — the connection is
    /// accepted on a production-bound listener and then simply not served, which
    /// is the condition being modelled (a daemon wedged before its reply) and
    /// not a fixture standing in for one. The `accepted` oneshot is what makes
    /// it deterministic: the healthy deck is not asked for anything until the
    /// stalled deck is provably holding the handshake.
    ///
    /// The five-second bound is three orders of magnitude over a sub-millisecond
    /// local snapshot, for the reason the scripted sibling gives:
    /// `.config/nextest.toml` keeps `retries = 0`, so a flaky timing test here
    /// would be worse than no test at all.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unresponsive_deck_does_not_queue_a_real_decks_whole_snapshot() {
        use dot_agent_deck::daemon_protocol::{bind_attach_listener, read_frame};

        let (stalled_dir, stalled_socket) = scratch_socket("m9-stall");
        let stalled_listener =
            bind_attach_listener(&stalled_socket).expect("bind the stalled deck's socket");
        let healthy = RealDeck::start("m9-healthy");
        let healthy_agent = healthy.spawn_agent("pane-healthy");

        let (accepted, accepted_rx) = tokio::sync::oneshot::channel::<()>();
        let (release, release_rx) = tokio::sync::oneshot::channel::<()>();
        let stalled_server = tokio::spawn(async move {
            let stream = stalled_listener.accept().await.expect("accept one client");
            // The write half is kept bound: dropping it half-closes the socket
            // and the client would see EOF instead of a stall.
            let (mut reader, _writer) = stream.into_split();
            let _ = read_frame(&mut reader).await;
            let _ = accepted.send(());
            let _ = release_rx.await;
        });

        let links = Arc::new(DaemonLinks::default());
        let stalled_endpoint = Endpoint::Local(LocalEndpoint::at(&stalled_socket));
        let blocked = {
            let links = Arc::clone(&links);
            tokio::spawn(async move {
                let _ = links.trusted(&stalled_endpoint).await;
            })
        };
        accepted_rx.await.expect(
            "the stalled deck must be holding the handshake before the \
                     healthy deck is asked for anything",
        );

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            snapshot_of(&healthy.endpoint, &links),
        )
        .await;

        // Cleanup BEFORE the assertions, so a failing run still lets go of the
        // parked connection, the PTY child and the scratch sockets.
        let _ = release.send(());
        let _ = stalled_server.await;
        blocked.abort();
        let healthy_endpoint = healthy.endpoint.clone();
        healthy.shutdown();
        let _ = std::fs::remove_dir_all(&stalled_dir);

        let snapshot = outcome.expect(
            "a healthy deck's whole snapshot must not be queued behind an \
             unresponsive deck's handshake — PRD #742 success criterion 2",
        );
        assert_eq!(
            snapshot.connection.status,
            ConnectionStatus::Connected,
            "{:?}",
            snapshot.connection.error
        );
        assert_eq!(
            snapshot
                .agents
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            vec![healthy_agent.as_str()],
            "the listing has to have completed too, not only the handshake"
        );
        assert_eq!(snapshot.connection.deck_id, deck_wire_id(&healthy_endpoint));
    }

    /// **Fleet membership against real servers.** Scenario: two production
    /// attach servers are observed and both held; the teardown pair
    /// `retarget_selection` runs — `DaemonLinks::invalidate_all` followed by
    /// `EndpointTunnels::retain` over the surviving key — is then applied, and
    /// the surviving deck is refreshed. The departed deck must stop being held,
    /// and the survivor's snapshot must be unaffected: connected, its own agent,
    /// its own identity, with nothing re-established for the deck that left.
    ///
    /// **What the real server adds.** That the survivor really answers after the
    /// teardown rather than returning something cached — its link was cleared by
    /// `invalidate_all`, so the snapshot below is a fresh handshake and a fresh
    /// listing against a daemon that has been running the whole time.
    ///
    /// **What it does not prove.** The settings-document half. `observed_fleet`
    /// and `DesktopSnapshot.fleet` are derived from the applied
    /// `EndpointSettings`, and a stored `[[endpoints.remote]]` row is remote by
    /// construction (`EndpointSettings::connectable_endpoints` leads with
    /// `Endpoint::local()` and extends with remote rows only) — so a fleet of
    /// two *local* real daemons cannot be expressed as a document at all. That
    /// half stays at `dto::tests::the_snapshot_fleet_is_the_observed_set_selected_first`,
    /// which needs a document and no listening socket. What is asserted here is
    /// the map-level half a document cannot reach: the transports and links
    /// themselves.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_real_deck_that_left_the_fleet_stops_being_held_and_the_survivor_is_unaffected() {
        use std::collections::HashSet;

        let kept = RealDeck::start("m9-kept");
        let dropped = RealDeck::start("m9-dropped");
        let kept_agent = kept.spawn_agent("pane-kept");
        dropped.spawn_agent("pane-dropped");

        let links = DaemonLinks::default();
        let tunnels = links.tunnels();
        let (kept_first, dropped_first) = tokio::join!(
            snapshot_of(&kept.endpoint, &links),
            snapshot_of(&dropped.endpoint, &links),
        );
        assert_eq!(kept_first.connection.status, ConnectionStatus::Connected);
        assert_eq!(dropped_first.connection.status, ConnectionStatus::Connected);
        assert_eq!(links.held().await, 2);
        assert_eq!(tunnels.held().await, 2);

        // Exactly what `retarget_selection` does to these two maps, in its own
        // order, with `dropped` no longer in the observed set.
        let observed: HashSet<_> = std::iter::once(kept.endpoint.identity()).collect();
        links.invalidate_all().await;
        tunnels.retain(&observed).await;
        assert_eq!(links.held().await, 0, "invalidate_all clears both links");
        assert_eq!(
            tunnels.held().await,
            1,
            "and retain keeps exactly the observed deck's transport"
        );

        let after = snapshot_of(&kept.endpoint, &links).await;

        let kept_endpoint = kept.endpoint.clone();
        let held_links = links.held().await;
        let held_tunnels = tunnels.held().await;
        kept.shutdown();
        dropped.shutdown();

        assert_eq!(
            after.connection.status,
            ConnectionStatus::Connected,
            "the surviving deck must re-handshake and answer: {:?}",
            after.connection.error
        );
        assert_eq!(
            after
                .agents
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            vec![kept_agent.as_str()],
            "with its own agent and nobody else's"
        );
        assert_eq!(after.connection.deck_id, deck_wire_id(&kept_endpoint));
        assert_eq!(
            held_links, 1,
            "only the observed deck is held again — a refresh must not \
             re-establish the deck that left"
        );
        assert_eq!(
            held_tunnels, 1,
            "and its transport is not re-acquired either"
        );
    }
}
