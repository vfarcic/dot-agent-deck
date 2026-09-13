//! PRD #741 M7 — who owns the `ssh -N -L` child, and when it dies.
//!
//! M5 built the tunnel and deliberately did not rewire the desktop, and its
//! reason is the constraint this module is the answer to: a [`TrustedDaemon`]
//! is re-established every [`HANDSHAKE_REVALIDATE_INTERVAL`] (5 s), so a tunnel
//! owned *by* one would re-authenticate ssh every five seconds. **Tunnel
//! lifetime is not handshake lifetime**, so the tunnel gets its own owner with
//! its own lifecycle, keyed by endpoint, held in `DesktopState` beside
//! [`DaemonLinks`] rather than inside it.
//!
//! [`TrustedDaemon`]: crate::daemon_bridge::TrustedDaemon
//! [`HANDSHAKE_REVALIDATE_INTERVAL`]: crate::daemon_bridge::HANDSHAKE_REVALIDATE_INTERVAL
//! [`DaemonLinks`]: crate::daemon_bridge::DaemonLinks
//!
//! # The ownership rules, stated so M9 and #742 inherit them
//!
//! 1. **Every live tunnel is in this map, and nothing else opens one.**
//!    `EndpointTunnels::acquire` is the only call site of
//!    `EndpointConnection::open` on the desktop's path. A caller that wants the
//!    address a client connects to asks for a lease; it never builds a tunnel
//!    of its own and never keeps a bare `PathBuf` past the lease.
//!
//! 2. **A lease is an `Arc`, so the tunnel outlives its removal from the map
//!    but not its last holder.** `release` and `retain` drop the map's handle;
//!    the child dies when the last lease drops, which is what stops a selection
//!    change from tearing the transport out from under a Hello or an attach
//!    that is still in flight. [`TrustedDaemon`] holds one for the life of the
//!    link precisely so that a held handshake can never describe a deck whose
//!    transport has already gone.
//!
//! 3. **Teardown has four triggers, and each one is a place in the app:**
//!    - *the selection changed* — `desktop_set_settings` calls [`EndpointTunnels::retain`]
//!      with the **observed** endpoints' keys, which closes the tunnel to a
//!      deck the user just deselected, re-addressed or removed. This is the
//!      leak the PRD names: without it every selection change leaves an
//!      authenticated `ssh` child behind. PRD #742 M2 widened that argument
//!      from one key to a set — under `Selection::All` every configured deck
//!      with somewhere to connect to is observed, and only the decks that left
//!      the set are dropped — and changed nothing else here: the map was
//!      already keyed, `retain` already took a set, and what was single-deck
//!      was the caller building a one-element one.
//!    - *the child died* — [`EndpointTunnels::acquire`] asks
//!      `EndpointConnection::health`, never `Path::exists`, and re-opens on
//!      `Exited`. The forwarded socket is created by the local `ssh` client and
//!      outlives a dead far end, so its presence answers a question nobody
//!      asked.
//!    - *a probe finished* — `Test connection` acquires a lease for a deck that
//!      may not be the selection and drops it when the probe returns, so a
//!      test never leaves a tunnel behind.
//!    - *the app is exiting* — `RunEvent::ExitRequested`/`Exit` calls
//!      [`EndpointTunnels::close_all`].
//!
//! 4. **The paths where none of that runs are M5's problem and stay solved
//!    there.** A SIGKILL or force-quit skips `Drop`, so the `ssh` child
//!    survives holding its socket — which is why every socket name is unique
//!    per open (no later run can adopt an orphan and read it as a healthy deck)
//!    and why `RemoteTunnel::open` sweeps `reap_orphaned_tunnels` on the way
//!    in. Nothing here weakens either.
//!
//! 5. **A local endpoint is in the map too**, and holds nothing: `EndpointConnection::Local`
//!    owns no child and its `connect_address` is the configured socket path.
//!    Keying both kinds the same way is what keeps every caller written against
//!    one seam, and it is what makes the transport's own presence — not a `stat`
//!    on the path — the thing that decides what a missing inode means.
//!
//! # What this deliberately does not do
//!
//! It does not multiplex, reference-count leases into a pool with a grace
//! period, or supervise a tunnel with reconnect/backoff. A dead child is
//! re-opened on the next `acquire` and that is the whole policy. #742's fleet
//! view is what turns "one selected deck plus whatever is being probed" into a
//! set worth managing, and it inherits this map rather than replacing it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use dot_agent_deck::daemon_client::{Endpoint, EndpointIdentity};
use tokio::sync::Mutex as AsyncMutex;

/// The address every [`EndpointTunnels::insert_stand_in`] lease reports. One
/// value for every key, because nothing that seam is used to test ever reads
/// it — a lease that is being *connected through* has to come from `acquire`.
#[cfg(test)]
const STAND_IN_ADDRESS: &str = "/tmp/dot-agent-deck-stand-in.sock";

#[cfg(unix)]
use dot_agent_deck::remote_tunnel::{EndpointConnection, SshProgram, TunnelHealth};

/// One established transport, and the address a client opens on it.
///
/// The `EndpointConnection` is behind a plain [`Mutex`] rather than an async
/// one because the only thing anybody does with it is ask
/// `EndpointConnection::health` — a `try_wait` on a child, with no `await`
/// anywhere inside it. The address is copied out at construction so the hot
/// path never takes that lock at all.
///
/// There is deliberately no `presence()` accessor. The presence a `stat` of the
/// address may be read with travels *inside* the [`dot_agent_deck::daemon_client::DaemonClient`]
/// [`Self::client`] builds, and adding a second way to get at it is how a
/// caller ends up pairing a remote address with `DaemonClient::new`.
#[cfg(unix)]
#[derive(Debug)]
pub(crate) struct TunnelLease {
    address: PathBuf,
    connection: Mutex<EndpointConnection>,
}

#[cfg(unix)]
impl TunnelLease {
    /// The address a client connects to.
    ///
    /// **A bare `&Path`, never a `LocalEndpoint`** — the property M2 built and
    /// every milestone since is the one most likely to undo. Handing this to
    /// `LocalEndpoint::at` would make `run_daemon_stop` compile against it, and
    /// its `SO_PEERCRED` lookup would name the local `ssh` client.
    pub(crate) fn address(&self) -> &std::path::Path {
        &self.address
    }

    /// Build a `DaemonClient` that carries this lease's presence.
    ///
    /// The one supported way to get a client for a leased endpoint. Reaching
    /// for `DaemonClient::new` instead would stamp the tunnel's own socket
    /// `LocalInode` and put `exists()`-as-health back — the path M3's
    /// `Elsewhere` closed.
    pub(crate) fn client(&self) -> Result<dot_agent_deck::daemon_client::DaemonClient, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "the endpoint transport lock was poisoned".to_string())?;
        Ok(dot_agent_deck::daemon_client::DaemonClient::for_connection(
            &connection,
        ))
    }

    /// Whether the substrate is still there. Never `Path::exists`.
    fn alive(&self) -> bool {
        match self.connection.lock() {
            Ok(mut connection) => matches!(connection.health(), TunnelHealth::Alive),
            // A poisoned lock means a panic happened while somebody held the
            // transport. "I no longer know" is not "healthy": say dead and let
            // the caller re-open, which is the fail-safe direction.
            Err(_) => false,
        }
    }
}

/// Why [`EndpointTunnels::acquire`] could not hand back a lease.
///
/// Two arms because there are two kinds of caller need. A classifier wants the
/// *typed* tunnel error, so it can read the verdict `classify_exit` already
/// derived from ssh's raw stderr rather than re-deriving it from scrubbed
/// text. A caller that only has a banner to fill wants a sentence, and gets one
/// from [`Display`].
#[cfg(unix)]
#[derive(Debug)]
pub(crate) enum AcquireError {
    /// The tunnel itself refused. Boxed because `TunnelError` is much the
    /// larger arm and `clippy::result_large_err` would otherwise object.
    Tunnel(Box<dot_agent_deck::remote_tunnel::TunnelError>),
    /// Everything before the tunnel could be attempted: no `ssh` program on
    /// this machine, or a blocking task that did not come back. Neither says
    /// anything about the far host, so neither is worth classifying.
    Local(String),
}

#[cfg(unix)]
impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tunnel(error) => error.fmt(f),
            Self::Local(message) => f.write_str(message),
        }
    }
}

/// The live transports, keyed by [`Endpoint::describe`] — the same key
/// [`DaemonLinks`] uses, so the two maps can be reasoned about together.
///
/// [`DaemonLinks`]: crate::daemon_bridge::DaemonLinks
#[cfg(unix)]
#[derive(Default)]
pub(crate) struct EndpointTunnels {
    tunnels: AsyncMutex<HashMap<EndpointIdentity, Arc<TunnelLease>>>,
}

#[cfg(unix)]
impl EndpointTunnels {
    /// A lease on the transport for `endpoint`, establishing one if there is
    /// none or the held one's child has exited.
    ///
    /// One async mutex over the whole map, held across establishment, for the
    /// same reason `DaemonLinks` takes one: concurrent first-uses collapse into
    /// ONE ssh authentication instead of N, and a deck that is not answering
    /// queues the other callers behind one connect attempt rather than giving
    /// each its own.
    ///
    /// The error is **typed**, and that is a security property rather than
    /// tidiness (PRD #741 final audit **F3**). This used to flatten
    /// `TunnelError` with `to_string()`, and `SshError`'s `detail` by then
    /// already holds `scrub_remote_text(stderr)` — so a caller re-classifying
    /// the flattened string was substring-matching text the scrubber had
    /// *rewritten*. Stripping can only create matches the raw bytes lacked, so
    /// a peer writing `Host key verification fai\x01led.` could promote an
    /// unrelated failure into a host-key verdict carrying a copy-paste `ssh`
    /// remedy. `TunnelError` already carries the verdict `classify_exit` derived
    /// from the **raw** stderr; handing it over means nobody has to re-derive it
    /// from a lossy copy.
    pub(crate) async fn acquire(
        &self,
        endpoint: &Endpoint,
    ) -> Result<Arc<TunnelLease>, AcquireError> {
        let key = endpoint.identity();
        let mut tunnels = self.tunnels.lock().await;
        if let Some(held) = tunnels.get(&key) {
            if held.alive() {
                return Ok(Arc::clone(held));
            }
            // Dropped from the map, not closed here: another caller may still
            // be holding a lease, and its `Drop` is what tears the child down.
            tunnels.remove(&key);
        }
        let ssh = SshProgram::resolve().map_err(|error| AcquireError::Local(error.to_string()))?;
        let endpoint = endpoint.clone();
        // `EndpointConnection::open` is blocking — it spawns `ssh` and polls for
        // the forwarded socket for up to `FORWARD_READY_TIMEOUT`. Off the async
        // runtime's worker, or every other desktop command stalls behind it.
        let connection =
            tokio::task::spawn_blocking(move || EndpointConnection::open(&endpoint, &ssh))
                .await
                .map_err(|error| {
                    AcquireError::Local(format!("the endpoint transport task failed: {error}"))
                })?
                .map_err(|error| AcquireError::Tunnel(Box::new(error)))?;
        let lease = Arc::new(TunnelLease {
            address: connection.connect_address().to_path_buf(),
            connection: Mutex::new(connection),
        });
        tunnels.insert(key, Arc::clone(&lease));
        Ok(lease)
    }

    /// Drop the map's handle on `endpoint`'s transport.
    pub(crate) async fn release(&self, endpoint: &Endpoint) {
        self.tunnels.lock().await.remove(&endpoint.identity());
    }

    /// Drop the map's handle on every transport except the ones `live` names.
    ///
    /// The selection-change teardown. Takes the keys rather than the endpoints
    /// because the caller already has the new selection in hand and the key is
    /// what the map is indexed by.
    ///
    /// **`live` is the observed set, which is one key only when the selection
    /// names one deck** (PRD #742 M2). It is a set difference and nothing more,
    /// so it does not care *which* of the three set-level events happened: a
    /// deck that was removed, and a deck whose address was edited — a different
    /// [`EndpointIdentity`], so the old one is simply no longer named — are the
    /// same instruction here. A deck that is newly named holds nothing yet and
    /// is acquired lazily on first use; this never establishes anything, which
    /// is what keeps N decks off `acquire`'s whole-map mutex until something
    /// actually wants one.
    ///
    /// It removes only the map's handle, so a deck dropped here while another
    /// deck's watcher or terminal holds its own lease keeps running until that
    /// holder lets go (rule 2) — which is what makes a fleet safe to edit while
    /// it is live.
    pub(crate) async fn retain(&self, live: &HashSet<EndpointIdentity>) {
        self.tunnels
            .lock()
            .await
            .retain(|key, _| live.contains(key));
    }

    /// Drop every handle. The app-exit teardown.
    pub(crate) async fn close_all(&self) {
        self.tunnels.lock().await.clear();
    }

    /// Seed the map with a stand-in transport under `endpoint`'s key. Test-only.
    ///
    /// A **remote** deck's transport cannot be established in a unit test —
    /// `acquire` would spawn `ssh` and authenticate against a host that is not
    /// there — and a fleet whose every non-local member is remote is otherwise
    /// unreachable from a test of the caller. What PRD #742 M2 needs to assert
    /// at that level is which keys survive a `retain`, and neither the map nor
    /// `retain` ever reads the connection, so a local stand-in under the remote
    /// deck's own key exercises exactly the part under test and fabricates
    /// nothing that part depends on. Anything that would *use* the transport
    /// belongs in an integration test with a real deck at the other end.
    #[cfg(test)]
    pub(crate) async fn insert_stand_in(&self, endpoint: &Endpoint) -> Arc<TunnelLease> {
        let address = PathBuf::from(STAND_IN_ADDRESS);
        let lease = Arc::new(TunnelLease {
            address: address.clone(),
            connection: Mutex::new(EndpointConnection::Local(
                dot_agent_deck::daemon_client::LocalEndpoint::at(address),
            )),
        });
        self.tunnels
            .lock()
            .await
            .insert(endpoint.identity(), Arc::clone(&lease));
        lease
    }

    /// How many transports are held. Test-only: no production path has a reason
    /// to ask, and the leak this milestone is about is exactly a count that
    /// does not come back down.
    #[cfg(test)]
    pub(crate) async fn held(&self) -> usize {
        self.tunnels.lock().await.len()
    }
}

// The tunnel itself is Unix-only (`remote_tunnel`'s `mod tunnel` is
// `#[cfg(unix)]`), so on Windows this type exists only far enough to keep
// `DesktopState` and every signature that threads it compiling. Windows desktop
// needs #754 and is out of PRD #741's scope; the seam is here so that PRD does
// not have to reshape every call site as well.
#[cfg(not(unix))]
#[derive(Debug)]
pub(crate) struct TunnelLease {
    address: PathBuf,
}

#[cfg(not(unix))]
impl TunnelLease {
    pub(crate) fn address(&self) -> &std::path::Path {
        &self.address
    }

    pub(crate) fn client(&self) -> Result<dot_agent_deck::daemon_client::DaemonClient, String> {
        Ok(dot_agent_deck::daemon_client::DaemonClient::new(
            self.address.clone(),
        ))
    }
}

#[cfg(not(unix))]
#[derive(Default)]
pub(crate) struct EndpointTunnels {
    tunnels: AsyncMutex<HashMap<EndpointIdentity, Arc<TunnelLease>>>,
}

#[cfg(not(unix))]
impl EndpointTunnels {
    pub(crate) async fn acquire(&self, endpoint: &Endpoint) -> Result<Arc<TunnelLease>, String> {
        let address = endpoint
            .connect_address()
            .map_err(|error| error.to_string())?
            .to_path_buf();
        let key = endpoint.identity();
        let mut tunnels = self.tunnels.lock().await;
        if let Some(held) = tunnels.get(&key) {
            return Ok(Arc::clone(held));
        }
        let lease = Arc::new(TunnelLease { address });
        tunnels.insert(key, Arc::clone(&lease));
        Ok(lease)
    }

    pub(crate) async fn release(&self, endpoint: &Endpoint) {
        self.tunnels.lock().await.remove(&endpoint.identity());
    }

    pub(crate) async fn retain(&self, live: &HashSet<EndpointIdentity>) {
        self.tunnels
            .lock()
            .await
            .retain(|key, _| live.contains(key));
    }

    pub(crate) async fn close_all(&self) {
        self.tunnels.lock().await.clear();
    }

    /// Seed the map with a stand-in transport under `endpoint`'s key. Test-only.
    ///
    /// A **remote** deck's transport cannot be established in a unit test —
    /// `acquire` would spawn `ssh` and authenticate against a host that is not
    /// there — and a fleet whose every non-local member is remote is otherwise
    /// unreachable from a test of the caller. What PRD #742 M2 needs to assert
    /// at that level is which keys survive a `retain`, and neither the map nor
    /// `retain` ever reads the connection, so a local stand-in under the remote
    /// deck's own key exercises exactly the part under test and fabricates
    /// nothing that part depends on. Anything that would *use* the transport
    /// belongs in an integration test with a real deck at the other end.
    #[cfg(test)]
    pub(crate) async fn insert_stand_in(&self, endpoint: &Endpoint) -> Arc<TunnelLease> {
        let lease = Arc::new(TunnelLease {
            address: PathBuf::from(STAND_IN_ADDRESS),
        });
        self.tunnels
            .lock()
            .await
            .insert(endpoint.identity(), Arc::clone(&lease));
        lease
    }

    #[cfg(test)]
    pub(crate) async fn held(&self) -> usize {
        self.tunnels.lock().await.len()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use dot_agent_deck::daemon_client::LocalEndpoint;

    /// A local deck is leased like any other, holds no child, and answers with
    /// the configured socket path.
    ///
    /// The point is not that a local deck works — it is that ONE seam covers
    /// both kinds, which is what lets `establish()` stop branching on the
    /// endpoint kind to find an address.
    #[tokio::test]
    async fn a_local_deck_leases_the_configured_address_and_holds_no_child() {
        let tunnels = EndpointTunnels::default();
        let endpoint = Endpoint::Local(LocalEndpoint::at("/tmp/dot-agent-deck-lease-test.sock"));

        let lease = tunnels
            .acquire(&endpoint)
            .await
            .expect("lease a local deck");

        assert_eq!(
            lease.address(),
            std::path::Path::new("/tmp/dot-agent-deck-lease-test.sock")
        );
        assert!(
            lease
                .client()
                .expect("a lease always yields a client")
                .ensure_socket_exists()
                .is_err(),
            "a local lease keeps the LOCAL presence, so a missing inode still means the daemon is \
             gone; a remote lease's `Elsewhere` makes the same call step aside instead"
        );
        assert_eq!(tunnels.held().await, 1);
    }

    /// The same endpoint leased twice is the SAME lease, not an equal one — so
    /// a second caller does not authenticate a second ssh connection.
    #[tokio::test]
    async fn leasing_the_same_deck_twice_hands_back_one_transport() {
        let tunnels = EndpointTunnels::default();
        let endpoint = Endpoint::Local(LocalEndpoint::at("/tmp/dot-agent-deck-lease-reuse.sock"));

        let first = tunnels.acquire(&endpoint).await.expect("first lease");
        let second = tunnels.acquire(&endpoint).await.expect("second lease");

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(tunnels.held().await, 1);
    }

    /// `retain` is the selection-change teardown, and the leak it closes is a
    /// count that never comes back down.
    #[tokio::test]
    async fn retain_drops_every_deck_the_selection_no_longer_names() {
        let tunnels = EndpointTunnels::default();
        let kept = Endpoint::Local(LocalEndpoint::at("/tmp/dot-agent-deck-lease-kept.sock"));
        let dropped = Endpoint::Local(LocalEndpoint::at("/tmp/dot-agent-deck-lease-dropped.sock"));
        tunnels.acquire(&kept).await.expect("lease the kept deck");
        tunnels
            .acquire(&dropped)
            .await
            .expect("lease the deck about to be deselected");
        assert_eq!(tunnels.held().await, 2);

        let live: HashSet<EndpointIdentity> = [kept.identity()].into_iter().collect();
        tunnels.retain(&live).await;

        assert_eq!(tunnels.held().await, 1);
        let again = tunnels
            .acquire(&kept)
            .await
            .expect("the kept deck survives");
        assert_eq!(
            again.address(),
            std::path::Path::new("/tmp/dot-agent-deck-lease-kept.sock")
        );
    }

    /// The fleet's live set keeps EVERY deck it names, not the one deck a
    /// single selection resolves to (PRD #742 M2).
    ///
    /// The sibling of the test above, and the pair is the whole of M2's
    /// lifetime change: `retain` was never single-deck — the map has been keyed
    /// and the argument has been a set since PRD #741 M7 — what was single-deck
    /// is the caller, which built a one-element set from `resolve()`. So what
    /// this pins is the semantics the new caller depends on, on the same three
    /// decks the contrast at the end then reduces to one.
    ///
    /// Survival is asserted by [`Arc::ptr_eq`] rather than by the count,
    /// because a count of three is also what "tear the map down and rebuild it
    /// from the new set" produces — with three brand-new `ssh` children and
    /// every lease a live holder is using orphaned behind them.
    #[tokio::test]
    async fn retain_keeps_every_deck_the_fleet_observes() {
        let tunnels = EndpointTunnels::default();
        let fleet: Vec<Endpoint> = ["local", "build-box", "laptop"]
            .into_iter()
            .map(|name| {
                Endpoint::Local(LocalEndpoint::at(format!(
                    "/tmp/dot-agent-deck-lease-fleet-{name}.sock"
                )))
            })
            .collect();
        let mut leased = Vec::new();
        for deck in &fleet {
            leased.push(tunnels.acquire(deck).await.expect("lease a fleet deck"));
        }
        assert_eq!(tunnels.held().await, 3);

        let observed: HashSet<EndpointIdentity> = fleet.iter().map(Endpoint::identity).collect();
        tunnels.retain(&observed).await;

        assert_eq!(tunnels.held().await, 3, "the fleet observes all three");
        for (deck, before) in fleet.iter().zip(&leased) {
            let after = tunnels.acquire(deck).await.expect("still leased");
            assert!(
                Arc::ptr_eq(before, &after),
                "a deck the fleet still observes keeps the transport it had, rather than being \
                 re-established: {}",
                deck.describe()
            );
        }

        // The contrast the name is making: the one-element set a single-deck
        // selection builds drops the other two off the same map.
        let single: HashSet<EndpointIdentity> = [fleet[0].identity()].into_iter().collect();
        tunnels.retain(&single).await;
        assert_eq!(tunnels.held().await, 1);
    }

    /// Removing ONE deck from the fleet tears down exactly that deck's
    /// transport and leaves the others' children running (PRD #742 M2).
    ///
    /// This is the property that makes a fleet safe to edit while it is live,
    /// and it is the one a "rebuild the whole map whenever the set changes"
    /// implementation fails silently: the count comes out right, every survivor
    /// is a *different* transport, and every lease a watcher or a terminal is
    /// holding now points at an `ssh` child nothing will ever reuse. So the
    /// survivors are asserted by identity of the `Arc`, and the departed deck by
    /// the map handing back a different one on the next acquire.
    #[tokio::test]
    async fn removing_one_deck_from_the_fleet_drops_only_that_decks_transport() {
        let tunnels = EndpointTunnels::default();
        let deck = |name: &str| {
            Endpoint::Local(LocalEndpoint::at(format!(
                "/tmp/dot-agent-deck-lease-edit-{name}.sock"
            )))
        };
        let (kept_a, removed, kept_b) = (deck("kept-a"), deck("removed"), deck("kept-b"));
        let lease_a = tunnels.acquire(&kept_a).await.expect("lease");
        let lease_removed = tunnels.acquire(&removed).await.expect("lease");
        let lease_b = tunnels.acquire(&kept_b).await.expect("lease");

        // The user deleted the middle row. Every other deck is still observed.
        let observed: HashSet<EndpointIdentity> =
            [kept_a.identity(), kept_b.identity()].into_iter().collect();
        tunnels.retain(&observed).await;

        assert_eq!(tunnels.held().await, 2, "exactly one deck left the fleet");
        for (deck, before) in [(&kept_a, &lease_a), (&kept_b, &lease_b)] {
            assert!(
                Arc::ptr_eq(before, &tunnels.acquire(deck).await.expect("still leased")),
                "editing one deck out of the fleet must not disturb another's transport: {}",
                deck.describe()
            );
        }
        assert!(
            !Arc::ptr_eq(
                &lease_removed,
                &tunnels.acquire(&removed).await.expect("re-leased")
            ),
            "the map genuinely let go of the removed deck: a later acquire establishes a new \
             transport rather than handing back the old one"
        );
        // Rule 2: the holder's own lease outlives the map's handle, so removing
        // a deck from a live fleet cannot tear the transport out from under
        // whoever is mid-request on it.
        assert_eq!(
            lease_removed.address(),
            std::path::Path::new("/tmp/dot-agent-deck-lease-edit-removed.sock"),
            "the departed deck's already-handed-out lease is still usable"
        );
    }

    /// A lease already handed out survives its removal from the map, which is
    /// what stops a selection change tearing the transport out from under a
    /// handshake that is still in flight.
    #[tokio::test]
    async fn a_held_lease_survives_release() {
        let tunnels = EndpointTunnels::default();
        let endpoint = Endpoint::Local(LocalEndpoint::at("/tmp/dot-agent-deck-lease-survive.sock"));
        let lease = tunnels.acquire(&endpoint).await.expect("lease");

        tunnels.release(&endpoint).await;

        assert_eq!(tunnels.held().await, 0, "the map let go");
        assert_eq!(
            lease.address(),
            std::path::Path::new("/tmp/dot-agent-deck-lease-survive.sock"),
            "the holder's lease is still usable"
        );
    }

    /// `close_all` is the app-exit teardown.
    #[tokio::test]
    async fn close_all_drops_every_transport() {
        let tunnels = EndpointTunnels::default();
        for name in ["a", "b", "c"] {
            let endpoint = Endpoint::Local(LocalEndpoint::at(format!(
                "/tmp/dot-agent-deck-lease-{name}.sock"
            )));
            tunnels.acquire(&endpoint).await.expect("lease");
        }
        assert_eq!(tunnels.held().await, 3);

        tunnels.close_all().await;

        assert_eq!(tunnels.held().await, 0);
    }
}
