#![cfg(feature = "e2e")]

//! PTY-attached, REAL-binary proof of the hook-socket provenance gate (issue
//! #1077), under the DEFAULT policy.
//!
//! ## Why this file exists when the gate is already unit-tested
//!
//! `src/hook_provenance.rs` pins the decision matrix and `src/daemon.rs`'s
//! `hook_provenance_*` tests drive the real `run_hook_loop` over a real socket.
//! Neither of them can see the seam this issue actually turns on: that the
//! daemon's **spawn** puts the token in the pane's environment, that the **real
//! CLI** reads it back out of that environment, and that the daemon then
//! attests it. Every one of those is in a different process, and a change that
//! quietly dropped any one of them would leave all the in-crate tests green
//! while every real delegation stopped working.
//!
//! So this test runs the same verb twice against one live deck:
//!
//! 1. **Forged** — the real `dot-agent-deck work-done` binary, launched by the
//!    TEST process, carrying the worker's `DOT_AGENT_DECK_PANE_ID` and the
//!    deck's hook socket path and nothing else. That is precisely the shape
//!    issue #1077 describes: a same-uid process that learned a pane id (here
//!    handed to it; in the wild, read off `daemon status`) and signals as that
//!    pane. It must reach the orchestrator's pane with nothing — and, since
//!    issue #1129, must be TOLD it was refused rather than exiting 0 on a report
//!    that went nowhere.
//! 2. **Legitimate** — the same binary, same verb, run by the worker's OWN
//!    pane, so the daemon's spawn gave it the capability token. It must reach
//!    the orchestrator's pane exactly as it always did.
//!
//! The order is load-bearing. The forgery is sent FIRST and the legitimate
//! signal second, so the legitimate report's arrival is itself the proof that
//! enough time passed for the forged one to have arrived had it been accepted.
//! An "absent after a fixed sleep" assertion proves much less.
//!
//! Lane 1 (`cargo test-e2e`): both roles are stand-ins, no agent and no
//! credential, so CI runs it on every PR.

mod common;

use std::cell::RefCell;
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::daemon_protocol::TabMembership;
use spec::spec;

/// Scenario: Open a real deck whose orchestrator command reports waiting, running and waiting from inside its own pane, then forge running and SessionStart from outside that pane with its public identity and no token. Its visible card must keep the legitimate status and identity after each forgery, and its own later finished event must still drive the card to Idle.
#[spec("orchestration/provenance/003")]
#[test]
fn provenance_003_outside_status_events_cannot_drive_a_spawned_panes_card() {
    use dot_agent_deck::event::EventType;
    let deck = TuiDeck::builder()
        .with_pty_size(160, 40)
        .with_env("DOT_AGENT_DECK_HOOK_PROVENANCE", "enforce")
        .with_env("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "0")
        .with_env("DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS", "0")
        .with_env("DAD_TEST_BIN", env!("CARGO_BIN_EXE_dot-agent-deck"))
        .launch_with_fixture("status-provenance");
    deck.wait_for_string("No active agents");
    let sub = deck.subscribe_events();
    open_orchestration(&deck);
    let (_, agent) = orchestration_ids(&deck);
    let record = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|record| record.id == agent)
        .expect("orchestrator record");
    let pane = record.pane_id_env.expect("pane id");
    sub.wait_for(
        |event| {
            event.pane_id.as_deref() == Some(&pane)
                && event.event_type == EventType::WaitingForInput
        },
        Duration::from_secs(20),
    );
    std::fs::write(deck.workdir().join("status-running"), b"go").expect("release own running");
    let own_running = sub.wait_for(
        |event| event.pane_id.as_deref() == Some(&pane) && event.event_type == EventType::Thinking,
        Duration::from_secs(20),
    );
    deck.wait_until_grid("own status renders Thinking", |grid| {
        grid.contains("orchestrator") && grid.contains("Thinking")
    });
    std::fs::write(deck.workdir().join("status-waiting"), b"go").expect("release own waiting");
    let own_waiting = sub.wait_for(
        |event| {
            event.pane_id.as_deref() == Some(&pane)
                && event.event_type == EventType::WaitingForInput
                && event.timestamp > own_running.timestamp
        },
        Duration::from_secs(20),
    );
    deck.wait_until_grid("own status renders Needs Input", |grid| {
        grid.contains("orchestrator") && grid.contains("Needs Input")
    });

    let forged = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["agent-event", "--type", "running"])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", &pane)
        .env("DOT_AGENT_DECK_AGENT_ID", &agent)
        .env_remove("DOT_AGENT_DECK_PANE_CAPABILITY")
        .env("HOME", deck.home_dir())
        .current_dir(deck.workdir())
        .output()
        .expect("outside status CLI");
    assert!(
        forged.status.success() && forged.stdout.is_empty(),
        "raw status CLI remains fire-and-forget"
    );
    let forgery_landed = sub
        .try_wait_for(
            |event| {
                event.pane_id.as_deref() == Some(&pane)
                    && event.event_type == EventType::Thinking
                    && event.timestamp > own_waiting.timestamp
            },
            Duration::from_secs(2),
        )
        .is_some();
    if forgery_landed {
        deck.wait_until_grid(
            "outside running erroneously drove the visible card",
            |grid| grid.contains("orchestrator") && grid.contains("Thinking"),
        );
    }
    assert!(
        !forgery_landed,
        "issue #318: outside agent-event --type running reached attach clients and drove the pane's card; grid:\n{}",
        deck.snapshot_grid()
    );
    deck.wait_until_grid_then_hold(
        "outside running cannot change the card",
        Duration::from_millis(500),
        |grid| grid.contains("Needs Input") && !grid.contains("Thinking"),
    );

    // Reading to EOF is an ingestion barrier: the absence below is checked
    // after the daemon processed the forged SessionStart, not after a sleep.
    use std::io::{Read, Write};
    let mut stream =
        std::os::unix::net::UnixStream::connect(deck.hook_socket_path()).expect("hook socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read bound");
    let start = serde_json::json!({"session_id": "forged-318", "pane_id": pane, "agent_id": agent,
        "agent_type": "pi", "event_type": "session_start", "timestamp": chrono::Utc::now(),
        "metadata": {"display_name": "FORGED-318-CARD"}});
    writeln!(stream, "{start}").expect("forged SessionStart");
    stream
        .shutdown(std::net::Shutdown::Write)
        .expect("half-close");
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).expect("ingestion barrier");
    assert!(reply.is_empty(), "raw hook events must remain silent");
    deck.wait_until_grid_then_hold(
        "forged SessionStart cannot retire or rename the card",
        Duration::from_millis(500),
        |grid| grid.contains("Needs Input") && !grid.contains("FORGED-318"),
    );
    assert!(
        !sub.snapshot()
            .iter()
            .any(|event| event.session_id == "forged-318"),
        "forged SessionStart must never reach attach clients"
    );
    std::fs::write(deck.workdir().join("status-finished"), b"go").expect("release own finished");
    deck.wait_until_grid("own later status still drives the card", |grid| {
        grid.contains("orchestrator") && grid.contains("Idle") && !grid.contains("Needs Input")
    });
}

/// The report the worker's own pane sends first. Its arrival proves the
/// legitimate path is live — the daemon's role maps are populated and the
/// worker's own token attests it — BEFORE the forgery is sent. Without that
/// ordering, a forgery dropped as "unknown pane" because the maps were not yet
/// populated is indistinguishable from one refused for want of provenance, and
/// the first cut of this test passed with the gate disabled for exactly that
/// reason.
const ATTESTED_FIRST: &str = "PROVENANCE-ATTESTED-ONE-7f3a";

/// The report the worker's own pane sends second, AFTER the forgery. Its
/// arrival is what makes the absence below an assertion rather than a fixed
/// sleep: a later message has completed the whole round trip on the same path.
const ATTESTED_SECOND: &str = "PROVENANCE-ATTESTED-TWO-4d2e";

/// The report the forgery sends. Must reach nothing.
const FORGED_SENTINEL: &str = "PROVENANCE-FORGED-9c1b";

/// The production new-pane flow that registers the daemon-side role maps
/// `handle_work_done` routes on: `Ctrl+n` → confirm dir → Right selects the
/// `[Orch: demo-orch]` chip → Enter → Enter.
fn open_orchestration(deck: &TuiDeck) {
    deck.send_keys(b"\x0e");
    deck.send_keys(b" ");
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
}

/// `(worker pane id, orchestrator registry agent id)` once both role panes are
/// registered — the pane id the forgery will claim, and the PTY the daemon
/// writes feedback into.
fn orchestration_ids(deck: &TuiDeck) -> (String, String) {
    let ids = RefCell::new(None);
    let ready = common::wait_until(Duration::from_secs(20), || {
        let records = common::agent_records_on(deck.attach_socket_path());
        let worker = records
            .iter()
            .find_map(|record| match &record.tab_membership {
                Some(TabMembership::Orchestration { role_name, .. }) if role_name == "worker" => {
                    record.pane_id_env.clone()
                }
                _ => None,
            });
        let orchestrator = records.iter().find_map(|record| {
            matches!(
                &record.tab_membership,
                Some(TabMembership::Orchestration {
                    is_start_role: true,
                    ..
                })
            )
            .then(|| record.id.clone())
        });
        if let (Some(worker), Some(orchestrator)) = (worker, orchestrator) {
            *ids.borrow_mut() = Some((worker, orchestrator));
            return true;
        }
        false
    });
    assert!(
        ready,
        "the orchestration's role panes were not registered within 20s; records = {:?}",
        common::agent_records_on(deck.attach_socket_path())
    );
    ids.into_inner().expect("the ready poll stores both ids")
}

/// The orchestrator PTY's scrollback straight from the daemon — the bytes it
/// wrote, before any rendering is involved.
fn orchestrator_pty(deck: &TuiDeck, orchestrator_agent_id: &str) -> String {
    String::from_utf8_lossy(&common::pane_snapshot_on(
        deck.attach_socket_path(),
        orchestrator_agent_id,
    ))
    .into_owned()
}

/// Release one of the worker's two trigger files and wait for the report it
/// gates to reach the orchestrator's PTY.
fn release_and_await(deck: &TuiDeck, trigger: &str, sentinel: &str, orchestrator_agent: &str) {
    std::fs::write(deck.workdir().join(trigger), b"go\n")
        .unwrap_or_else(|e| panic!("create the worker's trigger file {trigger}: {e}"));
    let landed = common::wait_until(Duration::from_secs(60), || {
        orchestrator_pty(deck, orchestrator_agent).contains(sentinel)
    });
    assert!(
        landed,
        "the worker's OWN `work-done`, run from inside its pane with the token the daemon put \
         in that pane's environment, never reached the orchestrator ({sentinel}) — the \
         provenance gate is refusing legitimate traffic, which is worse than the forgery it \
         exists to stop\nOrchestrator PTY:\n{}",
        orchestrator_pty(deck, orchestrator_agent)
    );
}

/// Scenario: Launch the real TUI and its lazy daemon under the DEFAULT hook-provenance policy and open the two-role `hook-provenance` fixture. Let the worker report once from inside its own pane so the legitimate path is proven live, then run the REAL `dot-agent-deck work-done` binary from the test process carrying only the worker's `DOT_AGENT_DECK_PANE_ID` — the same-uid forgery of issue #1077 — then let the worker report a second time. The orchestrator's pane must carry both of the worker's own reports and must never carry the forged one, and the forged invocation must now exit non-zero naming the daemon's refusal (issue #1129).
#[spec("orchestration/provenance/001")]
#[test]
fn provenance_001_a_forged_work_done_is_refused_while_the_pane_s_own_still_lands() {
    let deck = TuiDeck::builder()
        .with_pty_size(120, 40)
        // Both delegation watches off: nothing is delegated here, and a detector
        // firing into the orchestrator pane would be noise competing with the
        // one thing under assertion.
        .with_env("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "0")
        .with_env("DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS", "0")
        .with_env("DAD_TEST_BIN", env!("CARGO_BIN_EXE_dot-agent-deck"))
        .with_env("DAD_PROVENANCE_SENTINEL_1", ATTESTED_FIRST)
        .with_env("DAD_PROVENANCE_SENTINEL_2", ATTESTED_SECOND)
        // Deliberately NOT `impersonating_pane_signals()`. This is the one
        // work-done e2e that runs under the shipped policy.
        .launch_with_fixture("hook-provenance");
    deck.wait_for_string("No active agents");
    open_orchestration(&deck);
    deck.wait_for_string("worker");

    let (worker_pane, orchestrator_agent) = orchestration_ids(&deck);

    // ---- 1. THE LEGITIMATE PATH, PROVEN LIVE ----------------------------
    release_and_await(
        &deck,
        "provenance-go-1",
        ATTESTED_FIRST,
        &orchestrator_agent,
    );

    // ---- 2. THE FORGERY -------------------------------------------------
    // Everything a same-uid process on this box can obtain: the socket path
    // (predictable, and injected into every agent) and a pane id (published by
    // `daemon status` / `list-agents`). No token, because there is no way for
    // this process to have one.
    let forged = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .arg("work-done")
        .env_remove("DOT_AGENT_DECK_PANE_CAPABILITY")
        .arg("--task")
        .arg(format!("Forged completion. {FORGED_SENTINEL}"))
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", &worker_pane)
        .env("HOME", deck.home_dir())
        .current_dir(deck.workdir())
        .output()
        .expect("run the real `dot-agent-deck work-done` CLI as a forgery");
    // Issue #1129: the daemon now acknowledges this verb at the provenance gate,
    // so a refusal is reported to whoever sent it. Until then `work-done` read
    // no reply and exited 0 whatever the daemon did — and this assertion is the
    // inverse of the one that used to stand here, which recorded that silence as
    // the honest cost of the refusal.
    //
    // Being told is for the LEGITIMATE sender that trips the gate — a
    // `dot-agent-deck` in the pane older than the daemon, refused as
    // `missing_token` — not for this forgery, which is simply the only way to
    // drive a refusal through the real binary. It discloses nothing new either:
    // pane ids are published by `daemon status` and `list-agents`, and `delegate`
    // has answered its refusals on this same socket since #1077.
    let forged_stderr = String::from_utf8_lossy(&forged.stderr).into_owned();
    assert!(
        !forged.status.success(),
        "the forged `work-done` exited 0, so a refused report is still silent to its sender \
         (issue #1129); stdout={} stderr={forged_stderr}",
        String::from_utf8_lossy(&forged.stdout)
    );
    assert!(
        forged_stderr.contains("hook capability token"),
        "the CLI failed without passing on the daemon's reason, so the one legitimate cause \
         (an older binary in the pane) is undiagnosable from the caller's side: \
         {forged_stderr}"
    );

    // ---- 3. A LATER LEGITIMATE SIGNAL COMPLETES THE ROUND TRIP ----------
    release_and_await(
        &deck,
        "provenance-go-2",
        ATTESTED_SECOND,
        &orchestrator_agent,
    );

    // ---- 4. THE FORGERY LANDED NOWHERE ----------------------------------
    let pty = orchestrator_pty(&deck, &orchestrator_agent);
    assert!(
        !pty.contains(FORGED_SENTINEL),
        "a process that knew nothing but the worker's pane id made the daemon write into the \
         ORCHESTRATOR's pane — the gate did not hold\nOrchestrator PTY:\n{pty}"
    );
}

/// One role pane of an orchestration tab: its registry agent id (what a PTY
/// snapshot is keyed on) and its `DOT_AGENT_DECK_PANE_ID`.
#[derive(Clone, Debug)]
struct RolePane {
    agent_id: String,
    pane_id: String,
}

/// The live orchestration tabs, keyed by their per-tab `orchestration_id`, each
/// as `role name → pane`.
fn orchestration_tabs(
    deck: &TuiDeck,
) -> std::collections::BTreeMap<String, std::collections::HashMap<String, RolePane>> {
    let mut tabs: std::collections::BTreeMap<String, std::collections::HashMap<String, RolePane>> =
        std::collections::BTreeMap::new();
    for record in common::agent_records_on(deck.attach_socket_path()) {
        let (
            Some(TabMembership::Orchestration {
                role_name,
                orchestration_id: Some(orchestration_id),
                ..
            }),
            Some(pane_id),
        ) = (record.tab_membership.clone(), record.pane_id_env.clone())
        else {
            continue;
        };
        tabs.entry(orchestration_id).or_default().insert(
            role_name,
            RolePane {
                agent_id: record.id.clone(),
                pane_id,
            },
        );
    }
    tabs
}

/// The registry agent id holding `pane` now — see [`squeezed_pty`] for why it
/// is not the one captured when the tab came up.
fn current_agent_id(deck: &TuiDeck, pane: &RolePane) -> String {
    common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|record| record.pane_id_env.as_deref() == Some(pane.pane_id.as_str()))
        .map_or_else(|| pane.agent_id.clone(), |record| record.id)
}

/// The scrollback of whichever agent holds `pane` NOW, straight from the
/// daemon, with whitespace squeezed out so a needle wrapped at the pane's width
/// still matches. Resolved by pane id at every read rather than by an agent id
/// captured earlier: a role pane's agent can be replaced while the deck brings
/// the tab up, and a snapshot of the replaced agent reads as empty.
fn squeezed_pty(deck: &TuiDeck, pane: &RolePane) -> String {
    String::from_utf8_lossy(&common::pane_snapshot_on(
        deck.attach_socket_path(),
        &current_agent_id(deck, pane),
    ))
    .chars()
    .filter(|c| !c.is_whitespace())
    .collect()
}

/// The daemon's task pointer for the fixture's `worker` role, whitespace-free.
const WORKER_POINTER: &str = "worker-task-worker.md";

/// Scenario: Launch the real TUI and its lazy daemon under the DEFAULT hook-provenance policy and open the `stale-pane-identity` orchestration TWICE in one directory, so two orchestrators, A and B, are live at once. Inside B's own pane, run the real `delegate` with `DOT_AGENT_DECK_PANE_ID` and `DOT_AGENT_DECK_AGENT_ID` rewritten to A's — issue #712's stale identity from another dispatch, everything else as the daemon spawned it — and then delegate again with B's untouched environment. The stale delegate must exit non-zero with the daemon's refusal; B's own delegate must reach B's worker; and A's worker must never receive a task pointer.
#[spec("orchestration/provenance/002")]
#[test]
fn provenance_002_a_stale_pane_identity_cannot_route_into_another_live_orchestration() {
    let deck = TuiDeck::builder()
        .with_pty_size(160, 40)
        // Both delegation watches off: no notice may compete with the panes
        // under assertion.
        .with_env("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "0")
        .with_env("DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS", "0")
        .with_env("DAD_TEST_BIN", env!("CARGO_BIN_EXE_dot-agent-deck"))
        // Deliberately NOT `impersonating_pane_signals()`: the shipped policy.
        .launch_with_fixture("stale-pane-identity");
    deck.wait_for_string("No active agents");

    // Tab A, then tab B: the same orchestration in the same directory, two
    // routing groups (PRD #140). Ctrl+N is a global chord, so the second open
    // works from inside the first tab.
    open_orchestration(&deck);
    let first_ready = common::wait_until(Duration::from_secs(30), || {
        let tabs = orchestration_tabs(&deck);
        tabs.len() == 1 && tabs.values().all(|roles| roles.len() == 2)
    });
    assert!(
        first_ready,
        "the first orchestration tab never came up; tabs = {:?}",
        orchestration_tabs(&deck)
    );
    let tab_a_id = orchestration_tabs(&deck)
        .into_keys()
        .next()
        .expect("one tab");
    open_orchestration(&deck);
    let both_ready = common::wait_until(Duration::from_secs(30), || {
        let tabs = orchestration_tabs(&deck);
        tabs.len() == 2 && tabs.values().all(|roles| roles.len() == 2)
    });
    assert!(
        both_ready,
        "the second orchestration tab never came up; tabs = {:?}",
        orchestration_tabs(&deck)
    );
    let tabs = orchestration_tabs(&deck);
    let tab_a = tabs[&tab_a_id].clone();
    let tab_b = tabs
        .iter()
        .find(|(id, _)| **id != tab_a_id)
        .map(|(_, roles)| roles.clone())
        .expect("a second tab with its own orchestration id");
    let (orch_a, worker_a) = (&tab_a["orchestrator"], &tab_a["worker"]);
    let (orch_b, worker_b) = (&tab_b["orchestrator"], &tab_b["worker"]);

    // ---- 1. THE STALE IDENTITY, from inside B's own pane ------------------
    std::fs::write(
        deck.workdir().join(format!("stale-go-{}", orch_b.pane_id)),
        format!(
            "STALE_PANE='{}'\nSTALE_AGENT='{}'\n",
            orch_a.pane_id,
            current_agent_id(&deck, orch_a)
        ),
    )
    .expect("hand B's orchestrator A's identity");
    let stale_done = common::wait_until(Duration::from_secs(30), || {
        squeezed_pty(&deck, orch_b).contains("STALE-DELEGATE-EXIT=")
    });
    let orch_b_pty = squeezed_pty(&deck, orch_b);
    assert!(
        stale_done,
        "the stale-identity delegate never finished in B's pane; B's PTY = {orch_b_pty}"
    );
    assert!(
        !orch_b_pty.contains("STALE-DELEGATE-EXIT=0"),
        "issue #712: a delegate naming A's pane from inside B's pane exited 0 — it was routed \
         into A's orchestration; B's PTY = {orch_b_pty}"
    );
    assert!(
        orch_b_pty.contains("issuedforadifferentpane"),
        "the stale-identity delegate must be told why it was refused; B's PTY = {orch_b_pty}"
    );

    // ---- 2. B'S OWN IDENTITY: the control and the later round trip --------
    std::fs::write(
        deck.workdir().join(format!("own-go-{}", orch_b.pane_id)),
        b"go\n",
    )
    .expect("release B's own delegate");
    let own_landed = common::wait_until(Duration::from_secs(60), || {
        squeezed_pty(&deck, worker_b).contains(WORKER_POINTER)
    });
    let orch_b_pty = squeezed_pty(&deck, orch_b);
    assert!(
        own_landed && orch_b_pty.contains("OWN-DELEGATE-EXIT=0"),
        "control — B's delegate with its own untouched environment must reach B's worker; \
         B's PTY = {orch_b_pty}\nB's worker PTY = {}\nA's worker PTY = {}",
        squeezed_pty(&deck, worker_b),
        squeezed_pty(&deck, worker_a)
    );

    // ---- 3. A'S WORKER NEVER RECEIVED ANYTHING -----------------------------
    let worker_a_pty = squeezed_pty(&deck, worker_a);
    assert!(
        !worker_a_pty.contains(WORKER_POINTER),
        "issue #712: A's worker received a task pointer, but nothing was ever delegated in A's \
         orchestration — the stale identity routed B's work into it; A's worker PTY = \
         {worker_a_pty}"
    );
}
