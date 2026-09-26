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
/// Bounded by [`common::daemon_task_start_budget`], because the quantity being
/// waited on is a freshly scheduled task getting its turn, so the ceiling has to
/// follow how contended the machine is. It returns the instant the window opens,
/// so an idle box pays nothing for the headroom. (Issue #1148 renamed this from
/// [`common::child_boot_budget`], which this wait had borrowed while naming the
/// right quantity in this very sentence. Same 8 s base and the same load
/// scaling, so nothing here changed behaviour — but `delegate/034` bounded a
/// dispatch task's start on that constant and nobody questioned the size,
/// because the name said it was about a child.)
///
/// **The ~3 s this relies on is a property of `close_agent` specifically, not of
/// the stand-in alone** (issue #1148). The close terminates the child while
/// still holding its whole `RunningAgent` — PTY master included — so a
/// `trap '' TERM` worker really does survive to the SIGKILL backstop here. The
/// RESPAWN leg drops the master BEFORE terminating, which hands the same worker
/// EOF on stdin and ends it in milliseconds; that asymmetry is what
/// `delegate/034` was wrecked by, and it is why its stand-in reads nothing at
/// all while `/022`'s is a `cat`.
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
        // Captured, not re-read in the panic below: `daemon_task_start_budget`
        // samples the machine's load each call, so reporting a second sample
        // would name a duration this attempt never actually waited.
        let ceiling = common::daemon_task_start_budget();
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
    fixture_with_orchestrator("cat", worker_command_in_dir).await
}

/// Issue #708: the orchestrator stand-in `delegate/023` needs — a `cat` behind
/// `stty -echo -icanon -icrnl -opost`, so every byte the daemon writes into the
/// orchestrator's pane appears exactly once and untranslated. Under the cooked
/// `cat` the other tests use, a submit CR and a notice LF both come back as the
/// same echoed CRLF, and "was the notice submitted?" cannot be read at all. The
/// `&&` means the readiness marker is printed only once `stty` has SUCCEEDED.
const RAW_ORCHESTRATOR_COMMAND: &str =
    "stty -echo -icanon -icrnl -opost min 1 time 0 && printf RAW-ORCH-READY && exec cat -u";

/// What [`RAW_ORCHESTRATOR_COMMAND`] prints once its termios is in place.
const RAW_ORCHESTRATOR_READY: &[u8] = b"RAW-ORCH-READY";

async fn fixture_with_orchestrator(
    orchestrator_command: &str,
    worker_command_in_dir: impl FnOnce(&std::path::Path) -> String,
) -> Fixture {
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
            command: Some(orchestrator_command),
            cwd: Some(&cwd),
            display_name: Some("orchestrator"),
            env: vec![
                (DOT_AGENT_DECK_PANE_ID.to_string(), ORCH_PANE.to_string()),
                ("SHELL".to_string(), "/bin/sh".to_string()),
            ],
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
        supersede: false,
        timestamp: chrono::Utc::now(),
        token: None,
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

/// Issue #708: the dead-replacement notice's opening clause and stable FINAL
/// clause. The final clause ends in `.`, which `encode_pane_payload`'s
/// `trim_end_matches` cannot eat, so the byte after it is exactly the
/// terminator the daemon chose.
const RESPAWN_NOTICE_NEEDLE: &[u8] =
    b"delegated worker never came up (dot-agent-deck daemon report)";
const RESPAWN_NOTICE_TAIL: &[u8] = b"daemon log names the role.";

/// Issue #708: the byte the daemon wrote right after the dead-replacement
/// notice's payload — CR if it SUBMITTED the report, LF if it left it as
/// deferred scrollback — or `None` while the notice or that byte is still on its
/// way. Anchored to the notice's own opening and final clauses, never to the
/// first line break after the notice began, so an unrelated line break cannot be
/// mistaken for the terminator in either direction.
fn respawn_notice_terminator(snapshot: &[u8]) -> Option<u8> {
    respawn_notice_terminators(snapshot)
        .first()
        .copied()
        .flatten()
}

/// [`respawn_notice_terminator`] for EVERY dead-replacement notice in the pane,
/// in order: one entry per opening clause, `None` for a notice whose final
/// clause or terminator has not landed yet.
fn respawn_notice_terminators(snapshot: &[u8]) -> Vec<Option<u8>> {
    let starts: Vec<usize> = snapshot
        .windows(RESPAWN_NOTICE_NEEDLE.len())
        .enumerate()
        .filter(|(_, w)| *w == RESPAWN_NOTICE_NEEDLE)
        .map(|(i, _)| i)
        .collect();
    starts
        .iter()
        .map(|&start| {
            let rest = &snapshot[start..];
            let end = rest
                .windows(RESPAWN_NOTICE_TAIL.len())
                .position(|w| w == RESPAWN_NOTICE_TAIL)?
                + RESPAWN_NOTICE_TAIL.len();
            rest.get(end).copied()
        })
        .collect()
}

/// Scenario: start an orchestration whose `clear = true` worker refuses to start
/// while a marker file sits beside it, drop that marker once the first worker is
/// confirmed up, then delegate. The replacement dies before it can announce
/// itself, and the orchestrator must be TOLD — in its own pane, as a SUBMITTED
/// turn naming what to do next, and within five seconds rather than the thirty a
/// readiness wait would cost — instead of being left to wait for a `work-done`
/// that can never arrive.
#[tokio::test(flavor = "multi_thread")]
#[spec("orchestration/delegate/023")]
async fn delegate_023_a_replacement_that_dies_is_reported_to_the_orchestrator() {
    use std::os::unix::fs::PermissionsExt;

    // The stand-in refuses to start once a `die` marker exists beside it. The
    // TEST drops that marker, after confirming the first worker is up — so
    // "the replacement dies before it is ready" is a fact the test establishes,
    // not a race it hopes for.
    let fx = fixture_with_orchestrator(RAW_ORCHESTRATOR_COMMAND, |dir| {
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
    let orchestrator_ready = wait_for_pane_needle(
        &fx.daemon.registry,
        ORCH_PANE,
        RAW_ORCHESTRATOR_READY,
        Duration::from_secs(5),
    )
    .await;
    assert!(
        snapshot_contains(&orchestrator_ready, RAW_ORCHESTRATOR_READY),
        "precondition: the orchestrator stand-in must have its raw termios in place before \
         anything is written to it, or the terminator below is not readable; snapshot = {:?}",
        String::from_utf8_lossy(&orchestrator_ready)
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
        // Issue #708: the terminator byte is part of the wait, so the loop
        // cannot stop one byte early — the submit CR trails the payload by
        // `SUBMIT_DELAY` — and report a terminator that had not landed yet.
        if text.contains("delegated worker never came up")
            && text.contains(WORKER_PANE)
            && respawn_notice_terminator(&snapshot).is_some()
        {
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
    // Issue #708: SUBMITTED, not written. The orchestrator's `delegate` already
    // exited 0, so in an unattended dispatched unit a notice left sitting in its
    // input box reaches nobody and the orchestrator waits forever for a
    // `work-done` that cannot arrive. The report must be a turn of its own (CR)
    // and name what the orchestrator can do about it.
    let text = String::from_utf8_lossy(&snapshot);
    let terminator = respawn_notice_terminator(&snapshot);
    let missing_options: Vec<&str> = ["notify the user", "re-delegate", "reassign"]
        .into_iter()
        .filter(|option| !text.contains(option))
        .collect();
    assert!(
        terminator == Some(b'\r') && missing_options.is_empty(),
        "the dead-replacement notice must be SUBMITTED as a turn (terminated by CR, not left as \
         an LF-terminated line in the orchestrator's scrollback) and must name the remediation \
         options (notify the user, re-delegate, reassign); terminator = {terminator:?}, missing \
         options = {missing_options:?}, orchestrator pane = {text:?}"
    );
    // PRD #249 finding B3's precedent: this notice interpolates the worker's
    // scrubbed pane id and nothing else, so the role name — which is
    // caller-supplied config text — must not appear.
    assert!(
        !String::from_utf8_lossy(&snapshot).contains("'coder'"),
        "the notice must not interpolate the role name; snapshot = {:?}",
        String::from_utf8_lossy(&snapshot)
    );

    // Issue #708 (Greptile P2 on PR #1338): the SAME failure a second time. The
    // `die` marker is still there, so the next delegate's replacement dies too and
    // the daemon composes byte-identical text for the same worker pane. The user
    // has typed into the orchestrator meanwhile, which is the clock that arms the
    // repeat-payload refusal — without it the guard abstains and this would pass
    // for the wrong reason. The first report's payload record therefore has to be
    // released, or the second failure is refused as a repeat of the user's draft
    // and the orchestrator is left waiting after all.
    fx.daemon.registry.note_user_input(ORCH_PANE);
    delegate(&fx, "list the files in this directory").await;
    let deadline = tokio::time::Instant::now() + DEAD_REPLACEMENT_NOTICE_BUDGET;
    let terminators = loop {
        let snapshot = fx
            .daemon
            .registry
            .snapshot(&fx.orchestrator_agent_id)
            .unwrap_or_default();
        let terminators = respawn_notice_terminators(&snapshot);
        if terminators.len() >= 2 && terminators[1].is_some() {
            break terminators;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the SECOND dead-replacement report never reached the orchestrator within \
             {DEAD_REPLACEMENT_NOTICE_BUDGET:?} of the user typing and a second delegate; \
             terminators so far = {terminators:?}, orchestrator pane = {:?}",
            String::from_utf8_lossy(&snapshot)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
        terminators[1],
        Some(b'\r'),
        "the second dead-replacement report must be SUBMITTED like the first; terminators = \
         {terminators:?}"
    );
}

/// Issue #1114: how long `delegate/032` keeps the worker pane in the window
/// between `begin_pane_close` and `close_agent` before finishing the close.
///
/// Unlike the two fixed deadlines issue #709 took out of `delegate/022`, this
/// one cannot expire early on the thing it is waiting for: anywhere inside the
/// 6 s window it stays under, over-holding is safe in BOTH directions. A fixed
/// build parks in the respawn's wait for as long as the hold is up and then
/// re-creates the pane, and a broken one has already failed by then. All the
/// value has to be is long enough that a delegate cannot still be on its way to
/// `spawn_agent`, and short enough to stay inside the 6 s
/// `PANE_CLOSE_SETTLE_TIMEOUT` the daemon allows a close before it stops waiting
/// for one. One second is ~1000x the first and 6x under the second.
///
/// There is deliberately no early exit on "the record has gone", tempting as it
/// is — the record going IS the defect, so ending the hold on it hands the
/// broken build's `spawn_agent` a pane that is no longer held and lets it
/// succeed. Measured: with a 25 ms poll on that condition the pre-fix build came
/// back green in 1.18 s. The hold has to outlast the failure it is provoking,
/// not race it.
const CLOSE_WINDOW_HOLD: Duration = Duration::from_secs(1);

/// Issue #1114: how long `delegate/033` keeps the pane's cleanup hold up AFTER
/// `close_agent` has already removed the record — the starved tail of a close,
/// which is what outrunning `PANE_CLOSE_SETTLE_TIMEOUT` means in production.
///
/// It has to EXCEED that 6 s constant, or the test is `delegate/022`'s ordinary
/// in-flight close wearing a different fixture and says nothing about the
/// give-up this scenario is named for. Eight seconds clears it by two, and a
/// `sleep` can only overshoot, so load moves this in the safe direction. The
/// other end is the recovery's own ceiling — `PANE_CLOSE_RECREATE_TIMEOUT`,
/// 30 s — which this sits comfortably under.
const STARVED_CLOSE_TAIL: Duration = Duration::from_secs(8);

/// Finish a close the way `daemon_protocol.rs`'s `StopAgent` arm finishes one:
/// `close_agent`, then `unregister_pane` under the state write guard, then
/// `finish_pane_close`, and only then drop the cleanup hold.
///
/// The two tests below are ABOUT the interior of that sequence, so a faithful
/// reproduction has to be able to stand between its statements — which is why
/// they assemble it here rather than driving `StopAgent` over the socket the way
/// `delegate/022` does. Everything else about them is the production path: a
/// real `handle_delegate_with_state`, a real registry, real PTYs.
async fn finish_the_close(
    registry: &Arc<AgentPtyRegistry>,
    state: &dot_agent_deck::state::SharedState,
    agent_id: &str,
    hold: dot_agent_deck::agent_pty::PaneCleanupHold,
) -> Result<(), dot_agent_deck::agent_pty::AgentPtyError> {
    let closed = registry.close_agent(agent_id);
    state.write().await.unregister_pane(WORKER_PANE);
    registry.finish_pane_close(WORKER_PANE, closed.is_ok());
    drop(hold);
    closed
}

/// Scenario: put the worker's pane into the exact state the daemon's `StopAgent`
/// handler is in between `begin_pane_close` and `close_agent` — cleanup hold
/// taken, closing mark set, and the pane's registry record still there — then
/// delegate to the `clear = true` role and let the close finish a moment later.
/// The role must come back with a live agent that receives the task pointer, and
/// the delegate must not have destroyed the pane's record on its way past, which
/// the close itself reports by failing `NotFound` against an id that is gone.
#[tokio::test(flavor = "multi_thread")]
#[spec("orchestration/delegate/032")]
async fn delegate_032_a_delegate_inside_the_pre_close_window_leaves_the_record_alone() {
    let fx = fixture(|_| "cat".to_string()).await;

    // The window. `begin_pane_close` runs first and `close_agent` — the step
    // that removes the pane's entry — runs behind a `spawn_blocking` hop, so a
    // delegate landing between them finds a RECORD and takes the ordinary
    // respawn path instead of issue #606's recreation.
    let hold = fx
        .daemon
        .registry
        .hold_pane_for_cleanup(WORKER_PANE, &fx.worker_agent_id)
        .expect("the stopping agent must be able to hold its own pane for cleanup");
    fx.daemon.registry.begin_pane_close(WORKER_PANE);

    assert!(
        fx.daemon.registry.pane_close_in_flight(WORKER_PANE),
        "precondition: the pane must read as mid-close, or the delegate below is an ordinary \
         delegate to a healthy worker"
    );
    assert_eq!(
        fx.daemon
            .registry
            .pane_current_agent_id(WORKER_PANE)
            .as_deref(),
        Some(fx.worker_agent_id.as_str()),
        "precondition: the pane's record must still be PRESENT — a pane with no record is \
         `delegate/022`'s window, not this one, and it reaches the recovery by a different route"
    );

    // Finish the close the way the handler does, once the hold has been up long
    // enough that no delegate can still be on its way to `spawn_agent`. See
    // `CLOSE_WINDOW_HOLD` for why this does not end early on the defect it is
    // provoking.
    let registry = Arc::clone(&fx.daemon.registry);
    let state = fx.daemon.state.clone();
    let closing_id = fx.worker_agent_id.clone();
    let closing = tokio::spawn(async move {
        tokio::time::sleep(CLOSE_WINDOW_HOLD).await;
        finish_the_close(&registry, &state, &closing_id, hold).await
    });

    delegate(&fx, "list the files in this directory").await;

    let closed = closing.await.expect("the close task must not panic");
    assert!(
        closed.is_ok(),
        "the close could not find the agent it was already taking apart, so the delegate had \
         lifted the pane's record out from under it — `respawn_agent_for_pane_declared` removes \
         the entry and only THEN calls `spawn_agent`, which refuses a pane still held for \
         cleanup (#1114). error = {:?}",
        closed.unwrap_err()
    );

    let replacement = wait_for_replacement_agent(
        &fx.daemon.registry,
        WORKER_PANE,
        &fx.worker_agent_id,
        Duration::from_secs(20),
    )
    .await
    .unwrap_or_else(|| {
        panic!(
            "delegating to a `clear = true` role in the window before its close removed the \
             pane's record left the role with no live agent at all — the pane is dead for the \
             rest of the session (#1114, #606). records = {:?}",
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

    let state = fx.daemon.state.read().await;
    assert_eq!(
        state.pane_role_map.get(WORKER_PANE).map(String::as_str),
        Some(WORKER_ROLE),
        "the role must still route after the recovery, or the NEXT delegate is rejected with \
         `reached no worker for role(s)` — the permanent breakage #606 reports"
    );
}

/// Scenario: close the worker's pane for real but leave the close's pane-scoped
/// cleanup running for longer than the six seconds the respawn allows it — a
/// starved close tail — and delegate to the `clear = true` role while it runs.
/// The role must still come back once the close finally lets go, instead of the
/// delegate giving up at the settle timeout and leaving the pane dead for the
/// rest of the session.
#[tokio::test(flavor = "multi_thread")]
#[spec("orchestration/delegate/033")]
async fn delegate_033_a_close_that_outruns_the_settle_timeout_still_brings_the_role_back() {
    let fx = fixture(|_| "cat".to_string()).await;

    let hold = fx
        .daemon
        .registry
        .hold_pane_for_cleanup(WORKER_PANE, &fx.worker_agent_id)
        .expect("the stopping agent must be able to hold its own pane for cleanup");
    fx.daemon.registry.begin_pane_close(WORKER_PANE);
    fx.daemon
        .registry
        .close_agent(&fx.worker_agent_id)
        .expect("close the worker stand-in");

    assert!(
        fx.daemon
            .registry
            .pane_current_agent_id(WORKER_PANE)
            .is_none(),
        "precondition: `close_agent` must have removed the pane's record, or this is \
         `delegate/032`'s window rather than the give-up this test is named for"
    );
    assert!(
        fx.daemon.registry.pane_close_in_flight(WORKER_PANE),
        "precondition: the close's cleanup hold must still be up — it is what `spawn_agent` \
         refuses a fresh pane on, and without it the recovery has nothing to wait for"
    );

    // The starved tail: everything after `close_agent` in the `StopAgent` arm,
    // stretched past `PANE_CLOSE_SETTLE_TIMEOUT`. Measured on a real box at 80
    // concurrent copies, not invented — see issue #1114.
    let registry = Arc::clone(&fx.daemon.registry);
    let state = fx.daemon.state.clone();
    let closing = tokio::spawn(async move {
        tokio::time::sleep(STARVED_CLOSE_TAIL).await;
        state.write().await.unregister_pane(WORKER_PANE);
        registry.finish_pane_close(WORKER_PANE, true);
        drop(hold);
    });

    delegate(&fx, "list the files in this directory").await;

    let replacement = wait_for_replacement_agent(
        &fx.daemon.registry,
        WORKER_PANE,
        &fx.worker_agent_id,
        STARVED_CLOSE_TAIL + Duration::from_secs(20),
    )
    .await
    .unwrap_or_else(|| {
        panic!(
            "the close outran the respawn's settle window, so the `clear = true` delegate gave \
             up and left the role with no agent at all — the pane is dead for the rest of the \
             session (#1114). close still in flight = {}, records = {:?}",
            fx.daemon.registry.pane_close_in_flight(WORKER_PANE),
            fx.daemon.registry.agent_records()
        )
    });
    closing.await.expect("the close task must not panic");

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

    let state = fx.daemon.state.read().await;
    assert_eq!(
        state.pane_role_map.get(WORKER_PANE).map(String::as_str),
        Some(WORKER_ROLE),
        "the role must still route after the recovery, or the NEXT delegate is rejected with \
         `reached no worker for role(s)` — the permanent breakage #606 reports"
    );
}

/// Issue #1148: what a `trap '' TERM HUP` stand-in that reads NOTHING prints
/// once it is armed — `delegate/034`'s worker.
///
/// A separate marker from [`STUBBORN_WORKER_ARMED`] so the two stand-ins cannot
/// be confused in a snapshot dump, and because they are armed against different
/// things: `delegate/022`'s ignores SIGTERM and still reads its PTY, which is
/// all `close_agent` needs; this one must additionally survive having its PTY
/// taken away. See [`delegate_034_a_hold_taken_after_the_record_was_lifted_still_brings_the_role_back`].
const UNKILLABLE_WORKER_ARMED: &[u8] = b"UNKILLABLE-WORKER-ARMED";

/// Issue #1148: what `delegate/034`'s precondition found when it looked for the
/// respawn's gap.
#[derive(Debug)]
enum RespawnGap {
    /// The pane has no record: the respawn has lifted it out and has not yet
    /// published a replacement. This is the state the hold must be taken in.
    Entered,
    /// The pane has a record again, under an agent id that is not the one the
    /// respawn lifted out. So the respawn DID run — it ran to completion,
    /// gap included, before this poll could observe the gap at all.
    ///
    /// A verdict, not an inconclusive attempt: the gap is supposed to be
    /// `AGENT_TERMINATE_GRACE` wide (~3 s) and a 5 ms poll cannot step over
    /// something that lasts 3 s. Seeing a successor therefore means the
    /// premise the whole scenario rests on has stopped holding, and retrying
    /// would only find the same collapsed gap again.
    Collapsed { successor: String },
    /// The budget ran out with the pane still on the agent id the delegate was
    /// aimed at, so nothing lifted anything: the dispatch task never got as far
    /// as the respawn.
    ///
    /// `waited` is the ceiling this attempt ACTUALLY used, carried out rather
    /// than re-sampled by the panic (PR #1149 review, Greptile P2).
    /// [`common::daemon_task_start_budget`] reads `/proc/loadavg` on every
    /// call, so a second call names a duration nobody waited — and misstating
    /// the budget in a failure about the budget is the same class of defect as
    /// the message this whole helper replaces. [`close_pane_into_its_grace_window`]
    /// already captures its ceiling for exactly this reason.
    NeverStarted { waited: Duration },
}

/// Issue #1148: wait for the respawn to lift `pane_id`'s record out, and say
/// WHICH of the three things happened rather than only whether the record is
/// gone.
///
/// **The three-way answer is the fix, not a nicety.** Its predecessor was a
/// `while pane_current_agent_id(..).is_some()` poll whose failure message read
/// "the respawn never lifted the pane's record out" — and in every failure
/// measured for issue #1148 that sentence was FALSE. The respawn had lifted the
/// record and published a replacement 2.57 ms later, and the poll, having
/// missed a window that narrow, spent the rest of its budget looking at the
/// SUCCESSOR's record and reporting it as the original's. A wrong diagnosis is
/// worse than a bare failure: #1148 records a bisect built on that sentence
/// which produced a nonsense culprit. `Collapsed` is the arm that says so, and
/// it says it in milliseconds instead of burning the budget first.
///
/// **Polling is sound here, given a gap that is genuinely
/// `AGENT_TERMINATE_GRACE` wide** — and the fixture is what makes it so; see
/// the stand-in in
/// [`delegate_034_a_hold_taken_after_the_record_was_lifted_still_brings_the_role_back`].
/// Two properties, both needed. "No record for this pane" is MONOTONIC for the
/// rest of the gap: the respawn is parked in
/// `terminate_child_with_grace_and_wait` and is the only thing that can publish
/// onto the pane, so once true the state cannot be left until the respawn
/// itself leaves it — the same argument [`wait_for_pane_record_to_clear`]
/// makes for the close window. And it lasts ~3 s against a 5 ms poll, so a
/// poller gets ~600 looks at it. A poll cannot miss a state that cannot be left
/// and that outlasts its own cadence by three orders of magnitude.
///
/// **Which is why this does NOT take a registry signal for the lift edge, and
/// that was a real choice rather than an omission** (issue #1148). A signal
/// would make the edge unmissable, and unmissable is not what this test needs:
/// it needs to *be inside* the gap when it places the hold, not to learn
/// afterwards that the gap happened. Against the 2.57 ms gap that failed, a
/// signal-woken test would still have lost the race — it would simply have
/// failed somewhere else. The only signal that would fix it is one that HOLDS
/// the respawn between the lift and the spawn, i.e. a test-only barrier in a
/// hot product path, which is a large cost to pay for a premise the fixture can
/// restore on its own. The width of the gap was the defect; the sampling was
/// only how it surfaced.
///
/// Bounded by [`common::daemon_task_start_budget`] rather than
/// [`common::child_boot_budget`]: what is waited on is a freshly
/// `tokio::spawn`ed dispatch task getting its turn and reaching a synchronous
/// record removal, which is not a child producing its first byte. Both are
/// load-scaled and both are 8 s, so this is a naming fix and not a behaviour
/// change — but the old name is how the mis-sized wait went unquestioned in the
/// first place.
async fn wait_for_respawn_to_lift_the_record(
    registry: &AgentPtyRegistry,
    pane_id: &str,
    delegated_agent_id: &str,
) -> RespawnGap {
    let ceiling = common::daemon_task_start_budget();
    let deadline = tokio::time::Instant::now() + ceiling;
    loop {
        match registry.pane_current_agent_id(pane_id) {
            None => return RespawnGap::Entered,
            Some(id) if id != delegated_agent_id => {
                return RespawnGap::Collapsed { successor: id };
            }
            Some(_) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return RespawnGap::NeverStarted { waited: ceiling };
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Issue #1114 (Greptile P1 on PR #1119): how long `delegate/034` keeps the
/// cleanup hold up once it has taken it — a `StopAgent` whose remaining steps
/// are starved, modelled the same way `delegate/033` models a starved close
/// tail.
///
/// It has to outlast the respawn's own `AGENT_TERMINATE_GRACE`, because that
/// grace is what separates the record removal this test waits for from the
/// `spawn_agent` the hold has to refuse. Hold for less and the spawn lands after
/// the hold is down, succeeds, and the test proves nothing. Five seconds is 1.7x
/// the 3 s grace, and a `sleep` can only overshoot, so load moves it in the safe
/// direction — while staying far under the recovery's own 30 s ceiling, above
/// which a FIXED build would fail too.
const STARVED_STOPAGENT_HOLD: Duration = Duration::from_secs(5);

/// Scenario: delegate to a `clear = true` role whose worker survives being
/// terminated, so the respawn spends its full three-second termination grace
/// between lifting the pane's record out and spawning the replacement. Take the
/// pane's cleanup hold inside that gap — a `StopAgent` that read its record a
/// moment before the respawn removed it, and whose own remaining steps are then
/// starved — and keep it up past the grace, so the replacement spawn is
/// refused. The role must still come back once the hold goes down.
#[tokio::test(flavor = "multi_thread")]
#[spec("orchestration/delegate/034")]
async fn delegate_034_a_hold_taken_after_the_record_was_lifted_still_brings_the_role_back() {
    use std::os::unix::fs::PermissionsExt;

    // Issue #1148: NOT `delegate/022`'s stand-in, and the difference is the
    // whole of why that one could not hold this gap open.
    //
    // `/022`'s worker is `trap '' TERM` + `exec cat`, and against `close_agent`
    // that is exactly right: the close terminates the child while still HOLDING
    // its `RunningAgent`, PTY master included, so a SIGTERM-ignoring `cat` has
    // nothing to end it and the close spends the full `AGENT_TERMINATE_GRACE`.
    //
    // The RESPAWN leg is not shaped like that. `respawn_agent_for_pane_declared`
    // deliberately `drop`s the writer and the master BEFORE it terminates — so
    // that any I/O blocked on the PTY unblocks first — and closing the master
    // hands the child EOF on its stdin. A `cat` then exits **0**, of its own
    // accord, with the SIGTERM it is ignoring never mattering at all. Measured:
    // the old child's exit status on this path is `(success, 0)`, never a
    // signal, and the terminate returns after ONE 50 ms `try_wait` tick rather
    // than after 3 s. That made this test's gap ~50 ms instead of ~3 s, and
    // when the child's exit happened to land before the terminate helper's
    // immediate FIRST `try_wait` — a sub-millisecond scheduling race, which is
    // why the failure was load-sensitive — the gap collapsed to **2.57 ms** and
    // the 5 ms poll below stepped straight over it. That is issue #1148.
    //
    // So this worker reads nothing at all: it ignores TERM and HUP (a `trap ''`
    // disposition is `SIG_IGN`, which `execve` preserves, so the `sleep` that
    // replaces the shell inherits both) and then sleeps. Losing the PTY is not
    // an event for it, no signal the grace phase sends can end it, and it
    // survives to the SIGKILL backstop — which is what makes the gap
    // `AGENT_TERMINATE_GRACE` wide BY CONSTRUCTION rather than by luck.
    //
    // `sleep 300` rather than an unbounded sleep or a busy `while :` loop: it is
    // one process with no CPU cost, 50x the worst case this test can take, and a
    // bounded leak if the SIGKILL ever failed to land. Nothing here needs the
    // pane to echo — unlike `/022`, this test asserts no pointer delivery, only
    // that a LIVE replacement takes the pane and the role still routes — so the
    // `cat` that `/022` needs buys this test nothing and costs it the gap.
    let fx = fixture(|dir| {
        let script = dir.join("unkillable-worker.sh");
        let marker = String::from_utf8_lossy(UNKILLABLE_WORKER_ARMED).into_owned();
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntrap '' TERM HUP\nprintf '{marker}'\nexec sleep 300\n"),
        )
        .expect("write signal-ignoring, PTY-ignoring worker stand-in");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod signal-ignoring, PTY-ignoring worker stand-in");
        script.to_string_lossy().into_owned()
    })
    .await;

    // The marker is printed AFTER the trap and BEFORE the exec, so seeing it is
    // proof the dispositions are already `SIG_IGN` — the one fact this scenario
    // cannot proceed without. Issue #709's fix, borrowed.
    let armed = common::wait_for_child_first_output(
        &fx.daemon.registry,
        &fx.worker_agent_id,
        UNKILLABLE_WORKER_ARMED,
    )
    .await;
    assert!(
        snapshot_contains(&armed, UNKILLABLE_WORKER_ARMED),
        "precondition: the worker stand-in never got as far as installing its `trap '' TERM HUP`, \
         so the respawn below would terminate it promptly and leave no gap to place the hold in; \
         snapshot = {:?}",
        String::from_utf8_lossy(&armed)
    );
    assert!(
        !fx.daemon.registry.pane_close_in_flight(WORKER_PANE),
        "precondition: NOTHING is closing this pane when the delegate lands — that is what makes \
         this the ordinary respawn leg rather than `delegate/032`'s window, and the whole point \
         is that the ordinary leg can still meet a hold on its way to `spawn_agent`"
    );

    delegate(&fx, "list the files in this directory").await;

    // The gap: the respawn has lifted the record out and is now inside
    // `terminate_child_with_grace_and_wait`, which the stand-in makes spend the
    // full three seconds. "The record has gone" is the observable edge of it, so
    // the hold lands inside the gap by construction rather than by arithmetic.
    //
    // Issue #1148: the two ways this can fail are now told apart, because the
    // predecessor merged them and named the wrong one. See
    // [`wait_for_respawn_to_lift_the_record`].
    match wait_for_respawn_to_lift_the_record(&fx.daemon.registry, WORKER_PANE, &fx.worker_agent_id)
        .await
    {
        RespawnGap::Entered => {}
        RespawnGap::Collapsed { successor } => panic!(
            "precondition: the respawn lifted the pane's record out AND published a replacement \
             ({successor}, where the delegate was aimed at {delegated}) before this poll could \
             see the gap — so the gap is no longer the ~3 s of `AGENT_TERMINATE_GRACE` this \
             scenario needs, and the hold below would be taken after the spawn it is supposed to \
             refuse. The stand-in has stopped surviving the respawn's terminate: see the fixture \
             above, and note that the respawn drops the PTY master before terminating, so a \
             worker that READS its PTY exits on EOF at once however many signals it traps \
             (issue #1148). records = {records:?}",
            delegated = fx.worker_agent_id,
            records = fx.daemon.registry.agent_records()
        ),
        RespawnGap::NeverStarted { waited } => panic!(
            "precondition: the pane is still on the agent id the delegate was aimed at \
             ({delegated}) after {waited:?}, so nothing lifted its record and the respawn never \
             got going at all — there is no gap to take the hold in and the assertion below \
             would pass for the wrong reason. Unlike the collapsed case above, this one says the \
             dispatch task never reached the respawn. records = {records:?}",
            delegated = fx.worker_agent_id,
            records = fx.daemon.registry.agent_records()
        ),
    }

    // The `StopAgent` that lost this race: it read the worker's record just
    // before the respawn removed it, so `pane_claimed_by_other` sees nothing and
    // the hold is granted. Its own `close_agent` then fails against an id that
    // is gone — which is exactly what the real handler does here, and why it
    // rolls the closing mark back rather than completing.
    let hold = fx
        .daemon
        .registry
        .hold_pane_for_cleanup(WORKER_PANE, &fx.worker_agent_id)
        .expect("a pane nobody claims is grantable to the stopping agent");
    fx.daemon.registry.begin_pane_close(WORKER_PANE);
    let stale_close = fx.daemon.registry.close_agent(&fx.worker_agent_id);
    assert!(
        stale_close.is_err(),
        "precondition: this models a `StopAgent` whose record the respawn already took, so its \
         own close MUST fail — if it succeeded the respawn had not removed anything and the gap \
         was never entered"
    );

    let registry = Arc::clone(&fx.daemon.registry);
    let releasing = tokio::spawn(async move {
        tokio::time::sleep(STARVED_STOPAGENT_HOLD).await;
        registry.finish_pane_close(WORKER_PANE, false);
        drop(hold);
    });

    let replacement = wait_for_replacement_agent(
        &fx.daemon.registry,
        WORKER_PANE,
        &fx.worker_agent_id,
        STARVED_STOPAGENT_HOLD + Duration::from_secs(20),
    )
    .await
    .unwrap_or_else(|| {
        panic!(
            "a cleanup hold taken AFTER the respawn lifted the pane's record out left the role \
             with no agent at all — the ordinary respawn leg had already terminated the worker, \
             and `spawn_agent`'s `DuplicatePaneId` was returned instead of entering the recovery \
             that knows how to wait for the hold (#1114). records = {:?}",
            fx.daemon.registry.agent_records()
        )
    });
    releasing
        .await
        .expect("the hold-release task must not panic");

    let state = fx.daemon.state.read().await;
    assert_eq!(
        state.pane_role_map.get(WORKER_PANE).map(String::as_str),
        Some(WORKER_ROLE),
        "the role must still route afterwards, or the NEXT delegate is rejected with \
         `reached no worker for role(s)` — the permanent breakage #606 reports"
    );
    assert!(
        fx.daemon.registry.agent_is_live(&replacement),
        "and the replacement has to be a LIVE agent rather than a record: this pane's whole \
         history in this test is a worker that was terminated and a spawn that was refused"
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
        supersede: false,
        timestamp: chrono::Utc::now(),
        token: None,
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
