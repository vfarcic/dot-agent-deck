#![cfg(all(feature = "e2e", unix))]

//! L2 lane-1 coverage for PRD #742's fleet view: one client observing **two
//! real `dot-agent-deck daemon serve` processes** at once.
//!
//! # What this file exists to prove, and what already proves the rest
//!
//! The fleet's correctness properties — deck identity, fold isolation, per-deck
//! degradation, fleet membership — are pinned in the desktop crate's own fast
//! tier, against two production attach servers bound **in process**
//! (`desktop/src-tauri/src/daemon_bridge.rs`, the `RealDeck` tests). Those run
//! on three platforms in `cargo test-fast` and they reach code this file
//! cannot, because `DaemonLinks`, `snapshot_with` and `connection_from_handshake`
//! are `pub(crate)` to that crate.
//!
//! What an in-process server cannot show is the thing here: that a client can
//! talk to a daemon **it did not bind itself**. Both sockets below are created
//! by a separate process, at a time the client does not control, under a
//! lifecycle it does not own — and the client's first act against each is the
//! production trust check on an inode a real `daemon serve` made.
//!
//! # Why the client half is `DaemonClient` and not the desktop's own types
//!
//! It is not a simplification, it is the reachable boundary. The desktop crate
//! depends on this one by path, so a dev-dependency the other way would make
//! Tauri — and its GTK/WebKit closure — a build input of this tier on all three
//! platforms, and the types the fleet's logic lives in are `pub(crate)` anyway.
//! What IS shared is the part this file asserts: `DaemonClient` is the type the
//! desktop's `TrustedDaemon` holds (`transport.client()`), and
//! `EndpointIdentity::wire_id()` is the whole body of the desktop's
//! `deck_wire_id()` — the string every snapshot is stamped with and every
//! frontend row keyed by. So the identity asserted below is the identity the
//! fleet view renders, arrived at from two real daemon processes.
//!
//! Lane 1: no credential and no real agent — the agents are `sh -c 'sleep 600'`
//! stand-ins, exactly as `tests/e2e_handshake.rs` uses. `unix` on the file gate
//! because `DaemonProc` binds Unix-domain sockets. Every wait is a `common`
//! helper (linkage-check Decision 21).

mod common;

use std::time::Duration;

use common::{DaemonProc, spawn_daemon_serve_with_env};
use dot_agent_deck::daemon_client::{DaemonClient, Endpoint, LocalEndpoint};
use dot_agent_deck::daemon_protocol::AttachRequest;
use spec::spec;

/// Display names distinctive enough that finding one in the other deck's
/// listing is unambiguous — no shared substring, and neither is a prefix of
/// anything the registry generates.
const ALPHA_AGENT: &str = "zulu-alpha-41";
const BRAVO_AGENT: &str = "quebec-bravo-58";

/// Start one long-lived stand-in agent on `daemon` and block until its registry
/// reports it.
fn start_stand_in(daemon: &DaemonProc, display_name: &str, pane_id: &str) {
    let response = daemon
        .send_attach_request(&AttachRequest::StartAgent {
            command: Some("sh -c 'sleep 600'".into()),
            cwd: None,
            rows: 24,
            cols: 80,
            env: vec![("DOT_AGENT_DECK_PANE_ID".into(), pane_id.into())],
            display_name: Some(display_name.into()),
            tab_membership: None,
            agent_type: None,
            seed: None,
            authoring_kind: None,
        })
        .expect("StartAgent over the attach socket");
    assert!(
        response.error.is_none(),
        "StartAgent should succeed, got error: {:?}",
        response.error
    );
    let records = daemon.wait_for_agent_count(1, Duration::from_secs(10));
    assert_eq!(
        records.len(),
        1,
        "the stand-in agent must be registered before the fleet is observed"
    );
}

/// Scenario: start two real `dot-agent-deck daemon serve` processes on two
/// isolated endpoints, put one stand-in agent on each, and observe both from a
/// single client that bound neither socket. Each deck must clear the production
/// trust check, answer its own `Hello`, and list only its own agent under its
/// own deck identity — even though both registries mint the same agent id — and
/// killing one daemon process must leave the other answering unchanged.
#[spec("fleet/observe/001")]
#[test]
fn observe_001_two_real_daemon_processes_are_observed_as_two_decks() {
    common::init_test_env();

    // Each daemon gets its own log path. CLAUDE.md rule 12's trap is closed by
    // construction on this path rather than by these two lines:
    // `spawn_daemon_serve_with_env` `env_clear`s the child, so a sandbox daemon
    // cannot inherit an operator's exported `DOT_AGENT_DECK_LOG`, and
    // `init_logging_from_env` installs a file writer ONLY when that variable is
    // set — there is no default tracing-log path to fall back onto. (The
    // daemon's OTHER log sink, `<state_dir>/daemon.log` on the detached-spawn
    // path, is isolated by the harness's `DOT_AGENT_DECK_STATE_DIR`.) So these
    // buy diagnosis rather than isolation: a red run in CI has two daemons'
    // worth of tracing to read.
    let logs = common::harness_tempdir().expect("fleet log tempdir");
    let alpha_log = logs.path().join("alpha.log").to_string_lossy().into_owned();
    let bravo_log = logs.path().join("bravo.log").to_string_lossy().into_owned();

    // `"0"` disables the 30-second idle window, so neither daemon can exit
    // under the client mid-test (CLAUDE.md rule 12's second mouth).
    let alpha =
        spawn_daemon_serve_with_env(None, "0", &[("DOT_AGENT_DECK_LOG", alpha_log.as_str())]);
    let bravo =
        spawn_daemon_serve_with_env(None, "0", &[("DOT_AGENT_DECK_LOG", bravo_log.as_str())]);
    // Asserted rather than described: `init_logging_from_env` runs on the
    // `daemon serve` path before anything binds, so by the time
    // `spawn_daemon_serve_with_env` returned each daemon had already created its
    // own file. Two files, two daemons, and each one inside this test's sandbox.
    for (label, path) in [("alpha", &alpha_log), ("bravo", &bravo_log)] {
        assert!(
            std::path::Path::new(path).is_file(),
            "deck {label} must be logging to its own sandbox path ({path})"
        );
    }

    start_stand_in(&alpha, ALPHA_AGENT, "pane-alpha");
    start_stand_in(&bravo, BRAVO_AGENT, "pane-bravo");

    // The client side. Two endpoints the client did not create and does not own
    // — the inodes belong to two other processes.
    let deck_alpha = Endpoint::Local(LocalEndpoint::at(&alpha.attach_socket));
    let deck_bravo = Endpoint::Local(LocalEndpoint::at(&bravo.attach_socket));

    // The production gate `daemon_bridge::establish` runs before it connects,
    // against an inode a real `daemon serve` bound rather than one a fixture
    // chmod'd afterwards.
    for (label, deck) in [("alpha", &deck_alpha), ("bravo", &deck_bravo)] {
        let path = deck.as_local().expect("a local deck").path();
        dot_agent_deck::platform::fsperm::verify_endpoint_trusted(path)
            .unwrap_or_else(|reason| panic!("deck {label}'s socket must be trusted: {reason}"));
    }

    // `EndpointIdentity::wire_id()` IS the desktop's `deck_wire_id()`, so this
    // is the identity the fleet's rows are keyed by.
    let id_alpha = deck_alpha.identity().wire_id();
    let id_bravo = deck_bravo.identity().wire_id();
    assert_ne!(
        id_alpha, id_bravo,
        "two daemon processes must mint two deck identities — one identity for \
         two decks renders one fleet where there are two"
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build the fleet-observation runtime");
    let client_alpha = DaemonClient::for_endpoint(&deck_alpha).expect("a client for deck alpha");
    let client_bravo = DaemonClient::for_endpoint(&deck_bravo).expect("a client for deck bravo");

    // Both handshakes, concurrently, from one client process. This is the real
    // `Hello` each daemon answers with its own capability advertisement.
    let (caps_alpha, caps_bravo) = runtime
        .block_on(async { tokio::join!(client_alpha.capabilities(), client_bravo.capabilities()) });
    let caps_alpha = caps_alpha.expect("deck alpha answers its handshake");
    let caps_bravo = caps_bravo.expect("deck bravo answers its handshake");
    assert!(
        caps_alpha.is_advertised() && caps_bravo.is_advertised(),
        "both real daemons advertise a capability set"
    );

    let (listed_alpha, listed_bravo) = runtime
        .block_on(async { tokio::join!(client_alpha.list_agents(), client_bravo.list_agents()) });
    let listed_alpha = listed_alpha.expect("deck alpha lists its agents");
    let listed_bravo = listed_bravo.expect("deck bravo lists its agents");

    assert_eq!(
        listed_alpha.len(),
        1,
        "deck alpha carries exactly its own agent"
    );
    assert_eq!(
        listed_bravo.len(),
        1,
        "deck bravo carries exactly its own agent"
    );
    assert_eq!(listed_alpha[0].display_name.as_deref(), Some(ALPHA_AGENT));
    assert_eq!(listed_bravo[0].display_name.as_deref(), Some(BRAVO_AGENT));
    assert_eq!(listed_alpha[0].pane_id_env.as_deref(), Some("pane-alpha"));
    assert_eq!(listed_bravo[0].pane_id_env.as_deref(), Some("pane-bravo"));

    // The case a bare-id key gets wrong: two independent registries mint the
    // same first id, so only the deck half of `(deckId, agentId)` separates
    // these two agents.
    assert_eq!(
        listed_alpha[0].id, listed_bravo[0].id,
        "two registries must really mint the same first id, or the attribution \
         below proves nothing"
    );

    // One real daemon PROCESS dies. The survivor must be untouched — not merely
    // still connectable, but answering with the same agent it had before.
    drop(bravo);
    let (after_alpha, after_bravo) = runtime
        .block_on(async { tokio::join!(client_alpha.list_agents(), client_bravo.list_agents()) });
    let after_alpha = after_alpha.expect("the surviving deck must keep answering");
    assert_eq!(after_alpha.len(), 1);
    assert_eq!(after_alpha[0].display_name.as_deref(), Some(ALPHA_AGENT));
    assert!(
        after_bravo.is_err(),
        "the dead deck must fail on its own rather than answering from a cache: \
         {after_bravo:?}"
    );
    assert_eq!(
        deck_alpha.identity().wire_id(),
        id_alpha,
        "and the survivor's identity does not move because a sibling deck went \
         away"
    );
}
