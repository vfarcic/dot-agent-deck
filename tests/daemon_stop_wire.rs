// The attach server this file brings up binds a Unix-domain socket, and the
// relay in `wire_stop/003` forwards between two of them. `#![cfg(unix)]` keeps
// the crate empty on Windows so the cross-platform build compiles; on Unix
// every test runs. Same shape, and the same reason, as
// `tests/orchestration_orphan.rs`.
#![cfg(unix)]
//! Issue #1049 — the wire verb that stops a deck, and the refusal it carries.
//!
//! # What was actually missing
//!
//! The issue enumerates all seventeen `AttachRequest` variants and finds none
//! that stops the deck, which is true — but it is not the whole picture, and the
//! difference decides what this file has to prove. There were already **two**
//! ways to stop a deck:
//!
//! 1. `daemon stop` → `daemon_stop::run_daemon_stop`, which reads the daemon's
//!    pid off the socket with `SO_PEERCRED` and signals it. It carries the
//!    issue #770 refusal, and it is local-only *by type* since PRD #741 M2 —
//!    over a forwarded socket the credential names the `ssh` client, not the
//!    daemon.
//! 2. The `KIND_SHUTDOWN` frame → `DaemonClient::send_shutdown`, behind the
//!    `Stop` option of the Ctrl+C dialog. It is a plain protocol frame, so it
//!    already worked over **any** transport — and it carries **no guard at
//!    all**: it drains every managed agent unconditionally and names nothing it
//!    is about to destroy.
//!
//! So the gap was never simply "no remote stop". A remotely reachable,
//! completely unguarded stop was already on this wire; what did not exist was a
//! **guarded** one. `AttachRequest::StopDaemon` is that verb, and the refusal is
//! the whole reason it exists — which is why the refusal tests here outnumber
//! the success test, and why `wire_stop/001` asserts the daemon is still
//! serving afterwards rather than only that the reply said no.
//!
//! Whether `KIND_SHUTDOWN` should itself be narrowed is issue #1109's open
//! question and is deliberately not answered here.

// Issue #322: disk-backed scratch dirs resolved through the crate-internal
// helper rather than a bare `tempfile` constructor, which linkage-check rule 8
// rejects anywhere under `tests/`.
#[path = "../src/test_temp.rs"]
mod test_temp;
// Issue #668 / linkage-check rule 10: this file builds an `AgentPtyRegistry`,
// so it must arm the wrapped-child lifetime bound.
#[path = "common/child_lifetime_bound.rs"]
mod child_lifetime_bound;

use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dot_agent_deck::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use dot_agent_deck::daemon_client::issue_command;
use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, CAP_STOP_DAEMON, StopRefusalReason, bind_attach_listener,
    serve_attach_with_counter,
};
use dot_agent_deck::daemon_stop::{StopError, WireStopOutcome, run_daemon_stop_over_wire};
use dot_agent_deck::platform::ipc::IpcStream;
use dot_agent_deck::state::{AppState, OrchestrationIdentity};
use spec::spec;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

/// `bind_attach_listener` flips the process-global umask while binding; the
/// other suites that bind one share a lock for the same reason.
static HARNESS_BIND_LOCK: Mutex<()> = Mutex::new(());

/// The pane id shape `spawn::next_pane_id` mints for role 0 of a daemon-spawned
/// orchestration — the shape issue #770's incident reported.
const ORCHESTRATOR_PANE: &str = "sched-issue-work-42-r0";
const WORKER_PANE: &str = "sched-issue-work-42-r1";
/// A pane with a live agent but NO orchestration role, for the agents-only
/// guard.
const PLAIN_PANE: &str = "7";

struct Server {
    _dir: TempDir,
    path: PathBuf,
    handle: JoinHandle<()>,
    registry: Arc<AgentPtyRegistry>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
        // The stand-ins are real child processes; reap them rather than leaving
        // a `sleep 30` behind for the harness to trip over.
        self.registry.shutdown_all();
    }
}

/// Serve the production attach dispatch over a real attach socket, backed by the
/// caller's `AppState`. Nothing is stubbed: the `StopDaemon` arm under test is
/// the one a real daemon runs.
///
/// No shutdown `Notify` is wired, for the reason `serve_attach_with_counter`'s
/// own callers document — a test does not run the production hook loop. The
/// handler logs that and still drains the registry, which is the side effect
/// `wire_stop/004` asserts on.
async fn start_server(
    registry: Arc<AgentPtyRegistry>,
    state: dot_agent_deck::state::SharedState,
) -> Server {
    let (dir, path, listener) = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = test_temp::tempdir().unwrap();
        let path = dir.path().join("attach.sock");
        let listener = bind_attach_listener(&path).expect("bind attach listener");
        (dir, path, listener)
    };
    let (event_tx, _rx) = tokio::sync::broadcast::channel(16);
    let client_count = Arc::new(AtomicUsize::new(0));
    let scheduler = Arc::new(dot_agent_deck::scheduler::Scheduler::with_stderr_notifier());
    let reuse = dot_agent_deck::spawn::new_reuse_registry();
    let worktrees = dot_agent_deck::issue_dispatch_run::new_worktree_registry();
    let registry_for_task = registry.clone();
    let handle = tokio::spawn(async move {
        let _ = serve_attach_with_counter(
            listener,
            registry_for_task,
            event_tx,
            client_count,
            state,
            None,
            scheduler,
            reuse,
            worktrees,
        )
        .await;
    });
    Server {
        _dir: dir,
        path,
        handle,
        registry,
    }
}

/// Spawn a stand-in agent occupying `pane`, so the registry claims it.
fn spawn_stand_in(registry: &Arc<AgentPtyRegistry>, pane: &str) {
    registry
        .spawn_agent(SpawnOptions {
            command: Some("sleep 30"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
            ..SpawnOptions::default()
        })
        .expect("stand-in agent should spawn");
}

/// An `AppState` holding the two live orchestration roles.
fn state_with_roles() -> AppState {
    let mut state = AppState::default();
    let identity = OrchestrationIdentity::Instance {
        id: "inst-1049".to_string(),
        name: "issue-work".to_string(),
    };
    state.register_orchestration_role(
        ORCHESTRATOR_PANE,
        "orchestrator",
        true,
        identity.clone(),
        Some("/home/dev/issue-work"),
    );
    state.register_orchestration_role(
        WORKER_PANE,
        "coder",
        false,
        identity,
        Some("/home/dev/issue-work"),
    );
    state
}

/// One `StopDaemon` round trip over `path`.
async fn ask_stop(path: &std::path::Path, force: bool) -> AttachResponse {
    let stream = IpcStream::connect(path).await.expect("connect");
    let (mut rd, mut wr) = stream.into_split();
    let resp = issue_command(&mut rd, &mut wr, &AttachRequest::StopDaemon { force })
        .await
        .expect("stop-daemon round trip");
    drop(rd);
    drop(wr);
    resp
}

/// Poll `cond` until it holds or `budget` elapses.
async fn wait_until(budget: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    cond()
}

/// Scenario: Serve a real attach socket over an `AppState` holding two
/// orchestration roles whose panes have live stand-in agents, then send
/// `StopDaemon` with no force. The daemon must refuse, carry the panes, roles
/// and both renderings back on the wire, and — the part that matters — still be
/// serving afterwards.
#[spec("lifecycle/wire-stop/001")]
#[test]
fn wire_stop_001_refuses_over_the_wire_while_orchestration_roles_are_live() {
    child_lifetime_bound::arm();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(wire_stop_001_inner());
}

async fn wire_stop_001_inner() {
    let registry = Arc::new(AgentPtyRegistry::new());
    for pane in [ORCHESTRATOR_PANE, WORKER_PANE] {
        spawn_stand_in(&registry, pane);
    }
    let shared = Arc::new(tokio::sync::RwLock::new(state_with_roles()));
    let server = start_server(registry.clone(), shared).await;

    let resp = ask_stop(&server.path, false).await;

    assert!(
        !resp.ok,
        "a daemon holding live orchestration roles must refuse the wire stop"
    );
    let refusal = resp.stop_refusal.expect(
        "the refusal must cross the wire STRUCTURED — a remote caller has no other view \
                 of this deck's panes, so a bare string leaves it unable to present the choice",
    );
    assert_eq!(
        refusal.reason,
        StopRefusalReason::LiveOrchestrations,
        "the orchestration guard is the one that tripped, and a caller must be able to branch \
         on that without parsing prose"
    );

    // The panes and roles at stake, which is the whole content of the #770
    // refusal: WHAT is being destroyed, not just that something is.
    let named: Vec<(String, String, bool)> = refusal
        .roles
        .iter()
        .map(|r| (r.pane_id.clone(), r.role.clone(), r.is_orchestrator))
        .collect();
    assert_eq!(
        named,
        vec![
            (
                ORCHESTRATOR_PANE.to_string(),
                "orchestrator".to_string(),
                true
            ),
            (WORKER_PANE.to_string(), "coder".to_string(), false),
        ],
        "both live roles must cross, with the orchestrator flagged and a stable order"
    );
    assert_eq!(
        refusal.agent_ids.len(),
        2,
        "the managed agents that would go down with the daemon cross too: {:?}",
        refusal.agent_ids
    );

    // Both renderings, and they must say different useful things.
    assert!(
        refusal.message.contains(ORCHESTRATOR_PANE)
            && refusal.message.contains("can never delegate again")
            && refusal.message.contains("--force"),
        "the multi-line message must name the panes, say the loss is PERMANENT rather than a \
         termination, and point at the override: {:?}",
        refusal.message
    );
    assert!(
        refusal.message.ends_with('\n'),
        "a terminal caller eprint!s this directly, so it must end in a newline"
    );
    assert!(
        refusal.summary.contains("--force") && !refusal.summary.contains('\n'),
        "the summary must be ONE line and still point at the way out — a caller with a status \
         bar shows this one: {:?}",
        refusal.summary
    );
    assert_eq!(
        resp.error.as_deref(),
        Some(refusal.summary.as_str()),
        "a client that knows nothing about `stop_refusal` reads `error`, so the two must be the \
         same sentence rather than two accounts of one refusal"
    );

    // The safety property itself. A reply saying "no" is worth nothing if the
    // daemon tore itself down while saying it.
    assert!(
        registry.has_live_pane(ORCHESTRATOR_PANE) && registry.has_live_pane(WORKER_PANE),
        "a REFUSED stop must not have drained the registry"
    );
    let again = ask_stop(&server.path, false).await;
    assert!(
        !again.ok && again.stop_refusal.is_some(),
        "the daemon must still be serving after refusing, and refuse the same way again"
    );
}

/// Scenario: Serve a real attach socket over an `AppState` holding NO
/// orchestration roles, with one plain live agent, then send `StopDaemon` with
/// no force. The pre-existing managed-agent guard must refuse over the wire and
/// say so as the agents guard, not the orchestration one.
#[spec("lifecycle/wire-stop/002")]
#[test]
fn wire_stop_002_refuses_over_the_wire_for_plain_managed_agents() {
    child_lifetime_bound::arm();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(wire_stop_002_inner());
}

async fn wire_stop_002_inner() {
    let registry = Arc::new(AgentPtyRegistry::new());
    spawn_stand_in(&registry, PLAIN_PANE);
    let shared = Arc::new(tokio::sync::RwLock::new(AppState::default()));
    let server = start_server(registry.clone(), shared).await;

    let resp = ask_stop(&server.path, false).await;

    assert!(!resp.ok, "a daemon hosting managed agents must refuse");
    let refusal = resp.stop_refusal.expect("structured refusal");
    assert_eq!(
        refusal.reason,
        StopRefusalReason::LiveAgents,
        "with no roles registered this is the agents guard — reporting the orchestration one \
         would tell an operator their run is at stake when it is not"
    );
    assert!(
        refusal.roles.is_empty(),
        "no roles are registered, so none may be claimed: {:?}",
        refusal.roles
    );
    assert_eq!(refusal.agent_ids.len(), 1, "the one agent must be named");
    assert!(
        refusal.message.contains("managed agent(s) running") && refusal.message.contains("--force"),
        "the agents refusal keeps its own wording: {:?}",
        refusal.message
    );
    assert!(
        registry.has_live_pane(PLAIN_PANE),
        "a refused stop must not have drained the registry"
    );
}

/// Scenario: Put a byte-forwarding relay in front of the attach socket — what an
/// `ssh -L` tunnel is to a client — and drive the whole `run_daemon_stop_over_wire`
/// flow at the relay's address instead of the daemon's. The refusal must arrive
/// intact across both hops, proving the verb needs nothing but a path that can
/// carry frames: no pid, no peer credential, no shared kernel view.
#[spec("lifecycle/wire-stop/003")]
#[test]
fn wire_stop_003_refusal_survives_a_forwarding_relay() {
    child_lifetime_bound::arm();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(wire_stop_003_inner());
}

async fn wire_stop_003_inner() {
    let registry = Arc::new(AgentPtyRegistry::new());
    for pane in [ORCHESTRATOR_PANE, WORKER_PANE] {
        spawn_stand_in(&registry, pane);
    }
    let shared = Arc::new(tokio::sync::RwLock::new(state_with_roles()));
    let server = start_server(registry.clone(), shared).await;

    let relay = start_relay(&server.path).await;

    // The real client entry point, at the relay's address. A remote caller holds
    // exactly this: a local path that forwards to a daemon it cannot see.
    let err = run_daemon_stop_over_wire(&relay.path, false)
        .await
        .expect_err("the daemon must refuse across the relay just as it does directly");
    let StopError::Refused(refusal) = err else {
        panic!("expected a carried refusal, got {err:?}");
    };
    assert_eq!(refusal.reason, StopRefusalReason::LiveOrchestrations);
    assert!(
        refusal.message.contains(ORCHESTRATOR_PANE)
            && refusal.message.contains("can never delegate again"),
        "the panes and the permanence must survive the forward — this is the ONLY view a remote \
         caller has of them: {:?}",
        refusal.message
    );
    assert!(
        registry.has_live_pane(ORCHESTRATOR_PANE),
        "a refused stop must not have drained the registry, relay or no relay"
    );
}

/// Scenario: Serve a real attach socket holding live orchestration roles and
/// send `StopDaemon` with `force: true`. The daemon must accept, answer BEFORE
/// tearing down, and drain the registry — the override the refusal points at,
/// doing what it says.
#[spec("lifecycle/wire-stop/004")]
#[test]
fn wire_stop_004_force_clears_the_refusal_and_drains_the_registry() {
    child_lifetime_bound::arm();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(wire_stop_004_inner());
}

async fn wire_stop_004_inner() {
    let registry = Arc::new(AgentPtyRegistry::new());
    for pane in [ORCHESTRATOR_PANE, WORKER_PANE] {
        spawn_stand_in(&registry, pane);
    }
    let shared = Arc::new(tokio::sync::RwLock::new(state_with_roles()));
    let server = start_server(registry.clone(), shared).await;

    // Driven at the protocol level rather than through
    // `run_daemon_stop_over_wire`, deliberately: this harness wires no shutdown
    // `Notify` (no production hook loop), so the server keeps accepting and the
    // confirmation poll would spend its whole budget before reporting
    // `AcceptedNotConfirmed`. The drain below is the side effect that actually
    // proves the accept, and it is observable here.
    let resp = ask_stop(&server.path, true).await;

    assert!(resp.ok, "--force must clear both guards: {:?}", resp.error);
    assert!(
        resp.stop_refusal.is_none(),
        "an accepted stop carries no refusal"
    );
    let drained = wait_until(Duration::from_secs(10), || {
        !registry.has_live_pane(ORCHESTRATOR_PANE) && !registry.has_live_pane(WORKER_PANE)
    })
    .await;
    assert!(
        drained,
        "the accepted stop must drain the registry through the SAME graceful path \
         `KIND_SHUTDOWN` uses"
    );
}

/// Scenario: Ask a path nothing is listening on to stop. It must report no
/// daemon running rather than erroring, so a caller retrying a stop is
/// idempotent — the same contract the PID path has.
#[spec("lifecycle/wire-stop/005")]
#[test]
fn wire_stop_005_absent_daemon_is_idempotent() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let dir = test_temp::tempdir().unwrap();
        let missing = dir.path().join("no-such-daemon.sock");
        let outcome = run_daemon_stop_over_wire(&missing, false)
            .await
            .expect("an absent daemon is not an error");
        assert_eq!(outcome, WireStopOutcome::NoDaemonRunning);
    });
}

/// Scenario: Ask a live daemon for its capabilities over `Hello`. It must
/// advertise the wire stop, because a remote client that cannot find this
/// capability has no second path to fall back to — the PID stop needs a local
/// endpoint — and must say the deck is too old rather than send a frame that
/// comes back as `unknown variant`.
#[spec("lifecycle/wire-stop/006")]
#[test]
fn wire_stop_006_capability_is_advertised() {
    child_lifetime_bound::arm();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let registry = Arc::new(AgentPtyRegistry::new());
        let shared = Arc::new(tokio::sync::RwLock::new(AppState::default()));
        let server = start_server(registry, shared).await;

        let stream = IpcStream::connect(&server.path).await.expect("connect");
        let (mut rd, mut wr) = stream.into_split();
        let resp = issue_command(
            &mut rd,
            &mut wr,
            &AttachRequest::Hello {
                client_version: dot_agent_deck::daemon_protocol::PROTOCOL_VERSION,
                client_build_version: None,
            },
        )
        .await
        .expect("hello");
        drop(rd);
        drop(wr);

        let caps = resp
            .capabilities
            .expect("a current daemon advertises its capability set");
        assert!(
            caps.iter().any(|c| c == CAP_STOP_DAEMON),
            "the wire stop must be advertised, or a remote client withholds it forever: {caps:?}"
        );
    });
}

/// Scenario: Point the wire stop at a socket that ACCEPTS connections and then
/// stays permanently silent — what a forwarded socket does when its upstream is
/// stalled. The call must give up on its own budget rather than blocking
/// forever, and must report the outcome as unknown rather than as a stop.
#[spec("lifecycle/wire-stop/007")]
#[test]
fn wire_stop_007_a_silent_peer_times_out_instead_of_hanging() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let sink = start_black_hole().await;

        let started = std::time::Instant::now();
        let err = tokio::time::timeout(
            Duration::from_secs(20),
            dot_agent_deck::daemon_stop::run_daemon_stop_over_wire_with(
                &sink.path,
                false,
                Duration::from_millis(400),
                Duration::from_millis(400),
            ),
        )
        .await
        .expect("the call must return on its own — a hang here is the bug")
        .expect_err("a peer that never answers is not a successful stop");

        assert!(
            matches!(err, StopError::WireTimedOut),
            "a silent peer must surface as WireTimedOut, got {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the request budget must bound the wait, took {:?}",
            started.elapsed()
        );
        // The message must not claim the daemon did or did not see the request —
        // this side genuinely does not know, and a retry is the recovery.
        let msg = err.to_string();
        assert!(
            msg.contains("unknown") && msg.contains("retrying is safe"),
            "the error must say the outcome is unknown and that a retry is safe: {msg:?}"
        );
    });
}

/// Scenario: Accept the stop, then make every confirmation probe hang rather
/// than refuse — a wedged forward, not a dead daemon. The call must report
/// `AcceptedNotConfirmed`, never `Stopped`: a stop path that reports success for
/// a daemon it cannot see is the exact defect issue #1049 was filed about.
#[spec("lifecycle/wire-stop/008")]
#[test]
fn wire_stop_008_a_wedged_probe_is_never_reported_as_stopped() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let sink = start_accept_then_hang().await;

        let outcome = tokio::time::timeout(
            Duration::from_secs(20),
            dot_agent_deck::daemon_stop::run_daemon_stop_over_wire_with(
                &sink.path,
                true,
                Duration::from_secs(5),
                Duration::from_millis(600),
            ),
        )
        .await
        .expect("must return on its own")
        .expect("the stop was accepted, so this is not an error");

        assert_eq!(
            outcome,
            WireStopOutcome::AcceptedNotConfirmed,
            "a probe that hangs proves nothing about the daemon, so it must NOT be \
             promoted to Stopped — silence is a stalled peer, while a daemon that \
             exits closes its socket and its peer sees EOF"
        );
    });
}

// ---------------------------------------------------------------------------
// The relay
// ---------------------------------------------------------------------------

struct Relay {
    _dir: TempDir,
    path: PathBuf,
    handle: JoinHandle<()>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// A byte-forwarding proxy in front of `upstream`, standing in for the local end
/// of an `ssh -L` tunnel: it accepts connections and copies bytes both ways,
/// understanding nothing about frames.
///
/// **What this proves, precisely.** That `StopDaemon` needs only a path that can
/// carry bytes — no pid, no `SO_PEERCRED`, no shared kernel view of the peer —
/// which is exactly the property `run_daemon_stop` lacks and the reason PRD #741
/// M2 made that function `LocalEndpoint`-only.
///
/// **What it does not prove.** That `SO_PEERCRED` at the daemon's end would name
/// the relay rather than the daemon. It would, over a real tunnel, but both ends
/// here live in the test process, so the pid coincides and the substitution is
/// not observable. That half is a property of the transport rather than of this
/// code, and it is what the issue #1049 cross-version run exercised against a
/// real daemon.
async fn start_relay(upstream: &std::path::Path) -> Relay {
    let (dir, path, listener) = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = test_temp::tempdir().unwrap();
        let path = dir.path().join("relay.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("bind relay");
        (dir, path, listener)
    };
    let upstream = upstream.to_path_buf();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut client, _)) = listener.accept().await else {
                return;
            };
            let upstream = upstream.clone();
            tokio::spawn(async move {
                let Ok(mut server) = tokio::net::UnixStream::connect(&upstream).await else {
                    return;
                };
                // `copy_bidirectional` is not used: it completes only when BOTH
                // halves see EOF, and the daemon's dispatch answers one request
                // and closes, so the client half would hold the relay open. Two
                // independent pumps let each direction end on its own.
                let (mut cr, mut cw) = client.split();
                let (mut sr, mut sw) = server.split();
                let to_server = async {
                    let _ = tokio::io::copy(&mut cr, &mut sw).await;
                    let _ = sw.shutdown().await;
                };
                let to_client = async {
                    let _ = tokio::io::copy(&mut sr, &mut cw).await;
                    let _ = cw.shutdown().await;
                };
                tokio::join!(to_server, to_client);
            });
        }
    });
    Relay {
        _dir: dir,
        path,
        handle,
    }
}

// ---------------------------------------------------------------------------
// Stand-in peers for the timeout / confirmation tests
// ---------------------------------------------------------------------------

struct Sink {
    _dir: TempDir,
    path: PathBuf,
    handle: JoinHandle<()>,
}

impl Drop for Sink {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn bind_sink() -> (TempDir, PathBuf, tokio::net::UnixListener) {
    let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = test_temp::tempdir().unwrap();
    let path = dir.path().join("sink.sock");
    let listener = tokio::net::UnixListener::bind(&path).expect("bind sink");
    (dir, path, listener)
}

/// Accepts, then never writes and never closes — a forwarded socket whose
/// upstream is stalled. Holding the connection is the point: a peer that closed
/// would produce an EOF, which is a different and much easier case.
async fn start_black_hole() -> Sink {
    let (dir, path, listener) = bind_sink();
    let handle = tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            match listener.accept().await {
                Ok((conn, _)) => held.push(conn),
                Err(_) => return,
            }
        }
    });
    Sink {
        _dir: dir,
        path,
        handle,
    }
}

/// Answers the FIRST request (the stop, which it accepts) and then hangs on
/// every later connection — an accepted stop followed by a wedged forward.
async fn start_accept_then_hang() -> Sink {
    let (dir, path, listener) = bind_sink();
    let handle = tokio::spawn(async move {
        let mut first = true;
        let mut held = Vec::new();
        loop {
            let Ok((mut conn, _)) = listener.accept().await else {
                return;
            };
            if first {
                first = false;
                // Read the request frame, then answer `ok` exactly as the daemon's
                // accept path does.
                let mut hdr = [0u8; 5];
                if conn.read_exact(&mut hdr).await.is_err() {
                    continue;
                }
                let n = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
                let mut body = vec![0u8; n];
                if conn.read_exact(&mut body).await.is_err() {
                    continue;
                }
                let payload = br#"{"ok":true}"#;
                let mut out = vec![0x02u8];
                out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                out.extend_from_slice(payload);
                let _ = conn.write_all(&out).await;
                let _ = conn.flush().await;
            } else {
                // Hold it open and silent.
                held.push(conn);
            }
        }
    });
    Sink {
        _dir: dir,
        path,
        handle,
    }
}
