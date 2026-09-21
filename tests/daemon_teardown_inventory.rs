// This file spawns stand-in agents through a real `AgentPtyRegistry` and
// inspects the `pane_id`-keyed maps a daemon holds beside it. `#![cfg(unix)]`
// keeps the crate empty on Windows so the cross-platform build compiles; the
// stand-in's `sleep 30` is a Unix command and the wrapped-child lifetime bound
// this file arms is a Unix mechanism. Same shape, and the same reason, as
// `tests/daemon_stop_wire.rs`.
#![cfg(unix)]
//! Issue #1109 — what the UNGUARDED daemon-teardown paths say about what they
//! are destroying.
//!
//! # The asymmetry this covers
//!
//! There are four ways to ASK a daemon to stop, and only two can be refused:
//!
//! | how you ask | guard | disclosure |
//! | --- | --- | --- |
//! | `dot-agent-deck daemon stop` | issue #770 refusal, in the client | the refusal itself |
//! | `AttachRequest::StopDaemon` | the same refusal, on the wire (#1049) | the refusal itself |
//! | a termination SIGNAL | none, deliberately | this file |
//! | the `KIND_SHUTDOWN` frame | none, deliberately | this file |
//!
//! (The first row delivers a `SIGTERM`, so past its refusal it lands in the
//! third. Three further paths end a daemon with nobody asking — the idle timer,
//! which fires only with no clients, agents or schedules, and the two env-gated
//! test backstops — and none is in scope here.)
//!
//! The bottom two do not refuse and, per the decision recorded in
//! `docs/develop/daemon-teardown-paths.md`, must not: a SIGTERM is how a
//! service manager, a container runtime or a session logout asks a daemon to
//! stop, and a daemon that argues back is escalated to SIGKILL on the sender's
//! clock — which loses the graceful drain *and* the disclosure. What they gained
//! instead is the ability to say what they took down, which is the half that
//! was actually missing: #428's occurrence #5 needed log archaeology to
//! establish that a stray `pkill -f "daemon serve"` had stopped nine panes
//! across three dispatched units, because the shutdown line named none of them.
//!
//! # Why the fixture is a real registry and a real `AppState`
//!
//! `daemon_stop::format_teardown_inventory`'s own unit tests drive the wording
//! from hand-built records. What they cannot reach is the JOIN in front of it —
//! `AppState::live_orchestration_roles` filtering `pane_role_map` by
//! `AgentPtyRegistry::has_live_pane` — which is where an inventory would go
//! quietly empty and report a destructive teardown as harmless. So this file
//! registers roles the way `spawn` registers them, against panes a real
//! registry really holds, and asserts on the daemon's own log output rather
//! than on a returned string.

// Issue #668 / linkage-check rule 10: this file builds an `AgentPtyRegistry`,
// so it must arm the wrapped-child lifetime bound.
#[path = "common/child_lifetime_bound.rs"]
mod child_lifetime_bound;

use std::sync::{Arc, Mutex};

use dot_agent_deck::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use dot_agent_deck::daemon_stop::log_teardown_inventory;
use dot_agent_deck::state::{AppState, OrchestrationIdentity};
use spec::spec;
use tokio::sync::RwLock;

/// The pane id shape `spawn::next_pane_id` mints for role 0 of a
/// daemon-spawned orchestration — the shape issue #770's incident reported.
const ORCHESTRATOR_PANE: &str = "sched-issue-work-42-r0";
const WORKER_PANE: &str = "sched-issue-work-42-r1";
/// A pane with a live agent but NO orchestration role. The inventory must name
/// it too: #428's nine lost panes were not all role panes, and an agent whose
/// work is gone is worth naming even when nothing permanent went with it.
const PLAIN_PANE: &str = "7";

/// A `tracing` writer that keeps everything in memory, so a test can assert on
/// what the daemon actually logged rather than on a string the production path
/// never handed to a subscriber.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(|p| p.into_inner())).into_owned()
    }
}

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
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

/// Register `panes` the way `spawn` registers a daemon-dispatched
/// orchestration: role 0 is the orchestrator, the rest are workers.
fn state_with_roles(panes: &[(&str, &str, bool)]) -> AppState {
    let mut state = AppState::default();
    let identity = OrchestrationIdentity::Instance {
        id: "inst-1109".to_string(),
        name: "issue-work".to_string(),
    };
    for (pane, role, is_start) in panes {
        state.register_orchestration_role(
            pane,
            role,
            *is_start,
            identity.clone(),
            Some("/home/dev/issue-work"),
        );
    }
    state
}

/// Run `log_teardown_inventory` with every `tracing` event captured. The
/// subscriber is a thread-local default, and `#[tokio::test]` builds a
/// current-thread runtime, so the future runs on the thread the guard was
/// installed on. Switching these tests to a multi-thread runtime would break
/// that and is the one change to make deliberately.
async fn captured_inventory(
    state: &dot_agent_deck::state::SharedState,
    registry: &AgentPtyRegistry,
    path: &'static str,
) -> String {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    log_teardown_inventory(state, registry, path).await;
    drop(guard);
    capture.text()
}

/// Scenario: Build a real `AgentPtyRegistry` holding three live stand-in panes
/// and an `AppState` that registers two of them as orchestration roles, exactly
/// as a daemon-dispatched orchestration would. Run the disclosure an unguarded
/// teardown emits, capturing the daemon's own `tracing` output, and assert the
/// log names every agent, every role, which one is the orchestrator, and that
/// the role registrations are gone for good.
#[spec("lifecycle/teardown-inventory/001")]
#[tokio::test]
async fn teardown_inventory_001_unguarded_teardown_names_what_it_destroys() {
    child_lifetime_bound::arm();
    let registry = Arc::new(AgentPtyRegistry::new());
    spawn_stand_in(&registry, ORCHESTRATOR_PANE);
    spawn_stand_in(&registry, WORKER_PANE);
    spawn_stand_in(&registry, PLAIN_PANE);
    let state: dot_agent_deck::state::SharedState = Arc::new(RwLock::new(state_with_roles(&[
        (ORCHESTRATOR_PANE, "orchestrator", true),
        (WORKER_PANE, "coder", false),
    ])));

    let log = captured_inventory(&state, &registry, "signal").await;

    for expected in [
        // which teardown path emitted it, so a reader does not have to
        // correlate this line against the one above it by timestamp
        "signal",
        "role_count=2",
        "terminating 3 managed agent(s)",
        "destroying 2 orchestration role registration(s)",
        ORCHESTRATOR_PANE,
        WORKER_PANE,
        // the role-less pane is named too — it is still somebody's work
        PLAIN_PANE,
        "(orchestrator)",
        "[issue-work]",
        "can never delegate again",
    ] {
        assert!(
            log.contains(expected),
            "the teardown disclosure must name {expected:?} — establishing what \
             a stray SIGTERM destroyed is the whole deliverable of issue #1109, \
             and #428's occurrence #5 needed log archaeology for exactly this.\n\
             log was:\n{log}"
        );
    }

    registry.shutdown_all();
}

/// Scenario: Build the same registry and state, but drain the registry first —
/// the order a teardown would take if the disclosure were emitted after the
/// drain instead of before it. Assert the log then claims nothing was at stake,
/// pinning the ordering the production call sites depend on.
#[spec("lifecycle/teardown-inventory/002")]
#[tokio::test]
async fn teardown_inventory_002_a_drained_registry_discloses_nothing() {
    child_lifetime_bound::arm();
    let registry = Arc::new(AgentPtyRegistry::new());
    spawn_stand_in(&registry, ORCHESTRATOR_PANE);
    let state: dot_agent_deck::state::SharedState = Arc::new(RwLock::new(state_with_roles(&[(
        ORCHESTRATOR_PANE,
        "orchestrator",
        true,
    )])));

    // What every unguarded teardown path does immediately after disclosing.
    registry.shutdown_all();

    let log = captured_inventory(&state, &registry, "signal").await;
    assert!(
        !log.contains(ORCHESTRATOR_PANE),
        "a drained registry reports no live pane, so `live_orchestration_roles` \
         filters the role out and the disclosure says nothing was lost. That is \
         why the production call sites emit BEFORE the drain, and this test is \
         what makes reordering them fail rather than silently empty the \
         inventory.\nlog was:\n{log}"
    );
}
