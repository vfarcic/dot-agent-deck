use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use dot_agent_deck::daemon_attach::{
    DAEMON_START_POLL_TIMEOUT, ensure_daemon_running, spawn_daemon_serve_detached_with_exe,
};
use dot_agent_deck::daemon_client::{DaemonClient, Endpoint, issue_command};
#[cfg(test)]
use dot_agent_deck::daemon_protocol::RunningAgentsSummary;
use dot_agent_deck::daemon_protocol::{AttachRequest, AttachResponse, PROTOCOL_VERSION};
use dot_agent_deck::platform::ipc::IpcStream;
use tokio::sync::Mutex as AsyncMutex;

use crate::agent_view::AgentView;
use crate::dto::{
    BootstrapOptions, ConnectionStatus, DesktopConnection, DesktopSnapshot, disconnected_snapshot,
    map_agent, safe_message, selected_endpoint, selection_fields, socket_path_text,
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
    /// The protocol agreed and the two builds' release versions still disagreed
    /// (or one of them could not be read), so an override is legitimate. Never
    /// set when the wire itself is incompatible, and no longer set by a stamp
    /// difference *within* one release — see [`release_versions_are_compatible`].
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
/// 1. **The population of one.** There is exactly one `EventSubscription` per
///    desktop process — `DesktopState::start_watcher_once` guarantees a single
///    watcher — so half the muxing target is a set with one member in it.
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
/// # Concurrency
///
/// One async mutex over the whole map, held across establishment. That
/// serialises concurrent first-uses into ONE handshake instead of N, which is
/// the point; the cache-hit path is an uncontended lock acquire. A deck that is
/// not answering queues the other callers behind one connect timeout rather
/// than giving each its own, which is also the better of the two.
pub(crate) struct DaemonLinks {
    links: AsyncMutex<HashMap<String, Arc<TrustedDaemon>>>,
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
    pub(crate) async fn trusted(&self, endpoint: &Endpoint) -> Result<Arc<TrustedDaemon>, String> {
        let key = endpoint.describe();
        let mut links = self.links.lock().await;
        if let Some(held) = links.get(&key)
            && held.is_fresh(Instant::now())
        {
            return Ok(Arc::clone(held));
        }
        links.remove(&key);
        let established = Arc::new(establish(endpoint, &self.tunnels).await?);
        self.handshakes.fetch_add(1, Ordering::Relaxed);
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
        if established.connection.status == ConnectionStatus::Connected {
            links.insert(key, Arc::clone(&established));
        }
        Ok(established)
    }

    /// The shared transport map, for `DesktopState` and for the commands that
    /// tear a tunnel down (PRD #741 M7).
    pub(crate) fn tunnels(&self) -> Arc<EndpointTunnels> {
        Arc::clone(&self.tunnels)
    }

    /// Forget the link for `endpoint`, so the next [`Self::trusted`] handshakes
    /// again.
    pub(crate) async fn invalidate(&self, endpoint: &Endpoint) {
        self.links.lock().await.remove(&endpoint.describe());
    }

    /// Forget every link. Used where the reason to distrust the held state is
    /// not specific to one deck — the watcher losing its event stream, and the
    /// in-app build-mismatch allowance, whose entire effect is that the
    /// handshake must be classified again.
    pub(crate) async fn invalidate_all(&self) {
        self.links.lock().await.clear();
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

/// What a build-stamp difference MEANS for this kind of deck (PRD #741 M8).
///
/// # The three layers, kept apart
///
/// Issue #801's framing, adopted here for the `Remote` arm only:
///
/// | question | mechanism | this type |
/// |---|---|---|
/// | can we decode each other's frames at all? | [`PROTOCOL_VERSION`] | untouched — exact equality, for every deck kind, never bypassable |
/// | may I use *this* verb? | the `Hello` reply's advertised capability set | [`project_actions_reason`] |
/// | are we the same build? | the git-describe stamp | **this type** |
///
/// # Why the answer differs by deck kind
///
/// For a **local** deck the desktop bundles its own sidecar and starts it, so
/// lockstep is a property the app can actually hold — and **Replace daemon** is
/// a remedy the user can actually take. Refusing is right there and nothing
/// about it changes.
///
/// For a **remote** deck neither half survives. The remedy `:263-273` offers is
/// *"use Replace daemon to start the matching bundled build"*, which against a
/// deck on another host means terminating a daemon someone else may be using —
/// and PRD #741 M2 made that structurally impossible anyway, so the sentence
/// names an action whose button is disabled on the same screen that prints it.
/// **A remedy that cannot be taken is worse than none**: it reads as the user's
/// fault for not taking it. And the refusal is not rare. A released daemon never
/// matches a branch build, and the far host's daemon upgrades on its own
/// cadence, so "refuse on any stamp difference" against a remote deck is
/// "refuse most of the time".
///
/// # When this type is consulted at all — read this before testing it by hand
///
/// [`classify_handshake`] reaches the stamp branch only when the protocol
/// versions are **equal** and [`release_versions_are_compatible`] says `false`,
/// so two conditions must hold at once and the obvious hand test satisfies
/// neither. Building two commits of the same branch gives two stamps like
/// `0.39.4-g…`, [`compatibility_key`] maps both to `(0, 39)`, and the
/// classification falls through to `Connected` **for both deck kinds without
/// consulting this type** — which looks exactly like the remote demotion
/// working. It is not: a tester who concludes M8 works from that has tested
/// nothing, and would see the same result with this type deleted.
///
/// Two situations do reach it. A **released pair whose compatibility keys
/// differ** — while `0.x`, a minor bump, which by CLAUDE.md rule 12's bump
/// policy is exactly what a compatibility break is versioned as — provided the
/// two builds still agree on [`PROTOCOL_VERSION`], since the protocol check
/// returns first otherwise. And the **unreadable-stamp fail-safe**: a stamp
/// absent or unparseable on either side makes `release_versions_are_compatible`
/// return `false` on no positive evidence, which is the cheaper of the two to
/// stage by hand. Note the corollary for the current tree — protocol 9 is
/// unreleased, so no released build pairs with it across a key difference, and
/// the fail-safe is the only route there is today. The assertion in
/// `the_stamp_policy_follows_the_deck_kind` is therefore what pins the policy;
/// an end-to-end hand test is not a substitute for it.
///
/// # What is LOST by demoting it, stated rather than implied
///
/// A **semantic break behind a stable wire** — a field whose meaning changed
/// while its shape did not — is **not mechanically detectable**. No capability
/// string sees it, because the verb is still advertised and still answers. No
/// version digit sees it, because `docs/develop/versioning.md` is explicit that
/// such a break deliberately does not move [`PROTOCOL_VERSION`]. And the stamp
/// does not see it either, in the case that matters most: a development build's
/// `git describe` names the **last** release, so a branch carrying an unreleased
/// semantic break describes as compatible with the release it was cut from (see
/// [`release_versions_are_compatible`]'s own residual note).
///
/// What actually stands between a remote user and a silently wrong field is
/// CLAUDE.md rule 12's cross-version manual test, run against the previous
/// release before such a change merges, and the `.breaking.md` fragment its
/// outcome demands — which is what turns the break into a minor bump that
/// [`release_versions_are_compatible`] can then see. Before this type there were
/// two backstops for a remote deck and one of them was noisy; there is now
/// **one**, and it is a procedure rather than a mechanism. That is the price of
/// the demotion and it is accepted, not hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StampPolicy {
    /// `Local`: a stamp difference the release versions do not excuse refuses
    /// the connection, exactly as it did before this type existed.
    Enforced,
    /// `Remote`: a stamp difference is reported and connected through.
    Informational,
}

impl StampPolicy {
    /// The policy for a deck, decided by its kind and by nothing else.
    ///
    /// Deliberately a `match` on the endpoint rather than a flag somebody sets:
    /// the kind is the whole of the argument above, so anything that could set
    /// it independently would be a way to get the remote policy on a local deck.
    pub(crate) fn for_endpoint(endpoint: &Endpoint) -> Self {
        match endpoint {
            Endpoint::Local(_) => Self::Enforced,
            Endpoint::Remote(_) => Self::Informational,
        }
    }
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

/// The `MAJOR.MINOR.PATCH` a build stamp opens with, or `None` when it does not
/// open with one.
///
/// A stamp is `<version>-g<short-sha>` with an optional commit distance and an
/// optional `-dirty` suffix — `0.39.0-g1ea0fe7`, `0.39.0-49-ga0165f8`,
/// `0.39.0-g1ea0fe7-dirty` — and `<version>` may itself carry a SemVer
/// prerelease or build-metadata suffix (`0.25.0-alpha.0-g1ea0fe7`). Everything
/// from the first `-` or `+` onward is therefore discarded and only the three
/// core digits are read. A leading `v` is tolerated because that is the shape a
/// git tag has, even though `DAD_BUILD_ID` strips it.
fn release_core(stamp: &str) -> Option<(u64, u64, u64)> {
    fn field(text: &str) -> Option<u64> {
        // `u64::from_str` accepts a leading `+`; the explicit digit test keeps
        // the accepted set to exactly what a version field can look like.
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        text.parse().ok()
    }

    let stamp = stamp.trim();
    let stamp = stamp.strip_prefix(['v', 'V']).unwrap_or(stamp);
    let core = stamp.split(['-', '+']).next()?;
    let mut fields = core.split('.');
    let (major, minor, patch) = (fields.next()?, fields.next()?, fields.next()?);
    if fields.next().is_some() {
        return None;
    }
    Some((field(major)?, field(minor)?, field(patch)?))
}

/// The digits of a release version that move when a compatibility break is
/// declared, per `docs/develop/versioning.md`.
///
/// While the major version is `0` the bump rules are deliberately shifted down
/// one level from standard SemVer, so a protocol/handler break bumps the
/// **minor** while a feature or a bugfix bumps the patch: the key is
/// `0.MINOR`. From `1.0` onward the rules are standard and only the **major**
/// moves on a break, so the minor is dropped from the key rather than left in
/// it. Both arms are encoded because a hardcoded `major.minor` would silently
/// start refusing compatible peers the day this repo ships `1.0`.
fn compatibility_key(stamp: &str) -> Option<(u64, u64)> {
    let (major, minor, _patch) = release_core(stamp)?;
    Some(if major == 0 { (0, minor) } else { (major, 0) })
}

/// Whether the two builds' *release versions* declare them compatible.
///
/// `false` unless BOTH stamps parsed and their keys matched, so an absent,
/// truncated or otherwise unreadable stamp on either side falls back to the
/// prompt. Silent connection is the permissive answer and is reached only on
/// positive evidence — the fail-safe direction is what the tests below pin.
///
/// **The residual this does not close, and must not be read as closing.** A
/// development build's `git describe` names the LAST release, so a branch
/// carrying an *unreleased* semantic break describes as the release it was cut
/// from and reads as compatible with it. Nothing in a build stamp can see that
/// break: the whole point of the `.breaking.md` discipline is that a same-wire,
/// different-meaning change is not mechanically detectable. It is backstopped
/// by CLAUDE.md rule 12's cross-version manual test — run against the previous
/// release before such a change merges — and by the fragment that test's
/// outcome demands, which is what turns the break into a minor bump this
/// function can then see. This reads released versions; it says nothing about
/// unreleased ones.
fn release_versions_are_compatible(client_build: &str, daemon_build: Option<&str>) -> bool {
    let Some(daemon_build) = daemon_build else {
        return false;
    };
    match (
        compatibility_key(client_build),
        compatibility_key(daemon_build),
    ) {
        (Some(client), Some(daemon)) => client == daemon,
        _ => false,
    }
}

fn classify_handshake(
    response: &AttachResponse,
    client_build: &str,
    allowance: BuildMismatchAllowance,
    stamps: StampPolicy,
) -> HandshakeInfo {
    let server_protocol_version = response.server_version;
    let daemon_build_version = response.build_version.clone();
    let daemon_version = response.daemon_version.clone();
    let running_agent_count = response
        .running_agents
        .as_ref()
        .map(|summary| summary.count);

    // Set ONLY inside the stamp branch, which the protocol check guards. A
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
    } else if daemon_build_version.as_deref() != Some(client_build)
        && !release_versions_are_compatible(client_build, daemon_build_version.as_deref())
    {
        // Reached only AFTER the protocol check above returned equal, so the
        // wire shape is already agreed. Two builds get here: ones whose release
        // versions declare a compatibility break between them, and ones where a
        // stamp could not be read at all — the fail-safe fallback. A stamp
        // difference WITHIN one release falls through to the silent arm below,
        // because by this project's own bump policy nothing incompatible sits
        // between two builds that share a compatibility key (issue #801).
        build_stamp_mismatch_only = true;
        let builds = format!(
            "build mismatch: desktop is {client_build}, deck is {}",
            daemon_build_version.as_deref().unwrap_or("unreported")
        );
        // PRD #741 M8: for a remote deck the stamp is an informational badge and
        // never a refusal, so the connection is made and the caveat travels with
        // it. Checked BEFORE the allowance switches because it is not one of
        // them — nothing is being bypassed here, and the message must not tell a
        // user they overrode something they were never offered.
        if stamps == StampPolicy::Informational {
            build_mismatch_was_bypassed = true;
            Some(format!(
                "{builds}. Connected: protocol {PROTOCOL_VERSION} matched on both sides, and a deck on another host is not this app's to replace. A stamp difference can still mean divergent behaviour behind an identical wire — see the release notes for both builds before trusting a field that looks wrong."
            ))
        } else {
            // Whichever switch is armed, the mismatch is kept in `error` (not
            // dropped) so the caveat stays on screen for the whole session rather
            // than being silently forgotten.
            build_mismatch_was_bypassed = allowance.allows();
            match allowance {
                BuildMismatchAllowance::Env => Some(format!(
                    "{builds}. Bypassed by {BUILD_MISMATCH_BYPASS_ENV}; protocol {PROTOCOL_VERSION} matched on both sides. Development only — a stamp difference can still mean divergent behaviour behind an identical wire."
                )),
                BuildMismatchAllowance::Session => Some(format!(
                    "{builds}. Connected anyway for this session; protocol {PROTOCOL_VERSION} matched on both sides. A stamp difference can still mean divergent behaviour behind an identical wire."
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
                    Some(format!("{builds}. {recovery}"))
                }
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
    stamps: StampPolicy,
) -> HandshakeInfo {
    classify_handshake(response, client_build, build_mismatch_allowance(), stamps)
}

fn connection_from_handshake(handshake: HandshakeInfo) -> DesktopConnection {
    let (deck_kind, local_only_reason, selection_fallback) = selection_fields();
    DesktopConnection {
        status: handshake.status,
        socket_path: socket_path_text(),
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
pub(crate) async fn hello(
    socket_path: &Path,
    stamps: StampPolicy,
) -> Result<(HandshakeInfo, AttachResponse), String> {
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
    let info = classify_handshake(&response, &client_build, build_mismatch_allowance(), stamps);
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
    let transport = tunnels.acquire(endpoint).await.map_err(safe_message)?;
    // PRD #741 M3: `hello()` still takes the raw address and still connects with
    // an `IpcStream`, deliberately. Under DECISION 1A a remote deck is reached
    // through a forwarded Unix socket, so the handshake needs no transport of
    // its own — M5 supplies the address, not a different way of opening it.
    let (info, response) = hello(transport.address(), StampPolicy::for_endpoint(endpoint)).await?;
    let connection = connection_from_handshake(info);
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
        Err(error) => return disconnected_snapshot(error),
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
            protocol_version: PROTOCOL_VERSION,
            source: "daemon",
        };
    }

    if let Some(view) = view {
        if view.needs_fetch(tokio::time::Instant::now()).is_none() {
            let records = view.records();
            return connected_snapshot(connection, records);
        }
        return match daemon.client.list_agents().await {
            Ok(records) => {
                view.install(records, tokio::time::Instant::now());
                connected_snapshot(connection, view.records())
            }
            Err(error) => {
                // The fetch failed, so the view's demand stands: it was never
                // cleared, and the next refresh will try again rather than
                // promoting whatever it was holding into an answer.
                links.invalidate(endpoint).await;
                disconnected_snapshot(error.to_string())
            }
        };
    }

    match daemon.client.list_agents().await {
        Ok(records) => connected_snapshot(connection, records),
        Err(error) => {
            // The held link just failed to carry a request. Whatever is at the
            // other end is not the daemon this handshake classified, so the
            // classification goes with the connection — the next call
            // handshakes again rather than reporting a verdict it can no longer
            // support.
            links.invalidate(endpoint).await;
            disconnected_snapshot(error.to_string())
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
) -> DesktopSnapshot {
    DesktopSnapshot {
        connection: DesktopConnection {
            running_agent_count: Some(records.len()),
            ..connection
        },
        agents: records.into_iter().map(map_agent).collect(),
        protocol_version: PROTOCOL_VERSION,
        source: "daemon",
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
        Err(error) => disconnected_snapshot(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dot_agent_deck::daemon_client::{LocalEndpoint, RemoteEndpoint};
    use dot_agent_deck::remote_tunnel::{Hostname, RemoteSocketPath};
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

    /// A remote deck's build stamp is an informational badge, never a refusal
    /// (PRD #741 M8).
    ///
    /// The same `Hello` that refuses a LOCAL deck below connects here, and the
    /// pair is the milestone: `Local` keeps today's verdict because the desktop
    /// bundles its own sidecar and **Replace daemon** is a remedy the user can
    /// take; `Remote` cannot be replaced from here at all — M2 made it
    /// impossible by type — so refusing would offer an action that does not
    /// exist.
    #[test]
    fn a_remote_decks_build_stamp_never_refuses_the_connection() {
        let _guard = AllowanceGuard::acquire();
        let response = hello_with_build(Some("0.38.0-gdeadbee"));

        let remote = classify_handshake(
            &response,
            "0.39.0-gcafe123",
            BuildMismatchAllowance::Refuse,
            StampPolicy::Informational,
        );
        assert_eq!(remote.status, ConnectionStatus::Connected);

        let local = classify_handshake(
            &response,
            "0.39.0-gcafe123",
            BuildMismatchAllowance::Refuse,
            StampPolicy::Enforced,
        );
        assert_eq!(
            local.status,
            ConnectionStatus::Incompatible,
            "a local deck keeps today's verdict exactly"
        );
    }

    /// The stamp is demoted, not hidden: both builds are still named, and the
    /// sentence is honest about what it cannot rule out (PRD #741 M8).
    #[test]
    fn a_remote_decks_stamp_difference_is_disclosed_and_offers_no_impossible_remedy() {
        let _guard = AllowanceGuard::acquire();
        let info = classify_handshake(
            &hello_with_build(Some("0.38.0-gdeadbee")),
            "0.39.0-gcafe123",
            BuildMismatchAllowance::Refuse,
            StampPolicy::Informational,
        );

        let error = info.error.expect("the caveat travels with the connection");
        assert!(error.contains("0.38.0-gdeadbee"), "{error}");
        assert!(error.contains("0.39.0-gcafe123"), "{error}");
        assert!(
            error.contains("divergent behaviour behind an identical wire"),
            "the limit a stamp cannot see is stated rather than implied: {error}"
        );
        assert!(
            !error.contains("Replace deck"),
            "a remedy that cannot be taken is worse than none: {error}"
        );
        assert!(
            !error.contains("Bypassed") && !error.contains("anyway"),
            "nothing was overridden, so the user must not be told they overrode it: {error}"
        );
        assert!(
            info.build_stamp_mismatch_only,
            "the badge is what the screens key on"
        );
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
            StampPolicy::Informational,
        );

        assert_eq!(info.status, ConnectionStatus::Incompatible);
        assert!(
            !info.build_stamp_mismatch_only,
            "a wire mismatch must never advertise an override"
        );
        assert!(info.error.unwrap().contains("protocol mismatch"));
    }

    /// The policy comes from the endpoint's KIND and from nothing else.
    ///
    /// **The `Remote` arm is the assertion that matters**, and it was the one
    /// missing: `Enforced` for a local deck is the behaviour that existed before
    /// this type did, so a refactor that regressed to it would leave the local
    /// cases green while silently restoring the refusal M8 removed.
    ///
    /// Other tests pin what `Informational` *does* once something has chosen it
    /// — the classifier's own arm, and the probe's verdict. This is the only one
    /// that pins **which deck kind gets it**, which is why it is the one the
    /// mutation reaches: `for_endpoint` returning `Enforced` unconditionally
    /// reddens here and nowhere else in the crate's 204 tests, measured both
    /// before and after this assertion was added.
    #[test]
    fn the_stamp_policy_follows_the_deck_kind() {
        assert_eq!(
            StampPolicy::for_endpoint(&Endpoint::Local(LocalEndpoint::at("/tmp/deck.sock"))),
            StampPolicy::Enforced
        );
        assert_eq!(
            StampPolicy::for_endpoint(&Endpoint::local()),
            StampPolicy::Enforced
        );
        assert_eq!(
            StampPolicy::for_endpoint(&Endpoint::Remote(remote_deck())),
            StampPolicy::Informational,
            "a remote deck's stamp is informational: this is the whole of M8, and the branch that \
             makes a remote deck connect through a stamp difference is pinned by this assertion \
             alone"
        );
    }

    /// A remote deck to decide a policy about. The host and socket are the ones
    /// the endpoint tests already use; nothing here connects to either.
    fn remote_deck() -> RemoteEndpoint {
        RemoteEndpoint::new(
            Hostname::parse("build-box").expect("a plain host name is valid"),
            RemoteSocketPath::parse("/run/user/1000/dot-agent-deck-attach.sock")
                .expect("an absolute remote socket path is valid"),
        )
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
        let full = AttachResponse::hello(PROTOCOL_VERSION).with_capabilities();
        assert_eq!(
            classify_handshake(
                &full,
                full.build_version.as_deref().unwrap(),
                BuildMismatchAllowance::Refuse,
                StampPolicy::Enforced,
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
            StampPolicy::Enforced,
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
            StampPolicy::Enforced,
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
            StampPolicy::Enforced,
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
            StampPolicy::Enforced,
        );
        assert_eq!(info.status, ConnectionStatus::Incompatible);
        assert!(info.error.unwrap().contains("protocol mismatch"));
    }

    #[test]
    fn zero_agent_build_mismatch_points_to_safe_replacement() {
        let response = AttachResponse::hello(PROTOCOL_VERSION)
            .with_running_agents(RunningAgentsSummary::default());
        let info = classify_handshake(
            &response,
            "desktop-other-build",
            BuildMismatchAllowance::Refuse,
            StampPolicy::Enforced,
        );
        assert_eq!(info.status, ConnectionStatus::Incompatible);
        let error = info.error.unwrap();
        assert!(error.contains("build mismatch"));
        assert!(error.contains("use Replace deck"));
    }

    #[test]
    fn live_agent_build_mismatch_blocks_replacement() {
        let response =
            AttachResponse::hello(PROTOCOL_VERSION).with_running_agents(RunningAgentsSummary {
                count: 2,
                names: vec!["coder".into(), "tester".into()],
            });
        let info = classify_handshake(
            &response,
            "desktop-other-build",
            BuildMismatchAllowance::Refuse,
            StampPolicy::Enforced,
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

    /// A stamp difference is downgraded to a warning, not silence: the deck
    /// connects, but the connection message still names both builds so the
    /// caveat survives for the whole session.
    #[test]
    fn bypassed_build_mismatch_connects_and_keeps_the_warning_visible() {
        let response = AttachResponse::hello(PROTOCOL_VERSION)
            .with_running_agents(RunningAgentsSummary::default());
        let info = classify_handshake(
            &response,
            "desktop-other-build",
            BuildMismatchAllowance::Env,
            StampPolicy::Enforced,
        );
        assert_eq!(info.status, ConnectionStatus::Connected);
        let error = info.error.expect("bypass must not swallow the mismatch");
        assert!(error.contains("build mismatch"), "{error}");
        assert!(error.contains(BUILD_MISMATCH_BYPASS_ENV), "{error}");
    }

    /// The in-app override says so in its own words. Naming the env var here
    /// would tell a user who pressed a button to go looking for a shell
    /// variable they never set — and a `.app` launched from Finder could not
    /// have received one anyway (issue #801).
    #[test]
    fn session_override_connects_and_names_itself_rather_than_the_env_var() {
        let response =
            AttachResponse::hello(PROTOCOL_VERSION).with_running_agents(RunningAgentsSummary {
                count: 9,
                names: vec!["coder".into()],
            });
        let info = classify_handshake(
            &response,
            "desktop-other-build",
            BuildMismatchAllowance::Session,
            StampPolicy::Enforced,
        );
        assert_eq!(info.status, ConnectionStatus::Connected);
        let error = info
            .error
            .expect("the override must not swallow the mismatch");
        assert!(error.contains("build mismatch"), "{error}");
        assert!(
            error.contains("Connected anyway for this session"),
            "{error}"
        );
        assert!(
            error.contains("divergent behaviour behind an identical wire"),
            "{error}"
        );
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
            let info = classify_handshake(
                &response,
                "desktop-other-build",
                allowance,
                StampPolicy::Enforced,
            );
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
            let info = classify_handshake(
                &response,
                "desktop-other-build",
                allowance,
                StampPolicy::Enforced,
            );
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
            StampPolicy::Enforced,
        );
        assert_eq!(info.status, ConnectionStatus::Incompatible);
        assert!(!info.build_stamp_mismatch_only);
    }

    /// What the UI switches on: the protocol agreed and only the stamp differs,
    /// so an override is legitimate. True whether or not one is already armed —
    /// the flag describes the mismatch, not the response to it.
    #[test]
    fn build_mismatch_advertises_a_stamp_only_override() {
        let response = AttachResponse::hello(PROTOCOL_VERSION)
            .with_running_agents(RunningAgentsSummary::default());
        for allowance in [
            BuildMismatchAllowance::Refuse,
            BuildMismatchAllowance::Env,
            BuildMismatchAllowance::Session,
        ] {
            let info = classify_handshake(
                &response,
                "desktop-other-build",
                allowance,
                StampPolicy::Enforced,
            );
            assert!(info.build_stamp_mismatch_only, "{allowance:?}");
        }
    }

    /// A daemon that reports no build stamp at all is still a mismatch, and the
    /// bypass covers it the same way — otherwise the escape hatch would have a
    /// hole exactly where the least is known about the peer.
    #[test]
    fn bypass_covers_an_unreported_daemon_stamp() {
        let mut response = AttachResponse::hello(PROTOCOL_VERSION);
        response.build_version = None;
        let info = classify_handshake(
            &response,
            "desktop-build",
            BuildMismatchAllowance::Env,
            StampPolicy::Enforced,
        );
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
        let info = classify_handshake(
            &response,
            "desktop-other-build",
            build_mismatch_allowance(),
            StampPolicy::Enforced,
        );
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
        let response = AttachResponse::hello(PROTOCOL_VERSION)
            .with_running_agents(RunningAgentsSummary::default());

        let refused = classify_handshake(
            &response,
            "desktop-other-build",
            build_mismatch_allowance(),
            StampPolicy::Enforced,
        );
        assert_eq!(refused.status, ConnectionStatus::Incompatible);
        assert!(refused.build_stamp_mismatch_only);

        allow_build_mismatch_this_session();

        let retried = classify_handshake(
            &response,
            "desktop-other-build",
            build_mismatch_allowance(),
            StampPolicy::Enforced,
        );
        assert_eq!(retried.status, ConnectionStatus::Connected);
        assert!(
            retried
                .error
                .expect("the caveat must survive the override")
                .contains("build mismatch")
        );
    }

    /// The case issue #801 was filed about. A released daemon and a branch
    /// desktop that describe as the same release: the protocol agreed, and by
    /// this project's bump policy a compatibility break would have moved the
    /// minor, so nothing incompatible sits between them. Connect, silently —
    /// under EVERY allowance, because this must be the ordinary verdict and not
    /// something an override rescues.
    #[test]
    fn a_stamp_difference_within_one_release_connects_silently() {
        for (desktop, daemon) in [
            ("0.39.0-ga0165f8", "0.39.0-g1ea0fe7"),
            ("0.39.0-49-ga0165f8", "0.39.0-g1ea0fe7"),
            ("0.39.0-49-ga0165f8-dirty", "0.39.0-g1ea0fe7"),
            // The patch digit tracks features and bugfixes while the major is
            // `0`, so it is deliberately not part of the compatibility key.
            ("0.39.2-ga0165f8", "0.39.0-g1ea0fe7"),
        ] {
            for allowance in [
                BuildMismatchAllowance::Refuse,
                BuildMismatchAllowance::Env,
                BuildMismatchAllowance::Session,
            ] {
                let case = format!("{desktop} vs {daemon} under {allowance:?}");
                let info = classify_handshake(
                    &hello_with_build(Some(daemon)),
                    desktop,
                    allowance,
                    StampPolicy::Enforced,
                );
                assert_eq!(info.status, ConnectionStatus::Connected, "{case}");
                assert!(!info.build_stamp_mismatch_only, "{case}");
                assert!(info.error.is_none(), "{case}: {:?}", info.error);
            }
        }
    }

    /// A minor bump while the major is `0` IS the declared compatibility break,
    /// so this is the pair that must keep prompting — and must keep offering
    /// the override, because the wire itself still agreed.
    #[test]
    fn a_differing_minor_while_zerover_still_prompts_with_an_override() {
        let info = classify_handshake(
            &hello_with_build(Some("0.40.0-g1ea0fe7")),
            "0.39.0-ga0165f8",
            BuildMismatchAllowance::Refuse,
            StampPolicy::Enforced,
        );
        assert_eq!(info.status, ConnectionStatus::Incompatible);
        assert!(info.build_stamp_mismatch_only);
        let error = info.error.unwrap();
        assert!(error.contains("build mismatch"), "{error}");
        assert!(error.contains("Connect anyway"), "{error}");
    }

    /// The failure direction that matters. Silent connection is the permissive
    /// answer, so it is reached only on positive evidence that BOTH stamps
    /// parsed and their keys matched; anything unreadable on either side falls
    /// back to the prompt rather than through it.
    #[test]
    fn an_unreadable_stamp_on_either_side_falls_back_to_the_prompt() {
        for (desktop, daemon) in [
            ("0.39.0-ga0165f8", Some("nightly")),
            ("nightly", Some("0.39.0-g1ea0fe7")),
            ("0.39-ga0165f8", Some("0.39.0-g1ea0fe7")),
            ("0.39.0.1-ga0165f8", Some("0.39.0-g1ea0fe7")),
            ("", Some("0.39.0-g1ea0fe7")),
            // The daemon reported no stamp at all, which is the least that can
            // be known about a peer and so the least it may be trusted with.
            ("0.39.0-ga0165f8", None),
        ] {
            let case = format!("{desktop} vs {daemon:?}");
            let info = classify_handshake(
                &hello_with_build(daemon),
                desktop,
                BuildMismatchAllowance::Refuse,
                StampPolicy::Enforced,
            );
            assert_eq!(info.status, ConnectionStatus::Incompatible, "{case}");
            assert!(info.build_stamp_mismatch_only, "{case}");
        }
    }

    /// Every stamp form the build script can emit reduces to its release core,
    /// and everything else reduces to nothing.
    #[test]
    fn stamp_forms_parse_down_to_their_release_core() {
        assert_eq!(release_core("0.39.0-g1ea0fe7"), Some((0, 39, 0)));
        assert_eq!(release_core("0.39.0-49-ga0165f8"), Some((0, 39, 0)));
        assert_eq!(release_core("0.39.0-g1ea0fe7-dirty"), Some((0, 39, 0)));
        assert_eq!(release_core("0.25.0-alpha.0-g1ea0fe7"), Some((0, 25, 0)));
        assert_eq!(release_core("1.2.3+meta-g1ea0fe7"), Some((1, 2, 3)));
        assert_eq!(release_core("0.1.0-unknown"), Some((0, 1, 0)));
        assert_eq!(release_core("v1.2.3"), Some((1, 2, 3)));
        for unreadable in [
            "", "nightly", "1.2", "1.2.3.4", "1.2.x", "-1.2.3", "1.-2.3", "+1.2.3",
        ] {
            assert_eq!(release_core(unreadable), None, "{unreadable}");
        }
    }

    /// Both arms of the bump policy, at the key rather than at the handshake.
    #[test]
    fn the_compatibility_key_tracks_the_minor_while_zerover_and_the_major_after() {
        // While `0.x` the minor is the compatibility digit and the patch is not.
        assert_eq!(
            compatibility_key("0.39.0-ga0"),
            compatibility_key("0.39.7-g1e")
        );
        assert_ne!(
            compatibility_key("0.39.0-ga0"),
            compatibility_key("0.40.0-g1e")
        );
        // From `1.0` the rules are standard SemVer and only the major moves.
        assert_eq!(
            compatibility_key("1.4.0-ga0"),
            compatibility_key("1.9.2-g1e")
        );
        assert_ne!(
            compatibility_key("1.9.2-ga0"),
            compatibility_key("2.0.0-g1e")
        );
        // Dropping the minor on the `1.x` arm must not let the two arms collide.
        assert_ne!(
            compatibility_key("0.1.0-ga0"),
            compatibility_key("1.0.0-g1e")
        );
    }

    /// The `1.x` arm end to end. This repo has not shipped `1.0` yet; the arm
    /// exists so that the day it does, a differing minor stops being a refusal
    /// without anyone having to remember to come back here.
    #[test]
    fn from_one_zero_onward_only_a_differing_major_refuses() {
        let compatible = classify_handshake(
            &hello_with_build(Some("1.9.2-g1ea0fe7")),
            "1.4.0-ga0165f8",
            BuildMismatchAllowance::Refuse,
            StampPolicy::Enforced,
        );
        assert_eq!(compatible.status, ConnectionStatus::Connected);
        assert!(!compatible.build_stamp_mismatch_only);
        assert!(compatible.error.is_none(), "{:?}", compatible.error);

        let broken = classify_handshake(
            &hello_with_build(Some("2.0.0-g1ea0fe7")),
            "1.9.2-ga0165f8",
            BuildMismatchAllowance::Refuse,
            StampPolicy::Enforced,
        );
        assert_eq!(broken.status, ConnectionStatus::Incompatible);
        assert!(broken.build_stamp_mismatch_only);
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

        let (info, response) = hello(&socket, StampPolicy::Enforced)
            .await
            .expect("the handshake must complete");

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

        let (info, _response) = hello(&socket, StampPolicy::Enforced)
            .await
            .expect("the exchange still completes");

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
        let error = hello(&socket, StampPolicy::Enforced)
            .await
            .expect_err("nothing is listening");
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
            let info = classify_handshake(
                &response,
                "0.39.0-ga0165f8",
                allowance,
                StampPolicy::Enforced,
            );
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
            connection: connection_from_handshake(HandshakeInfo {
                status: ConnectionStatus::Connected,
                error: None,
                server_protocol_version: Some(PROTOCOL_VERSION),
                daemon_build_version: None,
                daemon_version: None,
                running_agent_count: Some(0),
                build_stamp_mismatch_only: false,
                project_actions_reason: None,
            }),
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

        let mut busy = AttachResponse::hello(PROTOCOL_VERSION);
        busy.build_version = Some("0.1.0-gdeadbee".into());
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
}
