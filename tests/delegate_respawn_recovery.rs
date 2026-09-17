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

use std::sync::Arc;
use std::time::Duration;

use dot_agent_deck::agent_pty::{
    AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, GuardedSend, SpawnOptions, TabMembership,
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
/// Issue #960: the tab's run-identifying title, stamped on every role pane by
/// both producers (`tab.rs` for `Ctrl+n`, `spawn.rs` for a dispatch). Present
/// here rather than `None` because a `clear = true` delegate that has to
/// RE-CREATE its worker pane rebuilds that pane's membership from scratch, and
/// used to rebuild it with `display_title: None` — so the tab kept its label
/// only while some OTHER title-carrying pane was still live, and lost it
/// silently once every pane had exited or been re-created this way. Distinct
/// from `ORCHESTRATION` on purpose: a fallback to the canonical name would read
/// as a pass if the two were equal.
const DISPLAY_TITLE: &str = "recovery-orchestration · issue-960";
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
        display_title: Some(DISPLAY_TITLE.to_string()),
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

/// Issue #954: how many times the close is asked for before the precondition
/// gives up and reports every attempt.
///
/// More than one because a `StopAgent` that comes back WITHOUT the close ever
/// beginning has established nothing — see
/// [`close_pane_into_its_grace_window`]. Three rather than "until the budget
/// runs out": each attempt re-asserts that the stand-in is still live and still
/// owns its pane, so a genuine defect fails on the second pass rather than being
/// retried at, and a small fixed count keeps the failure report short enough to
/// read.
const CLOSE_GRACE_ATTEMPTS: usize = 3;

/// What [`wait_for_pane_record_to_clear`] established.
enum RecordCleared {
    /// The pane has no registry entry and its close is still in flight — a
    /// delegate issued now reaches `respawn_or_recreate_agent_for_pane`'s
    /// `NotFound` arm, which is #606's recovery.
    WhileClosing,
    /// The close completed before the entry cleared. Not a verdict: the attempt
    /// simply did not reach the state it was aiming at.
    WindowShutFirst,
    /// Neither happened inside the budget.
    BudgetExpired,
}

/// PR #1115 review (Greptile P1): wait until `pane_id` has no registry entry,
/// while its close is still in flight.
///
/// See [`close_pane_into_its_grace_window`]'s doc for why the close having
/// BEGUN is not enough, and for why polling for this particular state is sound
/// when polling for the window was not.
async fn wait_for_pane_record_to_clear(
    registry: &AgentPtyRegistry,
    pane_id: &str,
    budget: Duration,
) -> RecordCleared {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        // `agent_id_for_pane_any`, not `pane_current_agent_id`: the respawn's
        // own lookup carries no `exited` filter, so an entry whose child had
        // died would still be found by it and still route away from the
        // recovery. (The one difference left is `pane_handed_over`, which this
        // helper skips and the respawn does not — no handover happens in this
        // test, and a stricter reading would only make this wait end later.)
        if registry.agent_id_for_pane_any(pane_id).is_none() {
            // Re-read rather than trust the earlier signal: the entry could
            // have cleared because the whole close finished, and a delegate
            // aimed after that is an ordinary post-close delegate.
            return if registry.pane_close_in_flight(pane_id) {
                RecordCleared::WhileClosing
            } else {
                RecordCleared::WindowShutFirst
            };
        }
        if !registry.pane_close_in_flight(pane_id) {
            return RecordCleared::WindowShutFirst;
        }
        if tokio::time::Instant::now() >= deadline {
            return RecordCleared::BudgetExpired;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Issue #709/#954: put `pane_id`'s close into its grace window and hand back
/// the still-running request, so a delegate aimed at that window lands inside it
/// rather than at a guessed offset from when the close was asked for.
///
/// **The window is OBSERVED, not sampled for.** The receiver comes from
/// [`AgentPtyRegistry::pane_close_signal`] and is taken BEFORE the request is
/// issued, so there is no interval in which the transition can happen unwitnessed:
/// `begin_pane_close` resolves it by dropping the sender, and a resolved
/// `oneshot` stays resolved, so the observation does not depend on when this
/// function happens to look. That is the whole difference from issue #954's
/// defect. The predecessor polled `pane_close_in_flight` on a 5 ms sleep and
/// **read a finished request as proof that there had been no window**, which is
/// exactly what it cannot prove: an attempt that never reached the daemon is
/// silent about whether the pane would have closed. The reported failures carried
/// `stop_agent finished = true` together with `stand-in still live = true`, and
/// those two are only consistent with the handler never reaching `close_agent` —
/// its first act is removing the registry entry, so a close that had begun could
/// not leave the stand-in live. The test then blamed the window.
///
/// So a request that returns before the close begins is treated as INCONCLUSIVE
/// and retried, and only a run of [`CLOSE_GRACE_ATTEMPTS`] failed attempts is a
/// failure — reported with each attempt's own outcome, which is the fact the old
/// `let _ = closing.await` discarded.
///
/// **The close BEGINNING is not enough, and waiting only for it silently loses
/// the coverage** (PR #1115 review, Greptile P1). `begin_pane_close` — which is
/// what resolves the signal — runs BEFORE `close_agent`, and `close_agent` is
/// what removes the pane's registry entry. A delegate aimed at the gap between
/// them finds a record and takes `respawn_agent_for_pane_declared`'s ORDINARY
/// path, not the `NotFound` recreation path that is all #606 is about. What
/// actually happens then is worse than a silent pass: that path removes the
/// record itself and only then calls `spawn_agent`, which REFUSES a pane still
/// in `cleanup_holds` with `DuplicatePaneId` — an error
/// `respawn_or_recreate_agent_for_pane` returns untouched, since only `NotFound`
/// routes to the recovery. The delegate fails, no replacement ever appears, and
/// the test reports #606's own symptom for a scenario it never reached. The
/// predecessor's gap was WIDER still: it returned on `pane_close_in_flight`,
/// true from `hold_pane_for_cleanup`, a step earlier again.
///
/// So the second half of the precondition waits for the pane's entry to be
/// GONE, with the close still in flight. That wait may poll, and the difference
/// from the defect above is not a matter of degree: "no entry for this pane" is
/// MONOTONIC for the rest of the close, because nothing can publish onto a pane
/// in `cleanup_holds` — `spawn_agent` refuses it — so once true it stays true
/// until the hold lifts, which is ~3 s of `AGENT_TERMINATE_GRACE` away. A poll
/// cannot miss a state that cannot be left. It re-reads `pane_close_in_flight`
/// on every turn as well, so it can never draw a conclusion from a window that
/// has since shut; that, too, is an inconclusive attempt rather than a verdict.
///
/// Bounded by [`common::child_boot_budget`] for the same reason the boot waits
/// are: the quantity being waited on is a freshly scheduled task getting its
/// turn, so the ceiling has to follow how contended the machine is. It returns
/// the instant the window opens, so an idle box pays nothing for the headroom.
async fn close_pane_into_its_grace_window(
    registry: &AgentPtyRegistry,
    attach_path: &std::path::Path,
    pane_id: &str,
    agent_id: &str,
) -> tokio::task::JoinHandle<Result<(), dot_agent_deck::daemon_client::ClientError>> {
    let mut attempts: Vec<String> = Vec::new();
    for attempt in 1..=CLOSE_GRACE_ATTEMPTS {
        // Issue #709: the assertion below says "still alive" but only ever
        // checked REGISTRATION, and the difference is the whole scenario.
        // `close_agent` spends `AGENT_TERMINATE_GRACE` only while the child is
        // still running — against an already-dead one it returns at once, the
        // close transition opens and shuts inside a few milliseconds, and the
        // grace window this test needs to deliver into never observably exists.
        //
        // Issue #954: re-checked on EVERY attempt, which is what keeps the retry
        // below from papering over a real defect. A previous attempt that closed
        // the pane without ever calling `begin_pane_close` leaves the stand-in
        // dead, and that fails here — naming every attempt — instead of being
        // asked again.
        assert!(
            registry.agent_is_live(agent_id)
                && registry.pane_current_agent_id(pane_id).as_deref() == Some(agent_id),
            "precondition: the worker stand-in must own its pane and still be running for the \
             close to spend its termination grace (attempt {attempt} of {CLOSE_GRACE_ATTEMPTS}); \
             earlier attempts = {attempts:?}, records = {:?}",
            registry.agent_records()
        );
        // Taken BEFORE the request is issued: everything after this point is
        // witnessed, whether the transition takes three seconds or one
        // instruction. A pane already mid-close hands back a pre-resolved
        // receiver, so even the register-after-`begin_pane_close` ordering is
        // covered.
        let close_began = registry.pane_close_signal(pane_id);
        let client = dot_agent_deck::daemon_client::DaemonClient::new(attach_path.to_path_buf());
        let closing_id = agent_id.to_string();
        let mut request = tokio::spawn(async move { client.stop_agent(&closing_id).await });
        // Captured, not re-read in the panic below: `child_boot_budget` samples
        // the machine's load each call, so reporting a second sample would name
        // a duration this attempt never actually waited.
        let ceiling = common::child_boot_budget();
        let budget = tokio::time::sleep(ceiling);
        tokio::pin!(budget);
        tokio::select! {
            // `biased` so a close that begins in the same instant the request
            // returns is read as the window it is, never as a failed attempt.
            biased;
            _ = close_began => {
                // The close has begun. Now wait for the half that makes the
                // delegate below reach #606's path at all: the pane's registry
                // entry gone, with the window still open. The doc above has the
                // ordering and why this poll is sound where the one it replaced
                // was not.
                match wait_for_pane_record_to_clear(registry, pane_id, ceiling).await {
                    RecordCleared::WhileClosing => return request,
                    RecordCleared::WindowShutFirst => attempts.push(format!(
                        "attempt {attempt}: the close began and had already finished before the \
                         pane's registry entry cleared, so the delegate could not be aimed \
                         inside the window"
                    )),
                    RecordCleared::BudgetExpired => panic!(
                        "precondition: {pane_id}'s close entered its grace window but its \
                         registry entry was still there {ceiling:?} later, so a delegate now \
                         would take the ordinary respawn path instead of #606's recreation \
                         path; attempt {attempt} of {CLOSE_GRACE_ATTEMPTS}, earlier attempts \
                         = {attempts:?}, stop_agent finished = {}, close still in flight = {}, \
                         records = {:?}",
                        request.is_finished(),
                        registry.pane_close_in_flight(pane_id),
                        registry.agent_records()
                    ),
                }
            }
            outcome = &mut request => {
                attempts.push(format!(
                    "attempt {attempt}: stop_agent returned {outcome:?} without the close ever \
                     beginning"
                ));
            }
            _ = &mut budget => {
                panic!(
                    "precondition: the close never entered its grace window within {:?} and the \
                     `StopAgent` request is still in flight, so the delegate below would race a \
                     teardown that has not started instead of landing in #606's window; \
                     attempt {attempt} of {CLOSE_GRACE_ATTEMPTS}, earlier attempts = {attempts:?}, \
                     stand-in still live = {}, records = {:?}",
                    ceiling,
                    registry.agent_is_live(agent_id),
                    registry.agent_records()
                );
            }
        }
    }
    panic!(
        "precondition: {CLOSE_GRACE_ATTEMPTS} `StopAgent` requests each came back without the \
         pane's close ever beginning, so the delegate below would be an ordinary post-close \
         delegate instead of #606's race; attempts = {attempts:?}, stand-in still live = {}, \
         records = {:?}",
        registry.agent_is_live(agent_id),
        registry.agent_records()
    );
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

    // Issue #709: this was a flat `sleep(200 ms)` — the reporter's own interval —
    // followed by a bare assertion, and it was the SECOND fixed deadline in this
    // test. The 200 ms was standing in for "the close has entered its grace
    // window", but `stop_agent` is driven by a spawned task and a socket round
    // trip, so on a loaded box neither had necessarily reached `begin_pane_close`
    // yet when the sleep expired. The assertion then fired saying the close was
    // not in flight — and it was right, for the opposite of the reason it names:
    // not "the close already finished" but "the close had not started".
    //
    // Issue #954: its replacement, a 5 ms poll of `pane_close_in_flight` that
    // bailed on `JoinHandle::is_finished`, kept the second half of that defect —
    // it still reported a request that had failed as a window that had closed.
    // `close_pane_into_its_grace_window` observes the transition through the
    // registry's own close signal, taken before the request is issued, and
    // retries an attempt that establishes nothing. Its doc carries the evidence.
    let closing = close_pane_into_its_grace_window(
        &fx.daemon.registry,
        &fx.daemon.attach_path,
        WORKER_PANE,
        &fx.worker_agent_id,
    )
    .await;
    delegate(&fx, "list the files in this directory").await;

    let replacement = wait_for_replacement_agent(
        &fx.daemon.registry,
        WORKER_PANE,
        &fx.worker_agent_id,
        Duration::from_secs(20),
    )
    .await
    .unwrap_or_else(|| {
        // Issue #954: the two extra values separate the verdicts this one
        // sentence used to merge. A `clear = true` respawn that finds no record
        // waits `PANE_CLOSE_SETTLE_TIMEOUT` (6 s, twice `AGENT_TERMINATE_GRACE`)
        // for the pane's hold to lift and then spawns anyway — and
        // `spawn_agent` REFUSES a pane that is still held, so a close that
        // outruns that window leaves the pane empty for the session — issue
        // #1114, which is a product-side residual of #606 rather than a
        // regression in this test, and which reads identically to one because
        // the pane is simply empty either way. `close still in flight` is what
        // tells them apart: true means the teardown never released the pane in
        // time (#1114), false means the recovery had its chance and did not take
        // it, which is #606 proper. Measured by shortening that constant below
        // the grace — deterministic — and no test-side budget recovers it: the
        // wait above was temporarily widened to 120 s and expired with the pane
        // still empty, so it was left at its flat 20 s.
        panic!(
            "delegating to a `clear = true` role while its pane was mid-close left the role with \
             no live agent at all — the pane is dead for the rest of the session (#606). \
             close still in flight = {}, stop_agent finished = {}, records = {:?}",
            fx.daemon.registry.pane_close_in_flight(WORKER_PANE),
            closing.is_finished(),
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

    // Issue #960's secondary path: the re-created pane's membership. There is no
    // record left to respawn from here, so the replacement is built from
    // `PaneRecreateIdentity` — which hardcoded `display_title: None`, silently
    // dropping the tab's run-identifying label from this pane. It is not
    // immediately visible, because `partition_hydrated_panes` keeps the first
    // non-`None` title it finds and the orchestrator pane still has one; the
    // label is lost once every title-carrying pane has exited or been re-created
    // this way. Asserted on the RECREATED pane specifically, since that is the
    // only one whose membership this path authors.
    let recreated_membership = fx
        .daemon
        .registry
        .agent_records()
        .into_iter()
        .find(|r| r.pane_id_env.as_deref() == Some(WORKER_PANE))
        .and_then(|r| r.tab_membership)
        .unwrap_or_else(|| {
            panic!(
                "the recovered worker pane must have an Orchestration membership; records = {:?}",
                fx.daemon.registry.agent_records()
            )
        });
    let TabMembership::Orchestration {
        display_title,
        role_index,
        orchestration_id,
        ..
    } = &recreated_membership
    else {
        panic!("the recovered worker pane left its orchestration tab: {recreated_membership:?}");
    };
    assert_eq!(
        (
            display_title.as_deref(),
            *role_index,
            orchestration_id.as_deref()
        ),
        (Some(DISPLAY_TITLE), 1, Some(ORCHESTRATION_ID)),
        "the re-created worker must rejoin its tab with the tab's own title, index and instance \
         token — a `None` title here is issue #960's secondary path, and it costs the tab its \
         label as soon as the last pane that still carries one goes away"
    );

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
            compose_orchestrator_context: Some(
                dot_agent_deck::orchestrator_context::Attendance::Unattended,
            ),
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

/// A raw, no-echo `cat` on `pane_id` that prints `marker` once its termios is
/// already in raw mode. Every byte the daemon submits into the pane afterwards
/// appears exactly once in the agent's snapshot and nothing else does, which is
/// what makes "this text was NOT submitted" directly observable — the same stub
/// `idle_worker_detector.rs` and `work_done_reporting.rs` use for the same
/// reason.
fn spawn_raw_cat_observer(
    registry: &Arc<AgentPtyRegistry>,
    pane_id: &str,
    marker: &str,
    cwd: &str,
) -> String {
    let command =
        format!("stty -echo -icanon -icrnl -opost min 1 time 0 && printf {marker} && exec cat -u");
    registry
        .spawn_agent(SpawnOptions {
            command: Some(&command),
            cwd: Some(cwd),
            env: vec![
                (DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string()),
                ("SHELL".to_string(), "/bin/sh".to_string()),
            ],
            ..SpawnOptions::default()
        })
        .unwrap_or_else(|error| panic!("spawn raw-cat observer on {pane_id}: {error}"))
}

/// Poll `agent_id`'s scrollback until `needle` appears, or the deadline passes.
/// Returns whatever the last snapshot held so the caller can print it.
async fn wait_for_snapshot(registry: &AgentPtyRegistry, agent_id: &str, needle: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot =
            String::from_utf8_lossy(&registry.snapshot(agent_id).unwrap_or_default()).into_owned();
        if snapshot.contains(needle) || tokio::time::Instant::now() >= deadline {
            return snapshot;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Scenario: An agent asks the daemon to dispatch, and its pane changes hands
/// while `handle_dispatch` does its worktree-and-spawn work — the caller is
/// closed and an unrelated agent inherits the same `DOT_AGENT_DECK_PANE_ID`.
/// Delivering the dispatch result must be refused as `WrongSession`, and none of
/// the result text may appear in the successor's scrollback even after a later
/// authorized write to that successor has demonstrably landed.
#[spec("orchestration/dispatch/006")]
#[tokio::test]
async fn dispatch_006_a_dispatch_result_is_refused_when_the_caller_pane_changed_hands() {
    common::init_test_env();
    const PANE: &str = "dispatch-result-handover-pane";
    const RESULT: &str = "DISPATCH-RESULT-MUST-NOT-REACH-THE-SUCCESSOR-2f7c";
    const BARRIER: &str = "AUTHORIZED-WRITE-AFTER-THE-REFUSAL-9b13";

    let dir = common::race_safe_tempdir();
    let cwd = dir.path().to_string_lossy().into_owned();
    let registry = Arc::new(AgentPtyRegistry::new());

    // The agent that asked for the dispatch, and whose registry id the daemon
    // captures from ONE `AgentRecord` before the slow half runs.
    let caller = spawn_raw_cat_observer(&registry, PANE, "CALLER-READY", &cwd);
    let ready = wait_for_snapshot(&registry, &caller, "CALLER-READY").await;
    assert!(
        ready.contains("CALLER-READY"),
        "precondition: the caller stub must be up before it is handed over; snapshot = {ready:?}"
    );

    // The hand-over: the caller goes away and an unrelated agent takes the pane
    // id, exactly as a recycled `DOT_AGENT_DECK_PANE_ID` is reissued.
    registry.close_agent(&caller).expect("close the caller");
    let successor = spawn_raw_cat_observer(&registry, PANE, "SUCCESSOR-READY", &cwd);
    assert_ne!(
        caller, successor,
        "the hand-over must produce a NEW registry agent id"
    );
    let ready = wait_for_snapshot(&registry, &successor, "SUCCESSOR-READY").await;
    assert!(
        ready.contains("SUCCESSOR-READY"),
        "precondition: the successor must be up and echoing, or the absence asserted below \
         proves only that its stub never started; snapshot = {ready:?}"
    );

    let outcome =
        dot_agent_deck::daemon::deliver_dispatch_result(&registry, PANE, &caller, RESULT).await;
    assert_eq!(
        outcome,
        GuardedSend::WrongSession,
        "a dispatch result bound to the caller must be refused once the caller's pane belongs \
         to somebody else"
    );

    // A barrier, not a sleep: an AUTHORIZED write to the successor that has
    // demonstrably arrived proves the pane has drained past the point where a
    // leaked result would have landed, so its absence below is a fact rather
    // than a race the test happened to win.
    let barrier = registry
        .write_and_submit_guarded(PANE, BARRIER, &successor, || async { true })
        .await
        .expect("the barrier write must reach the registry");
    assert_eq!(
        barrier,
        GuardedSend::Applied,
        "the successor owns the pane, so a write bound to IT must be applied — otherwise this \
         test proves nothing about the refusal above"
    );
    let snapshot = wait_for_snapshot(&registry, &successor, BARRIER).await;
    assert!(
        snapshot.contains(BARRIER),
        "the barrier write never reached the successor's PTY, so the absence below is untested; \
         snapshot = {snapshot:?}"
    );
    assert!(
        !snapshot.contains(RESULT),
        "the dispatch result reached a process that merely inherited the caller's pane id — the \
         successor may act on it with its own tools; snapshot = {snapshot:?}"
    );

    registry.shutdown_all();
}
