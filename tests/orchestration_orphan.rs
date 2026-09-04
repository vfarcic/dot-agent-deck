// The attach server this file brings up binds a Unix-domain socket, and
// `daemon stop`'s pid discovery reads `SO_PEERCRED` off it. `#![cfg(unix)]`
// keeps the crate empty on Windows so the cross-platform build compiles;
// on Unix every test runs. Same shape, and the same reason, as
// `tests/rehydration.rs`.
#![cfg(unix)]
//! Issue #770 — a daemon restart silently orphans running orchestrations.
//!
//! The mechanism: `AppState::register_orchestration_role` populates
//! `pane_role_map` / `pane_orchestration_map` / `orchestrator_pane_ids` /
//! `pane_cwd_map` in memory, with no persistence path anywhere. An agent that
//! has detached from the PTY it was born under (`ppid = 1`) survives a daemon
//! restart untouched, so the daemon that comes up next holds no role for its
//! pane. `handle_delegate` then refuses every `dot-agent-deck delegate` from it
//! for the rest of the run — while the same daemon keeps ACCEPTING its hook
//! events, so its status keeps updating and its card keeps looking healthy.
//! That asymmetry is the whole cost of the bug: the failure surfaces only at
//! the next delegate, which for an orchestrator is hours into a run.
//!
//! Two halves are pinned here, matching the first two items of the issue's
//! own slice:
//!
//! 1. **Prevention** — `daemon stop` refuses by default while the daemon holds
//!    live orchestration roles (`orphan/003`).
//! 2. **Surfacing** — the daemon marks hook events from an orphaned role pane,
//!    and the card says so (`orphan/001`, `orphan/002`).
//!
//! Self-healing re-registration (the issue's third item) is deliberately not
//! implemented and not tested.

// Issue #322: disk-backed scratch dirs resolved through the crate-internal
// helper rather than a bare `tempfile` constructor, which linkage-check rule 8
// rejects anywhere under `tests/`.
#[path = "../src/test_temp.rs"]
mod test_temp;
// Issue #668 / linkage-check rule 10: this file builds an `AgentPtyRegistry`,
// so it must arm the wrapped-child lifetime bound.
#[path = "common/child_lifetime_bound.rs"]
mod child_lifetime_bound;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use dot_agent_deck::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use dot_agent_deck::daemon_client::issue_command;
use dot_agent_deck::daemon_protocol::{
    AttachRequest, bind_attach_listener, serve_attach_with_counter,
};
use dot_agent_deck::daemon_stop::{StopError, run_daemon_stop};
use dot_agent_deck::event::{
    AgentEvent, AgentType, EventType, ORCHESTRATION_ORPHANED_METADATA_KEY,
};
use dot_agent_deck::platform::ipc::IpcStream;
use dot_agent_deck::state::{AppState, OrchestrationIdentity, SessionState, SessionStatus};
use dot_agent_deck::ui::{CardDensityKind, render_card_to_buffer};
use spec::spec;
use tempfile::TempDir;
use tokio::task::JoinHandle;

/// `bind_attach_listener` flips the process-global umask while binding; the
/// other suites that bind one share a lock for the same reason.
static HARNESS_BIND_LOCK: Mutex<()> = Mutex::new(());

/// The pane id shape `spawn::next_pane_id` mints for role 0 of a daemon-spawned
/// orchestration — the same shape the incident in issue #770 reported
/// (`sched-dispatch-issue-318-319-17-r0`).
const ORCHESTRATOR_PANE: &str = "sched-issue-work-17-r0";
const WORKER_PANE: &str = "sched-issue-work-17-r1";
/// A role pane whose agent has EXITED without the pane being closed — the
/// state that leaves a role-map entry behind with nothing running under it.
const DEPARTED_PANE: &str = "sched-issue-work-17-r2";

/// One hook event as an agent's own hook script would post it: a mid-session
/// `ToolStart`, which is what an orphaned pane keeps emitting for hours.
fn tool_start(pane_id: &str) -> AgentEvent {
    AgentEvent {
        session_id: format!("hook-{pane_id}"),
        agent_type: AgentType::ClaudeCode,
        event_type: EventType::ToolStart,
        tool_name: Some("Read".to_string()),
        tool_detail: Some("src/main.rs".to_string()),
        cwd: None,
        timestamp: Utc::now(),
        user_prompt: None,
        metadata: HashMap::new(),
        pane_id: Some(pane_id.to_string()),
        agent_id: None,
        agent_version: None,
        schema_version: None,
        live_target: None,
    }
}

fn is_marked(event: &AgentEvent) -> bool {
    event.is_orchestration_orphaned()
}

/// A card fixture that differs from the healthy one in exactly one field, so
/// what the assertions attribute to orphaning cannot come from anything else.
fn card(orphaned: bool) -> SessionState {
    let now = Utc::now();
    SessionState {
        session_id: "sess-orphan".to_string(),
        agent_type: AgentType::ClaudeCode,
        cwd: Some("/home/dev/issue-work".to_string()),
        status: SessionStatus::Working,
        active_tool: None,
        started_at: now,
        last_activity: now,
        recent_events: Default::default(),
        tool_count: 3,
        last_user_prompt: Some("dispatch the worker".to_string()),
        first_prompts: vec!["dispatch the worker".to_string()],
        pane_id: Some(ORCHESTRATOR_PANE.to_string()),
        agent_id: Some("1".to_string()),
        display_name: Some("orchestrator".to_string()),
        shell_synthetic_working: false,
        orchestration_orphaned: orphaned,
    }
}

fn buffer_to_text(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area();
    let mut out = String::with_capacity((area.width as usize + 1) * area.height as usize);
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// Scenario: Feed the daemon's event-ingest stamping step a mid-session
/// `ToolStart` from a pane carrying the daemon's own orchestration-role id
/// shape, with no role registered and the registry claiming nothing — the
/// state a restart leaves behind. The event must come out marked, a forged
/// inbound mark must be stripped, and applying the marked event must leave the
/// pane's card flagged orphaned for good.
#[spec("orchestration/orphan/001")]
#[test]
fn orphan_001_daemon_marks_events_from_a_role_pane_it_holds_no_role_for() {
    // The post-restart daemon: no role map, and its registry has never heard of
    // the pane. This is the whole precondition — nothing was torn down, the
    // agent simply outlived the process that knew about it.
    let post_restart = AppState::default();

    let mut event = tool_start(ORCHESTRATOR_PANE);
    post_restart.stamp_orchestration_orphan(&mut event, false);
    assert!(
        is_marked(&event),
        "a role pane the daemon holds no role for is an orphan and must be marked"
    );

    // The spawn race is NOT an orphan. `spawn::spawn` registers each role
    // synchronously as its spawn lands, so a fast agent can post its first hook
    // in the window between the child starting and the registration completing.
    // In that window the registry DOES claim the pane — which is exactly what
    // tells the two cases apart.
    let mut racing = tool_start(ORCHESTRATOR_PANE);
    post_restart.stamp_orchestration_orphan(&mut racing, true);
    assert!(
        !is_marked(&racing),
        "a pane the registry still claims is mid-spawn, not orphaned"
    );

    // A registered role is healthy however the registry answers.
    let mut healthy_state = AppState::default();
    healthy_state.register_orchestration_role(
        ORCHESTRATOR_PANE,
        "orchestrator",
        true,
        OrchestrationIdentity::Instance {
            id: "inst-1".to_string(),
            name: "issue-work".to_string(),
        },
        Some("/home/dev/issue-work"),
    );
    let mut registered = tool_start(ORCHESTRATOR_PANE);
    healthy_state.stamp_orchestration_orphan(&mut registered, false);
    assert!(
        !is_marked(&registered),
        "a pane with a live role registration must never be marked"
    );

    // A pane id that is not the daemon's role shape is not classified either
    // way — a `Ctrl+n` orchestration's panes are numbered by the TUI and carry
    // nothing to recognise, so they are left alone rather than guessed at.
    let mut numbered = tool_start("3");
    post_restart.stamp_orchestration_orphan(&mut numbered, false);
    assert!(
        !is_marked(&numbered),
        "a TUI-numbered pane id carries no role shape and must not be marked"
    );

    // The mark is the daemon's alone. The hook socket is unauthenticated, so a
    // producer must not be able to badge a card the daemon considers healthy.
    let mut forged = tool_start(ORCHESTRATOR_PANE);
    forged.metadata.insert(
        ORCHESTRATION_ORPHANED_METADATA_KEY.to_string(),
        "1".to_string(),
    );
    healthy_state.stamp_orchestration_orphan(&mut forged, false);
    assert!(
        !is_marked(&forged),
        "an inbound mark must be stripped before the daemon decides, not trusted"
    );

    // …and the mark reaches the card. The TUI applies the broadcast event to
    // its own `AppState`, which still knows the pane from before the restart.
    let mut tui = AppState::default();
    tui.register_pane(ORCHESTRATOR_PANE.to_string());
    tui.apply_event(event);
    let session = tui
        .sessions
        .values()
        .find(|s| s.pane_id.as_deref() == Some(ORCHESTRATOR_PANE))
        .expect("the orphaned pane's event must land on a card");
    assert!(
        session.orchestration_orphaned,
        "the card must record the daemon's orphan verdict"
    );

    // Sticky: a later event with no mark must not clear it. There is no
    // un-orphaning edge — a re-dispatch mints a new pane id and a new card —
    // so clearing here would only make the badge flicker.
    tui.apply_event(tool_start(ORCHESTRATOR_PANE));
    let session = tui
        .sessions
        .values()
        .find(|s| s.pane_id.as_deref() == Some(ORCHESTRATOR_PANE))
        .expect("card still present");
    assert!(
        session.orchestration_orphaned,
        "an unmarked follow-up event must not un-orphan the card"
    );
}

/// Scenario: Render one dashboard card whose session is flagged orphaned and
/// one identical card that is not, into an 80-column Normal-density L1 buffer.
/// The orphaned card must carry an `orphaned` marker in its title and a row
/// saying delegation is unavailable; the healthy card must carry neither.
#[spec("orchestration/orphan/002")]
#[test]
fn orphan_002_card_says_orphaned_and_delegation_unavailable() {
    let density = CardDensityKind::Normal;
    let height = density.rendered_height();

    let orphaned = buffer_to_text(&render_card_to_buffer(
        &card(true),
        Some("orchestrator"),
        Some(1),
        density,
        0,
        false,
        80,
        height,
    ));
    assert!(
        orphaned.contains("orphaned"),
        "the title must mark the card orphaned:\n{orphaned}"
    );
    assert!(
        orphaned.contains("Orphaned — delegation unavailable"),
        "the body must say what is actually broken — the pane cannot delegate:\n{orphaned}"
    );

    let healthy = buffer_to_text(&render_card_to_buffer(
        &card(false),
        Some("orchestrator"),
        Some(1),
        density,
        0,
        false,
        80,
        height,
    ));
    assert!(
        !healthy.to_lowercase().contains("orphan"),
        "a card with a live role registration must show nothing about orphaning:\n{healthy}"
    );
}

/// Scenario: Serve a real attach socket over an `AppState` holding two
/// orchestration roles whose panes have live stand-in agents, plus a THIRD
/// whose agent has exited, then ask for `list-agents` and run the real
/// `daemon stop` flow against that socket with no `--force`. The reply must
/// report the two live roles and not the dead one, and the stop must refuse,
/// naming them and saying the loss is permanent.
#[spec("orchestration/orphan/003")]
#[test]
fn orphan_003_daemon_stop_refuses_while_orchestration_roles_are_live() {
    child_lifetime_bound::arm();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(orphan_003_inner());
}

async fn orphan_003_inner() {
    let registry = Arc::new(AgentPtyRegistry::new());
    // Two live role panes, and a third whose agent exits immediately. The
    // third is the Greptile-review case: nothing but a pane CLOSE calls
    // `unregister_pane`, so a role agent that simply exits leaves its map
    // entry behind — and reporting THAT as live wedged `daemon stop` for the
    // rest of the daemon's life, with `--force` as the only way out.
    for pane in [ORCHESTRATOR_PANE, WORKER_PANE] {
        registry
            .spawn_agent(SpawnOptions {
                command: Some("sleep 30"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
                ..SpawnOptions::default()
            })
            .expect("stand-in role agent should spawn");
    }
    registry
        .spawn_agent(SpawnOptions {
            command: Some("true"),
            env: vec![(
                DOT_AGENT_DECK_PANE_ID.to_string(),
                DEPARTED_PANE.to_string(),
            )],
            ..SpawnOptions::default()
        })
        .expect("stand-in departed agent should spawn");

    let mut state = AppState::default();
    let identity = OrchestrationIdentity::Instance {
        id: "inst-1".to_string(),
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
        identity.clone(),
        Some("/home/dev/issue-work"),
    );
    state.register_orchestration_role(
        DEPARTED_PANE,
        "reviewer",
        false,
        identity,
        Some("/home/dev/issue-work"),
    );
    let shared = Arc::new(tokio::sync::RwLock::new(state));

    // The registry marks an agent exited from its own reaper, so the `true`
    // stand-in's departure is observable but not synchronous with the spawn.
    let departed_gone = wait_until(Duration::from_secs(10), || {
        !registry.has_live_pane(DEPARTED_PANE)
    })
    .await;
    assert!(
        departed_gone,
        "the `true` stand-in must exit so the registry stops claiming its pane"
    );

    let server = start_server(registry.clone(), shared).await;

    // The wire half: the roles must actually reach a client. Nothing else in
    // the response changes, so an older client reading only `agents` is
    // unaffected.
    let stream = IpcStream::connect(&server.path).await.expect("connect");
    let (mut rd, mut wr) = stream.into_split();
    let resp = issue_command(&mut rd, &mut wr, &AttachRequest::ListAgents)
        .await
        .expect("list-agents");
    drop(rd);
    drop(wr);
    assert!(resp.ok, "list-agents failed: {:?}", resp.error);
    let roles = resp
        .orchestration_roles
        .expect("a daemon holding roles must report them on list-agents");
    let named: Vec<(String, String, bool)> = roles
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
        "the two LIVE roles must be reported, orchestrator flagged, order \
         stable — and the role whose agent exited must NOT appear, or an \
         ordinary `daemon stop` would be refused forever over a dead pane"
    );

    // The policy half, driven through the real `daemon stop` entry point so the
    // whole chain is covered: socket → list-agents → decode → refusal.
    //
    // NOTE for whoever regresses this: the refusal is what STOPS this flow
    // before `terminate_daemon_graceful`, and the pid on the other end of this
    // socket is the test process itself. If the guard goes away, this test does
    // not merely fail its assertion — it gets SIGTERMed. That is a loud red,
    // not a flake.
    let err = run_daemon_stop(&server.path, false)
        .await
        .expect_err("a daemon holding live orchestration roles must refuse to stop");
    let StopError::LiveOrchestrations { roles } = err else {
        panic!("expected LiveOrchestrations, got {err:?}");
    };
    assert_eq!(
        roles.len(),
        2,
        "the refusal must carry both live roles and neither the dead one: {roles:?}"
    );
    let msg = dot_agent_deck::daemon_stop::format_live_orchestrations_refusal(&roles);
    assert!(
        msg.contains(ORCHESTRATOR_PANE) && msg.contains("can never delegate again"),
        "the refusal must name the panes and say the loss is permanent: {msg:?}"
    );
}

/// Scenario: Register ONE orchestration role whose stand-in agent exits on its
/// own and never close its pane, then serve a real attach socket and ask for
/// `list-agents`. The role-map entry must still be there while the reply
/// reports no live roles and no agents, so the policy `daemon stop` applies to
/// that reply is "proceed" rather than a refusal over a pane that is a corpse.
#[spec("orchestration/orphan/004")]
#[test]
fn orphan_004_a_role_whose_agent_exited_on_its_own_does_not_block_daemon_stop() {
    child_lifetime_bound::arm();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(orphan_004_inner());
}

async fn orphan_004_inner() {
    // The Greptile P1 case in isolation: ONE role, its agent gone, its pane
    // never closed, and nothing else in the daemon. `orphan/003` proves the
    // dead role is dropped from a report that still has live roles in it; this
    // proves the empty report that follows is not itself a refusal — which is
    // the half an operator actually feels, because the pre-fix behaviour wedged
    // `daemon stop` and `daemon restart` for the life of the daemon with
    // `--force` as the only way out.
    let registry = Arc::new(AgentPtyRegistry::new());
    registry
        .spawn_agent(SpawnOptions {
            command: Some("true"),
            env: vec![(
                DOT_AGENT_DECK_PANE_ID.to_string(),
                DEPARTED_PANE.to_string(),
            )],
            ..SpawnOptions::default()
        })
        .expect("stand-in departed agent should spawn");

    let mut state = AppState::default();
    state.register_orchestration_role(
        DEPARTED_PANE,
        "reviewer",
        false,
        OrchestrationIdentity::Instance {
            id: "inst-1".to_string(),
            name: "issue-work".to_string(),
        },
        Some("/home/dev/issue-work"),
    );

    // Nothing here calls `unregister_pane`, and nothing on the PTY-EOF path
    // does either — its only callers are the `StopAgent` handler, the TUI's
    // close paths and `spawn`'s partial-orchestration rollback. So the agent
    // dying is observable ONLY through the registry: `pump_reader` sets the
    // per-agent `exited` flag when the PTY read loop ends, which is what makes
    // `has_live_pane` go false for a child that was never explicitly closed.
    let departed_gone = wait_until(Duration::from_secs(10), || {
        !registry.has_live_pane(DEPARTED_PANE)
    })
    .await;
    assert!(
        departed_gone,
        "the `true` stand-in must exit on its own, with no pane close, so the \
         registry stops claiming its pane"
    );
    assert!(
        state.pane_role_map.contains_key(DEPARTED_PANE),
        "the role-map entry must OUTLIVE its agent — if it were cleaned up here \
         this test would pass for the wrong reason, and the filter under review \
         would be doing nothing"
    );
    assert!(
        state.live_orchestration_roles(&registry).is_empty(),
        "a role whose agent has exited must not be reported as live"
    );

    let shared = Arc::new(tokio::sync::RwLock::new(state));
    let server = start_server(registry.clone(), shared).await;
    let stream = IpcStream::connect(&server.path).await.expect("connect");
    let (mut rd, mut wr) = stream.into_split();
    let resp = issue_command(&mut rd, &mut wr, &AttachRequest::ListAgents)
        .await
        .expect("list-agents");
    drop(rd);
    drop(wr);
    assert!(resp.ok, "list-agents failed: {:?}", resp.error);
    assert_eq!(
        resp.orchestration_roles.as_deref(),
        Some(&[][..]),
        "a daemon holding only a DEAD role must answer `Some(vec![])` — it holds \
         no live roles — and not `None`, which `daemon stop` reads as \"this \
         daemon cannot answer\""
    );

    // The policy over the real reply, derived exactly as `run_daemon_stop`
    // derives it. Deliberately NOT through `run_daemon_stop` itself: with no
    // refusal to stop it, that call reaches `terminate_daemon_graceful`, whose
    // target pid is this test process (see `orphan/003`'s note). Asserting the
    // decision instead of the SIGTERM is the whole reason `stop_refusal` is a
    // separate pure function.
    let agent_ids: Vec<String> = resp
        .agent_records
        .map(|rs| rs.into_iter().map(|r| r.id).collect::<Vec<_>>())
        .or(resp.agents)
        .unwrap_or_default();
    assert!(
        agent_ids.is_empty(),
        "the exited stand-in must be gone from `agent_records` too, or the \
         PRE-EXISTING managed-agent guard would refuse the stop and this would \
         prove nothing about the new one: {agent_ids:?}"
    );
    let roles = resp.orchestration_roles.unwrap_or_default();
    assert!(
        dot_agent_deck::daemon_stop::stop_refusal(&roles, &agent_ids, false).is_none(),
        "with only a dead role left, an ordinary `daemon stop` must PROCEED — \
         refusing here is the wedge the Greptile P1 named"
    );
}

struct Server {
    _dir: TempDir,
    path: PathBuf,
    handle: JoinHandle<()>,
    registry: Arc<AgentPtyRegistry>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
        // The stand-in role agents are real child processes; reap them rather
        // than leaving a `sleep 30` behind for the harness to trip over.
        self.registry.shutdown_all();
    }
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

/// Serve the production `ListAgents` handler over a real attach socket, backed
/// by the caller's `AppState` — the same shape `tests/rehydration.rs` uses, and
/// the only way to exercise the handler's own read of the role maps.
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
