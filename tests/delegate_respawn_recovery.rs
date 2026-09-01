//! What a `clear = true` delegate does when its replacement worker cannot be
//! produced, or dies before it is ready — issues #584 and #606.
//!
//! Both issues land on the same seam: `dispatch_one_owned` destroys the current
//! worker (`respawn_agent_for_pane`) BEFORE it has anywhere to deliver the task
//! pointer, and then treats "no live worker at delivery time" as a terminal,
//! silent drop. The orchestrator's `delegate` has already exited 0, so it waits
//! for a `work-done` that can never arrive and nothing anywhere says why.
//!
//! * **#606** — a `StopAgent` for the worker pane is in flight, so the pane has
//!   no registry entry when the respawn looks for one. It failed with `NotFound`
//!   and the role was gone for the rest of the session.
//! * **#584** — the respawn succeeded but the replacement child died before it
//!   ever announced itself, so the identity gate refused the pointer with
//!   `NoLiveTarget` after the full readiness wait, logging one `warn!` and
//!   nothing else.
//!
//! The third test here is #584's CONTROL rather than a defect: it drives one
//! `clear = true` respawn through the daemon's dispatch spawn primitive
//! (`crate::spawn::spawn`) and another through the TUI's `StartAgent` shape, and
//! compares what the two replacements were actually launched with. #584's
//! leading hypothesis was that those two paths preserve different relaunch
//! parameters; the issue asked for that to be reproduced before it was believed,
//! and this is what does the asking.
//!
//! No LLM: the workers are `cat` / small shell stand-ins, because what is under
//! test is a daemon-side lifecycle race, a delivery decision, and a comparison
//! of launch parameters — not an agent's behaviour. The real-agent half of #584
//! lives on the dispatch path's `orchestration/dispatch/002`, whose `coder` role
//! is `clear = true` precisely so a REAL worker drives this same respawn.

#![cfg(unix)]

use std::time::Duration;

use dot_agent_deck::agent_pty::{
    AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions, TabMembership,
};
use dot_agent_deck::event::{AgentEvent, AgentType, DelegateSignal, EventType};
use dot_agent_deck::state::OrchestrationIdentity;
use spec::spec;

mod common;

const ORCH_PANE: &str = "recovery-orchestrator";
const WORKER_PANE: &str = "recovery-coder";
const WORKER_ROLE: &str = "coder";
const ORCHESTRATION: &str = "recovery-orchestration";
const ORCHESTRATION_ID: &str = "recovery-instance-1";
const POINTER: &[u8] = b"Read .dot-agent-deck/worker-task-coder.md for your task.";

/// Issue #709: what the SIGTERM-ignoring stand-in prints once — and only once —
/// its `trap '' TERM` is installed, so `delegate/022` can wait for the state its
/// scenario depends on instead of guessing at how long a `sh` takes to boot.
const STUBBORN_WORKER_ARMED: &[u8] = b"STUBBORN-WORKER-ARMED";

fn config(worker_command: &str) -> String {
    format!(
        "[[orchestrations]]\nname = \"{ORCHESTRATION}\"\n\n\
         [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n\n\
         [[orchestrations.roles]]\nname = \"{WORKER_ROLE}\"\ncommand = \"{worker_command}\"\nclear = true\n"
    )
}

fn membership(role_index: usize, role_name: &str, is_start_role: bool, cwd: &str) -> TabMembership {
    TabMembership::Orchestration {
        name: ORCHESTRATION.to_string(),
        role_index,
        role_name: role_name.to_string(),
        is_start_role,
        orchestration_cwd: Some(cwd.to_string()),
        display_title: None,
        orchestration_id: Some(ORCHESTRATION_ID.to_string()),
    }
}

fn snapshot_contains(snapshot: &[u8], needle: &[u8]) -> bool {
    snapshot.windows(needle.len()).any(|w| w == needle)
}

async fn wait_for_pane_needle(
    registry: &AgentPtyRegistry,
    pane_id: &str,
    needle: &[u8],
    timeout: Duration,
) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let snapshot = registry
            .pane_current_agent_id(pane_id)
            .and_then(|id| registry.snapshot(&id).ok())
            .unwrap_or_default();
        if snapshot_contains(&snapshot, needle) || tokio::time::Instant::now() >= deadline {
            return snapshot;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Issue #709: wait until `pane_id`'s close has OBSERVABLY entered its grace
/// window, so a delegate aimed at that window lands inside it rather than at a
/// guessed offset from when the close was asked for.
///
/// Bounded by [`common::child_boot_budget`] for the same reason the boot waits
/// are: the quantity being waited on is a freshly scheduled task getting its
/// turn, so the ceiling has to follow how contended the machine is. It returns
/// the instant the window opens, so an idle box pays nothing for the headroom.
async fn wait_for_close_in_flight<T>(
    registry: &AgentPtyRegistry,
    pane_id: &str,
    request: &tokio::task::JoinHandle<T>,
) -> bool {
    let deadline = tokio::time::Instant::now() + common::child_boot_budget();
    loop {
        if registry.pane_close_in_flight(pane_id) {
            return true;
        }
        // The `StopAgent` request returns only once the close has run to
        // completion (measured: `is_finished` flips in the same 100 ms tick that
        // `pane_close_in_flight` goes back to false), so a finished request with
        // no window ever observed means there is nothing left to wait for — the
        // request failed, or the pane was never closed at all. Ending here turns
        // that into a prompt, legible failure instead of one that spends the
        // whole budget and then reports the wrong cause.
        if request.is_finished() {
            return false;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The pane's live agent id, once it is one this test has not seen before.
async fn wait_for_replacement_agent(
    registry: &AgentPtyRegistry,
    pane_id: &str,
    old_agent_id: &str,
    timeout: Duration,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(id) = registry.pane_current_agent_id(pane_id)
            && id != old_agent_id
        {
            return Some(id);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn session_start(pane_id: &str, agent_id: &str) -> String {
    let event = AgentEvent {
        session_id: format!("session-{agent_id}"),
        agent_type: AgentType::None,
        event_type: EventType::SessionStart,
        tool_name: None,
        tool_detail: None,
        cwd: None,
        timestamp: chrono::Utc::now(),
        user_prompt: None,
        metadata: std::collections::HashMap::new(),
        pane_id: Some(pane_id.to_string()),
        agent_id: Some(agent_id.to_string()),
        agent_version: None,
        schema_version: None,
        live_target: None,
    };
    serde_json::to_string(&event).expect("serialize synthetic SessionStart")
}

struct Fixture {
    daemon: common::InProcDaemon,
    _dir: tempfile::TempDir,
    cwd: String,
    orchestrator_agent_id: String,
    worker_agent_id: String,
}

async fn fixture(worker_command_in_dir: impl FnOnce(&std::path::Path) -> String) -> Fixture {
    let daemon = common::spawn_inprocess_daemon().await;
    let dir = common::race_safe_tempdir();
    let worker_command = worker_command_in_dir(dir.path());
    std::fs::write(
        dir.path().join(".dot-agent-deck.toml"),
        config(&worker_command),
    )
    .expect("write orchestration config");
    let cwd = dir.path().to_string_lossy().into_owned();

    let orchestrator_agent_id = daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(&cwd),
            display_name: Some("orchestrator"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), ORCH_PANE.to_string())],
            tab_membership: Some(membership(0, "orchestrator", true, &cwd)),
            ..SpawnOptions::default()
        })
        .expect("spawn orchestrator stand-in");
    let worker_agent_id = daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some(&worker_command),
            cwd: Some(&cwd),
            display_name: Some(WORKER_ROLE),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string())],
            tab_membership: Some(membership(1, WORKER_ROLE, false, &cwd)),
            ..SpawnOptions::default()
        })
        .expect("spawn worker stand-in");

    {
        let mut state = daemon.state.write().await;
        let identity = OrchestrationIdentity::Instance {
            id: ORCHESTRATION_ID.to_string(),
            name: ORCHESTRATION.to_string(),
        };
        state.register_orchestration_role(
            ORCH_PANE,
            "orchestrator",
            true,
            identity.clone(),
            Some(&cwd),
        );
        state.register_orchestration_role(WORKER_PANE, WORKER_ROLE, false, identity, Some(&cwd));
    }

    Fixture {
        daemon,
        _dir: dir,
        cwd,
        orchestrator_agent_id,
        worker_agent_id,
    }
}

async fn delegate(fx: &Fixture, task: &str) {
    let signal = DelegateSignal {
        pane_id: ORCH_PANE.to_string(),
        task: task.to_string(),
        to: vec![WORKER_ROLE.to_string()],
        timestamp: chrono::Utc::now(),
    };
    fx.daemon
        .state
        .read()
        .await
        .handle_delegate_with_state(
            signal,
            &fx.daemon.registry,
            &fx.daemon.event_tx,
            Some(&fx.daemon.state),
        )
        .await;
}

/// Scenario: start an orchestration whose `coder` role is `clear = true`, wait
/// for its SIGTERM-ignoring stand-in to print the marker that proves its
/// `trap '' TERM` is installed, close the worker's pane through the daemon's
/// real `StopAgent` path, and — as soon as that close is observably in flight,
/// still spending its termination grace — delegate to `coder`. The worker role must come back:
/// a live agent on its pane that physically receives the task pointer, and a
/// role registration that still routes the NEXT delegate.
#[tokio::test(flavor = "multi_thread")]
#[spec("orchestration/delegate/022")]
async fn delegate_022_delegate_during_an_in_flight_close_brings_the_role_back() {
    use std::os::unix::fs::PermissionsExt;

    // The stand-in IGNORES SIGTERM, so `close_agent` spends its full
    // `AGENT_TERMINATE_GRACE` before the child is reaped — which is the window
    // #606 is about. A plain `cat` dies on the first signal and the whole close
    // is over in well under the 200 ms the reporter measured, so it cannot
    // reproduce the race at all. `exec` keeps the ignore disposition (it is
    // inherited across `execve`) while still giving the pane something that
    // echoes what the daemon writes into it.
    let fx = fixture(|dir| {
        let script = dir.join("stubborn-worker.sh");
        // Issue #709: the marker is printed AFTER the trap and BEFORE the exec,
        // so seeing it is proof the disposition is already `SIG_IGN` — the one
        // fact this scenario cannot proceed without. `exec` carries it across
        // `execve`, so it still holds for the `cat` that replaces the shell.
        let marker = String::from_utf8_lossy(STUBBORN_WORKER_ARMED).into_owned();
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntrap '' TERM\nprintf '{marker}'\nexec cat\n"),
        )
        .expect("write SIGTERM-ignoring worker stand-in");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod SIGTERM-ignoring worker stand-in");
        script.to_string_lossy().into_owned()
    })
    .await;
    let client = dot_agent_deck::daemon_client::DaemonClient::new(fx.daemon.attach_path.clone());

    // Issue #709: this was a flat 400 ms sleep, and it was the load-sensitive
    // seam of the whole test. What the scenario needs is not "400 ms have
    // passed" but "the stand-in has installed its `trap '' TERM`" — and on a
    // loaded box a freshly forked `sh` has not necessarily run its first line
    // inside 400 ms. When it has not, the `StopAgent` below kills it on the
    // first signal, the close is over in milliseconds, and the test fails at the
    // in-flight precondition further down: a starvation failure wearing the
    // costume of a #606 regression. Waiting for the marker asserts the fact
    // directly, and cannot pass before it is true.
    let armed = common::wait_for_child_first_output(
        &fx.daemon.registry,
        &fx.worker_agent_id,
        STUBBORN_WORKER_ARMED,
    )
    .await;
    assert!(
        snapshot_contains(&armed, STUBBORN_WORKER_ARMED),
        "precondition: the worker stand-in never got as far as installing its `trap '' TERM`, so \
         the close below would be an ordinary fast termination rather than the grace-period \
         window #606 is about; snapshot = {:?}",
        String::from_utf8_lossy(&armed)
    );
    assert_eq!(
        fx.daemon
            .registry
            .pane_current_agent_id(WORKER_PANE)
            .as_deref(),
        Some(fx.worker_agent_id.as_str()),
        "precondition: the worker stand-in must still own its pane before the close"
    );
    // Issue #709: the assertion above says "still alive" but only ever checked
    // REGISTRATION, and the difference is the whole scenario. `close_agent`
    // spends `AGENT_TERMINATE_GRACE` only while the child is still running —
    // against an already-dead one it returns at once, the close transition opens
    // and shuts inside a few milliseconds, and the grace window this test needs
    // to deliver into never observably exists. That reads downstream as "the
    // close was not in flight", which is true and useless. Check the fact the
    // sentence always meant.
    assert!(
        fx.daemon.registry.agent_is_live(&fx.worker_agent_id),
        "precondition: the worker stand-in is registered but no longer running, so the close \
         below would finish instantly instead of spending its termination grace"
    );

    let closing_id = fx.worker_agent_id.clone();
    let closing = tokio::spawn(async move { client.stop_agent(&closing_id).await });

    // Issue #709: this was a flat `sleep(200 ms)` — the reporter's own interval
    // — followed by the assertion below, and it was the SECOND fixed deadline in
    // this test. The 200 ms was standing in for "the close has entered its grace
    // window", but `stop_agent` is driven by a spawned task and a socket round
    // trip, so on a loaded box neither had necessarily reached
    // `begin_pane_close` yet when the sleep expired. The assertion then fired
    // saying the close was not in flight — and it was right, for the opposite of
    // the reason it names: not "the close already finished" but "the close had
    // not started". Measured on this branch at load 78 on 16 cores, in a
    // full-tier run whose whole failing case took 0.98 s, so nothing had
    // overshot anything.
    //
    // `pane_close_in_flight` is exactly the state the 200 ms was approximating —
    // it is true from the moment the cleanup hold and the closing mark go up,
    // which is also the moment the pane's registry entry is gone and its role is
    // still registered: the window a `clear = true` respawn used to fail
    // `NotFound` in. Waiting for it puts the delegate inside that window by
    // construction rather than by arithmetic, and it cannot report a window that
    // has not opened as one that has closed.
    let entered_grace = wait_for_close_in_flight(&fx.daemon.registry, WORKER_PANE, &closing).await;
    assert!(
        entered_grace,
        "precondition: the close never entered its grace window, so the delegate below would be \
         an ordinary post-close delegate instead of #606's race; stop_agent finished = {}, \
         stand-in still live = {}, records = {:?}",
        closing.is_finished(),
        fx.daemon.registry.agent_is_live(&fx.worker_agent_id),
        fx.daemon.registry.agent_records()
    );
    delegate(&fx, "list the files in this directory").await;

    let replacement = wait_for_replacement_agent(
        &fx.daemon.registry,
        WORKER_PANE,
        &fx.worker_agent_id,
        Duration::from_secs(20),
    )
    .await
    .unwrap_or_else(|| {
        panic!(
            "delegating to a `clear = true` role while its pane was mid-close left the role with \
             no live agent at all — the pane is dead for the rest of the session (#606). \
             records = {:?}",
            fx.daemon.registry.agent_records()
        )
    });

    // A `cat` stand-in emits no readiness signal of its own, so stand in for the
    // agent's hook exactly as the rest of the fast delegate suite does.
    common::write_hook_line(
        &fx.daemon.hook_path,
        &session_start(WORKER_PANE, &replacement),
    )
    .expect("deliver synthetic SessionStart for the replacement worker");

    let snapshot = wait_for_pane_needle(
        &fx.daemon.registry,
        WORKER_PANE,
        POINTER,
        Duration::from_secs(20),
    )
    .await;
    assert!(
        snapshot_contains(&snapshot, POINTER),
        "the recovered worker never received the task pointer; snapshot = {:?}",
        String::from_utf8_lossy(&snapshot)
    );

    let _ = closing.await;

    let state = fx.daemon.state.read().await;
    assert_eq!(
        state.pane_role_map.get(WORKER_PANE).map(String::as_str),
        Some(WORKER_ROLE),
        "the role must still route after the recovery, or the NEXT delegate is rejected with \
         `reached no worker for role(s)` — the permanent breakage #606 reports"
    );
}

/// Issue #584's promptness half: how long the orchestrator may be left in the
/// dark after its `clear = true` replacement worker dies before it is ready.
///
/// **Re-derived in issue #243, from a measurement rather than from the
/// alternative.** It was 20 s, justified in the catalog as "well under the
/// production `SESSION_START_WAIT_TIMEOUT` + readiness buffer (31 s) that the
/// pre-fix path burned" — a bound picked to be under the thing it was replacing.
/// That reasoning has expired twice over: #584 itself ended the readiness wait
/// on the replacement's PTY reaching EOF, and #243 removed the dead wait for
/// declared-no-signal agents outright, so 31 s is nobody's behaviour any more and
/// a 20 s ceiling on a ~0.1 s operation asserts approximately nothing.
///
/// **Measured on this branch: 103.1 / 103.4 / 103.9 / 104.1 ms idle, and
/// 54.4-108.4 ms across eight runs with all 16 cores saturated and a concurrent
/// full fast tier.** The figure is dominated by the fixture's own 50 ms poll
/// interval and barely moves under load, because the notice is driven by the
/// child's exit rather than by any timer.
///
/// Five seconds is ~46x the slowest figure measured here — room for a CI runner
/// an order of magnitude slower than this box and then some — while staying 6x
/// under the 30 s `SESSION_START_WAIT_TIMEOUT` a reverted EOF-driven wait would
/// cost. It is deliberately not tighter: this is an upper bound on a fast event,
/// so unlike `orchestration/delegate/010`'s lower bound it IS the load-sensitive
/// direction, and headroom is the only mitigation available.
const DEAD_REPLACEMENT_NOTICE_BUDGET: Duration = Duration::from_secs(5);

/// Scenario: start an orchestration whose `clear = true` worker refuses to start
/// while a marker file sits beside it, drop that marker once the first worker is
/// confirmed up, then delegate. The replacement dies before it can announce
/// itself, and the orchestrator must be TOLD — in its own pane, and within five
/// seconds rather than the thirty a readiness wait would cost — instead of being
/// left to wait for a `work-done` that can never arrive.
#[tokio::test(flavor = "multi_thread")]
#[spec("orchestration/delegate/023")]
async fn delegate_023_a_replacement_that_dies_is_reported_to_the_orchestrator() {
    use std::os::unix::fs::PermissionsExt;

    // The stand-in refuses to start once a `die` marker exists beside it. The
    // TEST drops that marker, after confirming the first worker is up — so
    // "the replacement dies before it is ready" is a fact the test establishes,
    // not a race it hopes for.
    let fx = fixture(|dir| {
        let script = dir.join("one-shot-worker.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nif [ -e \"$(dirname \"$0\")/die\" ]; then exit 3; fi\nexec cat\n",
        )
        .expect("write one-shot worker stand-in");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod one-shot worker stand-in");
        script.to_string_lossy().into_owned()
    })
    .await;

    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        fx.daemon
            .registry
            .pane_current_agent_id(WORKER_PANE)
            .as_deref(),
        Some(fx.worker_agent_id.as_str()),
        "precondition: the first worker must be up before we make the next one fail"
    );
    std::fs::write(std::path::Path::new(&fx.cwd).join("die"), "")
        .expect("arm the stand-in's refusal to start again");

    delegate(&fx, "list the files in this directory").await;

    // The user's altitude: something visible in the orchestrator's own pane.
    let deadline = tokio::time::Instant::now() + DEAD_REPLACEMENT_NOTICE_BUDGET;
    let mut snapshot;
    loop {
        snapshot = fx
            .daemon
            .registry
            .snapshot(&fx.orchestrator_agent_id)
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&snapshot);
        if text.contains("delegated worker never came up") && text.contains(WORKER_PANE) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "a `clear = true` delegate whose replacement worker died reported nothing to the \
             orchestrator within {DEAD_REPLACEMENT_NOTICE_BUDGET:?} — either the notice is gone \
             entirely, or the readiness wait no longer ends on the replacement's EOF and the \
             orchestrator is sitting through it (#584; budget re-derived in #243 against a \
             measured ~0.1 s). orchestrator pane = {:?}",
            text
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let worker_snapshot = fx
        .daemon
        .registry
        .pane_current_agent_id(WORKER_PANE)
        .and_then(|id| fx.daemon.registry.snapshot(&id).ok())
        .unwrap_or_default();
    assert!(
        !snapshot_contains(&worker_snapshot, POINTER),
        "nothing may be written into a pane whose agent is not live"
    );
    // PRD #249 finding B3's precedent: this notice family interpolates the
    // worker's scrubbed pane id and nothing else, so the role name — which is
    // caller-supplied config text — must not appear.
    assert!(
        !String::from_utf8_lossy(&snapshot).contains("'coder'"),
        "the notice must not interpolate the role name; snapshot = {:?}",
        String::from_utf8_lossy(&snapshot)
    );
}

// ---------------------------------------------------------------------------
// #584's control: the two spawn paths' respawns, side by side.
// ---------------------------------------------------------------------------

/// A recorder worker: appends everything that decides HOW it was launched to a
/// log, then behaves like a `cat` pane so the delegate's pointer is observable.
#[cfg(unix)]
fn write_recorder(path: &std::path::Path, log: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    let script = format!(
        "#!/bin/sh\n\
         {{\n\
         echo \"argv0=$0 args=$*\"\n\
         echo \"cwd=$(pwd)\"\n\
         echo \"pane=$DOT_AGENT_DECK_PANE_ID\"\n\
         echo \"sock=$DOT_AGENT_DECK_SOCKET\"\n\
         echo \"shell=$SHELL\"\n\
         echo \"---\"\n\
         }} >> \"{log}\"\n\
         exec cat\n",
        log = log.display()
    );
    std::fs::write(path, script).expect("write recorder worker stand-in");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod recorder worker stand-in");
}

/// The recorder's per-invocation blocks, minus the agent id (which is expected
/// to differ — it is the whole point of a respawn).
fn recorded_launches(log: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .split("---\n")
        .map(str::trim)
        .filter(|block| !block.is_empty())
        .map(str::to_string)
        .collect()
}

struct SilentNotifier;
impl dot_agent_deck::scheduler::Notifier for SilentNotifier {
    fn notify(&self, event: dot_agent_deck::scheduler::NotifyEvent) {
        // Surfaced only on failure, via the assertions below; a spawn error here
        // would otherwise be invisible.
        eprintln!("[spawn notifier] {event:?}");
    }
}

/// Scenario: bring the same `clear = true` orchestration up twice — once through
/// the daemon's own dispatch spawn primitive (what `dot-agent-deck dispatch`,
/// the scheduler and issue-dispatch all use) and once through the `StartAgent`
/// shape the TUI's Ctrl+N path uses — then delegate to the worker on each. Both
/// replacements must be launched with the same command, cwd, pane id, hook
/// socket and shell, and both workers must physically receive the task pointer.
#[tokio::test(flavor = "multi_thread")]
#[spec("orchestration/dispatch/003")]
async fn dispatch_003_the_dispatch_and_startagent_paths_respawn_identically() {
    let daemon = common::spawn_inprocess_daemon().await;

    // --- the DISPATCH path: `crate::spawn::spawn`, exactly as the daemon's
    // `dispatch` / scheduler / issue-dispatch producers call it.
    let dispatch_dir = common::race_safe_tempdir();
    let dispatch_log = dispatch_dir.path().join("launches.log");
    let dispatch_recorder = dispatch_dir.path().join("recorder.sh");
    write_recorder(&dispatch_recorder, &dispatch_log);
    std::fs::write(
        dispatch_dir.path().join(".dot-agent-deck.toml"),
        config(&dispatch_recorder.to_string_lossy()),
    )
    .expect("write dispatched orchestration config");

    let handle = dot_agent_deck::spawn::spawn(
        dot_agent_deck::spawn::SpawnRequest {
            task_name: "parity".to_string(),
            working_dir: dispatch_dir.path().to_string_lossy().into_owned(),
            command: None,
            prompt: "coordinate the team".to_string(),
            resolved_target: None,
            compose_orchestrator_context: true,
        },
        &daemon.registry,
        &SilentNotifier,
        Some(&daemon.event_tx),
        true,
        Some(&daemon.state),
    )
    .await
    .expect("the dispatch spawn primitive must bring the orchestration up");
    let dispatched_orchestrator = handle
        .agents
        .iter()
        .find(|a| a.role_name.as_deref() == Some("orchestrator"))
        .expect("dispatched orchestration has an orchestrator pane")
        .pane_id
        .clone();
    let dispatched_worker = handle
        .agents
        .iter()
        .find(|a| a.role_name.as_deref() == Some(WORKER_ROLE))
        .expect("dispatched orchestration has a worker pane")
        .pane_id
        .clone();
    let dispatched_worker_agent = daemon
        .registry
        .pane_current_agent_id(&dispatched_worker)
        .expect("the dispatched worker pane has a live agent");

    // --- the StartAgent path: the shape `AttachRequest::StartAgent` builds.
    let control = fixture(|dir| {
        let recorder = dir.join("recorder.sh");
        write_recorder(&recorder, &dir.join("launches.log"));
        recorder.to_string_lossy().into_owned()
    })
    .await;
    let control_log = std::path::Path::new(&control.cwd).join("launches.log");

    // Both first invocations must be on disk before either respawn, or the
    // comparison below cannot tell a respawn's block from an initial one.
    for (label, log) in [
        ("dispatch", dispatch_log.as_path()),
        ("startagent", control_log.as_path()),
    ] {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while recorded_launches(log).is_empty() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the {label} path's worker never recorded an initial launch"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    // --- delegate on each path and let each replacement announce itself, so
    // neither side pays the production readiness fallback.
    let signal = DelegateSignal {
        pane_id: dispatched_orchestrator,
        task: "list the files in this directory".to_string(),
        to: vec![WORKER_ROLE.to_string()],
        timestamp: chrono::Utc::now(),
    };
    daemon
        .state
        .read()
        .await
        .handle_delegate_with_state(
            signal,
            &daemon.registry,
            &daemon.event_tx,
            Some(&daemon.state),
        )
        .await;
    let dispatched_replacement = wait_for_replacement_agent(
        &daemon.registry,
        &dispatched_worker,
        &dispatched_worker_agent,
        Duration::from_secs(20),
    )
    .await
    .expect("the dispatched worker must be replaced by the clear=true delegate");
    common::write_hook_line(
        &daemon.hook_path,
        &session_start(&dispatched_worker, &dispatched_replacement),
    )
    .expect("deliver the dispatched replacement's SessionStart");

    delegate(&control, "list the files in this directory").await;
    let control_replacement = wait_for_replacement_agent(
        &control.daemon.registry,
        WORKER_PANE,
        &control.worker_agent_id,
        Duration::from_secs(20),
    )
    .await
    .expect("the StartAgent-path worker must be replaced by the clear=true delegate");
    common::write_hook_line(
        &control.daemon.hook_path,
        &session_start(WORKER_PANE, &control_replacement),
    )
    .expect("deliver the control replacement's SessionStart");

    // --- both workers actually receive the pointer.
    let dispatched_snapshot = wait_for_pane_needle(
        &daemon.registry,
        &dispatched_worker,
        POINTER,
        Duration::from_secs(20),
    )
    .await;
    assert!(
        snapshot_contains(&dispatched_snapshot, POINTER),
        "the DISPATCHED orchestration's respawned worker never received the task pointer — the \
         user-visible half of #584; snapshot = {:?}",
        String::from_utf8_lossy(&dispatched_snapshot)
    );
    let control_snapshot = wait_for_pane_needle(
        &control.daemon.registry,
        WORKER_PANE,
        POINTER,
        Duration::from_secs(20),
    )
    .await;
    assert!(
        snapshot_contains(&control_snapshot, POINTER),
        "the CONTROL orchestration's respawned worker never received the task pointer — a broken \
         control means the harness is wrong and the dispatched result above proves nothing; \
         snapshot = {:?}",
        String::from_utf8_lossy(&control_snapshot)
    );

    // --- and the two paths' relaunch parameters agree.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while recorded_launches(&dispatch_log).len() < 2 || recorded_launches(&control_log).len() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "one of the two paths never recorded a SECOND launch: dispatch = {:?}, control = {:?}",
            recorded_launches(&dispatch_log),
            recorded_launches(&control_log)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let dispatch_launches = recorded_launches(&dispatch_log);
    let control_launches = recorded_launches(&control_log);

    // Within a path, the respawn must reproduce the initial launch. Everything
    // that is legitimately per-pane (the recorder's own path, the pane id, the
    // cwd) is normalised away so the two paths can then be compared to each
    // other as well.
    let normalise = |block: &str, dir: &str, pane: &str| {
        block
            .replace(dir, "<CWD>")
            .replace(pane, "<PANE>")
            .replace(&daemon.hook_path.to_string_lossy().into_owned(), "<SOCK>")
            .replace(
                &control.daemon.hook_path.to_string_lossy().into_owned(),
                "<SOCK>",
            )
    };
    let dispatch_dir_str = dispatch_dir.path().to_string_lossy().into_owned();
    let dispatch_initial = normalise(&dispatch_launches[0], &dispatch_dir_str, &dispatched_worker);
    let dispatch_respawn = normalise(&dispatch_launches[1], &dispatch_dir_str, &dispatched_worker);
    let control_initial = normalise(&control_launches[0], &control.cwd, WORKER_PANE);
    let control_respawn = normalise(&control_launches[1], &control.cwd, WORKER_PANE);

    assert_eq!(
        dispatch_initial, dispatch_respawn,
        "a dispatched pane's respawn must relaunch the worker exactly as its initial spawn did"
    );
    assert_eq!(
        control_initial, control_respawn,
        "a StartAgent pane's respawn must relaunch the worker exactly as its initial spawn did"
    );
    assert_eq!(
        dispatch_respawn, control_respawn,
        "#584's leading hypothesis was that the dispatch and StartAgent paths hand the respawn \
         DIFFERENT relaunch parameters. They do not — and a future change that makes them \
         diverge is what this assertion is here to catch"
    );
}
