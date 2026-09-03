#![cfg(feature = "e2e")]

//! L2 end-to-end coverage for the orchestrator remit re-assertion feature: an
//! orchestration's start role has its remit delivered exactly once, as a seed
//! prompt at spawn, and never re-asserted — so role adherence decays with
//! session length, worst exactly when an orchestration has run long enough to
//! compact. This file pins the fix: a `Compacting` event, or a
//! `/clear`-originated `SessionStart`, on the orchestrator start-role pane
//! re-delivers the `.dot-agent-deck/orchestrator-context.md` pointer, scoped
//! to the start role only, through the SAME readiness-gating and delivery-
//! confirmation discipline the spawn-time seed already uses
//! (`deliver_orchestrator_prompt`, `src/ui.rs`).
//!
//! Uses the `remit-reassert-orchestration` fixture (`orchestrator` start
//! role running a synthetic script written by each test into the deck's
//! workdir, `worker` a plain `cat` stub) rather than the shared `orch-deck`/
//! `send-result-orchestration` fixtures — this needs BOTH a role that tees
//! its own stdin to a log file (to count re-deliveries, mirroring
//! `pane_input_007`'s `orchestrator-prompt.log` technique,
//! `tests/e2e_pane_send_result.rs`) AND a script capable of toggling its own
//! declared liveness live -> history-only -> live on cue from the test
//! driver (needed only by `orchestration/remit/003`; `001`/`002`/`004`-`007`
//! simply never trigger that phase).
//!
//! `orchestration/remit/003` deliberately asserts only on the RENDERED GRID
//! feedback string and the delivery-log line count — both pre-existing,
//! stable observables `deliver_orchestrator_prompt` already produces for a
//! `HistoryOnly` `SendResult` today — never on an internal Rust symbol or
//! enum variant, so this file stays correct regardless of internal
//! refactoring of the delivery-confirmation machinery.
//!
//! Gated behind the `e2e` feature so `cargo test-fast` never compiles it.

mod common;

use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::event::{
    AgentEvent, AgentType, CLEAR_SESSION_START_METADATA_KEY, CLEAR_SESSION_START_METADATA_VALUE,
    EventType,
};
use spec::spec;

const DELIVERED_POINTER: &str = "Read .dot-agent-deck/orchestrator-context.md";

/// Ceiling on "the daemon has applied the injected event to its own state".
///
/// Issue #818. This was a bare 10s at both call sites, merely restating the
/// harness-wide `WAIT_TIMEOUT` default (`tests/common/mod.rs`). That is
/// ample locally — `orchestration_remit_001` completes in ~2.2s — and too tight
/// on a GitHub runner, where lane 1 runs the whole 8000-test tier in parallel
/// and `e2e-deterministic` went red on every branch and on `main` within
/// seconds of this file landing. The wait itself was never the problem: it is a
/// bounded `common::wait_until` poll, exactly what Decision 21 asks for.
///
/// 30s rather than a larger round number because the work being waited on is
/// one socket write plus one state application — if that genuinely needs longer,
/// something IS wrong and the test should say so rather than wait it out.
/// Existing e2e tests already bound slower waits at 15s, 20s, 30s, 60s and 75s,
/// so this needs no new mechanism.
///
/// THIS BOUND ALONE DOES NOT FIX #818, and raising it further will not either.
/// At 30s under default parallelism these tests still failed; they pass only
/// with the `orchestration-remit` `max-threads = 1` test group in
/// `.config/nextest.toml` as well. The bound is still load-bearing — serialised
/// by that group but left at 10s, `orchestration_remit_001` alone still blew
/// past it (13.5s) — so the two halves must move together. That file's comment
/// carries the four-way measurement.
const STATE_APPLIED_TIMEOUT: Duration = Duration::from_secs(30);

/// Ceiling on "the fixture's prompt log shows the remit pointer N times".
///
/// Issue #818, same cause as [`STATE_APPLIED_TIMEOUT`] and raised for the same
/// reason: `orchestration_remit_001` was observed failing at BOTH bounds on a
/// runner — at the state wait in one run and at the first delivery wait in
/// another — so raising only one of them just moves the flake.
///
/// Applies to the POSITIVE waits only, the ones asserting a delivery *did*
/// happen. The deliberate short bounds elsewhere in this file
/// (`Duration::from_millis(900)`, always under `assert!(!…)`) assert a delivery
/// did NOT happen; raising those would invert what the test proves while making
/// it slower. Leave them alone.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Drive the new-pane dialog to open the (single) orchestration in the
/// `remit-reassert-orchestration` fixture. With no `[[modes]]` defined the
/// Mode chip row is `[No mode] [Orch: remit-reassert] [schedule]`, so ONE
/// Right selects the orchestration; selecting an orchestration hides the
/// Command field, so a second Enter submits the form. Lands with the
/// orchestrator (start) role focused in `PaneInput` mode. Mirrors
/// `tests/e2e_orchestration_focus.rs::open_orchestration`.
fn open_orchestration(deck: &TuiDeck) {
    deck.send_keys(b"\x0e"); // Ctrl+n -> directory picker
    deck.send_keys(b" "); // Space -> confirm current dir -> new-pane form
    deck.wait_for_string("No mode"); // form up, Mode field focused at "No mode"
    deck.send_keys(b"\x1b[C"); // Right -> [Orch: remit-reassert]
    deck.send_keys(b"\r"); // Mode -> Name
    deck.send_keys(b"\r"); // submit (Command hidden for an orchestration)
}

/// Write `contents` to `path` and mark it executable (unix `0o755`). Mirrors
/// `tests/e2e_pane_send_result.rs::write_executable`.
#[cfg(unix)]
fn write_executable(path: &std::path::Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, contents).expect("write executable test script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod executable test script");
}

/// The synthetic orchestrator role script: a BACKGROUNDED subshell declares
/// the role live immediately (fast-path readiness for the spawn-time remit
/// pointer) and — only if the test writes the corresponding control file
/// into the workdir — later declares history-only and then live again;
/// meanwhile the FOREGROUND script body reads and logs every line delivered
/// to its real stdin, however many arrive, to `orchestrator-prompt.log`, and
/// reports each one back over the hook socket as a `user_prompt`-carrying
/// `thinking` event — the evidence `deliver_orchestrator_prompt`'s
/// submission-confirmation logic accepts as CONFIRMATION that a written
/// prompt was actually submitted rather than merely landed on the PTY.
/// Without that confirmation every delivery against this fixture — the
/// spawn-time seed included — is stuck permanently unconfirmed.
///
/// The background/foreground split is load-bearing, not stylistic: a
/// non-interactive POSIX shell reassigns an ASYNCHRONOUS (`&`) job's stdin to
/// `/dev/null` unless that job never touches stdin at all, so a version of
/// this script that ran `cat >> orchestrator-prompt.log &` in the background
/// would silently read nothing — the delivered pointer would land on the
/// real PTY (visible on the rendered grid) but never reach the log,
/// producing a false RED against this file's own precondition assertion
/// instead of the feature under test. The `emit_target` and
/// `confirm_submission` subshells below never read stdin, so backgrounding
/// or forking them is unaffected; the `read` loop stays in the foreground, so
/// it keeps the real PTY stdin. Mirrors the `emit_target` helper
/// `tests/e2e_pane_send_result.rs::pane_input_007_orchestrator_prompt_retries_after_non_applied_result`
/// uses for the identical raw hook-socket `session_start` technique.
///
/// **Timing hazard**: this file's log-count assertions on [`DELIVERED_POINTER`]
/// are only stable while `confirm_submission` completes inside
/// `unconfirmed_retry_delay(1)` — 500ms (`src/prompt_delivery.rs`) — of the
/// initial write. `MAX_PAYLOAD_SUBMISSIONS` there is 2, so a delivery still
/// unconfirmed past that window earns one automatic *replacement* payload
/// write, appending a second `DELIVERED_POINTER` line to the log with no real
/// re-assertion behind it — which would read as a false GREEN on any test
/// here asserting a count of 2. The forked `python3` `confirm_submission`
/// round-trip normally beats the window comfortably, but nothing enforces
/// that margin; if these tests start flaking under load, check here first.
const ORCHESTRATOR_REMIT_SCRIPT: &str = r#"#!/bin/sh
emit_target() {
    WRITABLE="$1" python3 - <<'PY'
import datetime
import json
import os
import socket

pane = os.environ["DOT_AGENT_DECK_PANE_ID"]
payload = {
    "session_id": "remit-reassert-boot-session",
    "agent_type": "codex",
    "event_type": "session_start",
    "timestamp": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "pane_id": pane,
    "agent_id": os.environ.get("DOT_AGENT_DECK_AGENT_ID"),
    "live_target": {
        "kind": "pty" if os.environ["WRITABLE"] == "live" else "process",
        "writable": os.environ["WRITABLE"],
    },
}
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(os.environ["DOT_AGENT_DECK_SOCKET"])
s.sendall((json.dumps(payload) + "\n").encode())
s.close()
PY
}

confirm_submission() {
    SUBMITTED="$1" python3 - <<'PY'
import datetime
import json
import os
import socket

pane = os.environ["DOT_AGENT_DECK_PANE_ID"]
payload = {
    "session_id": "remit-reassert-boot-session",
    "agent_type": "codex",
    "event_type": "thinking",
    "timestamp": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "pane_id": pane,
    "agent_id": os.environ.get("DOT_AGENT_DECK_AGENT_ID"),
    "user_prompt": os.environ["SUBMITTED"],
}
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(os.environ["DOT_AGENT_DECK_SOCKET"])
s.sendall((json.dumps(payload) + "\n").encode())
s.close()
PY
}

(
    emit_target live
    touch initial-live-emitted

    while [ ! -f go-history-only ]; do sleep 0.05; done
    emit_target history-only
    touch history-only-emitted

    while [ ! -f go-live-again ]; do sleep 0.05; done
    emit_target live
    touch relive-emitted
) &

while IFS= read -r line; do
    printf '%s\n' "$line" >> orchestrator-prompt.log
    confirm_submission "$line"
done
"#;

/// The fixture's full daemon registry record for `role`. Mirrors
/// `tests/e2e_orchestration_focus.rs::role_agent_record`.
fn role_agent_record(
    socket: &std::path::Path,
    role: &str,
) -> dot_agent_deck::agent_pty::AgentRecord {
    common::agent_records_on(socket)
        .into_iter()
        .find(|r| {
            matches!(
                &r.tab_membership,
                Some(dot_agent_deck::agent_pty::TabMembership::Orchestration { role_name, .. })
                    if role_name == role
            )
        })
        .unwrap_or_else(|| {
            panic!("remit-reassert-orchestration fixture's {role} role pane must be registered with the daemon")
        })
}

/// Inject a synthetic `Compacting` `AgentEvent` for the given pane/agent
/// identity over the deck's hook socket, and block until the daemon's own
/// `ListAgents`/live-status join reports `SessionStatus::Compacting` for
/// that pane — proof the daemon's state (not just the wire) reflects the
/// change before the caller starts asserting on anything driven by it.
/// Mirrors `tests/e2e_orchestration_focus.rs::inject_role_status`,
/// specialized to the one event type this file needs.
#[cfg(unix)]
fn inject_compacting(
    deck: &TuiDeck,
    socket: &std::path::Path,
    pane_id: &str,
    agent_id: &str,
    session_id: &str,
) {
    let event = AgentEvent {
        session_id: session_id.to_string(),
        agent_type: AgentType::Codex,
        event_type: EventType::Compacting,
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
    let line = serde_json::to_string(&event).expect("serialize synthetic Compacting AgentEvent");
    common::write_hook_line(deck.hook_socket_path(), &line)
        .expect("inject synthetic Compacting AgentEvent over hook socket");

    let applied = common::wait_until(STATE_APPLIED_TIMEOUT, || {
        common::agent_records_on(socket).into_iter().any(|r| {
            r.pane_id_env.as_deref() == Some(pane_id)
                && r.live.as_ref().map(|s| &s.status)
                    == Some(&dot_agent_deck::state::SessionStatus::Compacting)
        })
    });
    assert!(
        applied,
        "the daemon's own ListAgents/live-status join never reported Compacting \
         for pane {pane_id} (agent_id {agent_id}) within {STATE_APPLIED_TIMEOUT:?}. \
         The hook socket write was accepted. On a loaded CI runner suspect the \
         bound before the code (issue #818); if this reproduces locally, where the \
         whole test takes ~2s, then AppState::apply_event really is rejecting the \
         event or applying it to the wrong session.",
    );
}

/// Inject a synthetic `/clear`-shaped `SessionStart` `AgentEvent` for the
/// given pane/agent identity over the deck's hook socket, and block until the
/// daemon's own `ListAgents`/live-status join reports `SessionStatus::Idle`
/// for that pane — proof the daemon's state (not just the wire) reflects the
/// change before the caller starts asserting on anything driven by it.
/// Mirrors [`inject_compacting`] above, specialized to the `/clear` trigger:
/// `AppState::apply_event`'s `EventType::SessionStart` arm unconditionally
/// sets `session.status = SessionStatus::Idle` (`src/state.rs`), which is the
/// one observable, already-existing state transition available to poll on —
/// there is no persisted "clear was observed" status the way `Compacting` is
/// a status in its own right, since a `SessionStart` is a point-in-time
/// event. The event carries no `live_target`, exactly like
/// `inject_compacting` above, and the same-agent reuse guard in
/// `AppState::apply_event` keeps this update on the SAME session card (keyed
/// by pane/agent identity, not by `session_id`) rather than spawning a second
/// one, so a differing synthetic `session_id` per call is safe here too.
///
/// The `metadata` map carries [`CLEAR_SESSION_START_METADATA_KEY`] /
/// [`CLEAR_SESSION_START_METADATA_VALUE`] (`dot_agent_deck::event`) — the
/// real production constants `build_event_typed` (`src/hook.rs`) forwards
/// `ClaudeCodeHookInput.source == "clear"` into.
///
/// `agent_type` is a caller-supplied parameter, not a hardcoded value: the
/// re-assertion feature's `/clear` trigger is Claude Code only, so
/// `orchestration_remit_004`/`_005` pass [`AgentType::ClaudeCode`] to
/// exercise the trigger within scope, while `orchestration_remit_006`
/// deliberately passes a non-Claude `agent_type` to prove events outside
/// that scope are ignored.
#[cfg(unix)]
fn inject_clear_session_start(
    deck: &TuiDeck,
    socket: &std::path::Path,
    pane_id: &str,
    agent_id: &str,
    session_id: &str,
    agent_type: AgentType,
) {
    let mut metadata = std::collections::HashMap::new();
    metadata.insert(
        CLEAR_SESSION_START_METADATA_KEY.to_string(),
        CLEAR_SESSION_START_METADATA_VALUE.to_string(),
    );
    let event = AgentEvent {
        session_id: session_id.to_string(),
        agent_type,
        event_type: EventType::SessionStart,
        tool_name: None,
        tool_detail: None,
        cwd: None,
        timestamp: chrono::Utc::now(),
        user_prompt: None,
        metadata,
        pane_id: Some(pane_id.to_string()),
        agent_id: Some(agent_id.to_string()),
        agent_version: None,
        schema_version: None,
        live_target: None,
    };
    let line = serde_json::to_string(&event)
        .expect("serialize synthetic clear-originated SessionStart AgentEvent");
    common::write_hook_line(deck.hook_socket_path(), &line)
        .expect("inject synthetic clear-originated SessionStart AgentEvent over hook socket");

    let applied = common::wait_until(STATE_APPLIED_TIMEOUT, || {
        common::agent_records_on(socket).into_iter().any(|r| {
            r.pane_id_env.as_deref() == Some(pane_id)
                && r.live.as_ref().map(|s| &s.status)
                    == Some(&dot_agent_deck::state::SessionStatus::Idle)
        })
    });
    assert!(
        applied,
        "the daemon's own ListAgents/live-status join never reported Idle for pane \
         {pane_id} (agent_id {agent_id}) within {STATE_APPLIED_TIMEOUT:?} after \
         injecting a synthetic clear-originated SessionStart. The hook socket write \
         was accepted. On a loaded CI runner suspect the bound before the code \
         (issue #818); if this reproduces locally then AppState::apply_event really \
         is rejecting the event or applying it to the wrong session.",
    );
}

/// Open the orchestration, write and launch the orchestrator's synthetic
/// script, and confirm the spawn-time remit pointer lands once. Returns the
/// daemon socket path, the start role's `(pane_id, agent_id)`, and the log
/// path every test in this file asserts delivery counts against — both the
/// script and the log live directly under `deck.workdir()`, the directory
/// the orchestrator role pane actually runs in.
fn open_and_confirm_initial_delivery(
    deck: &TuiDeck,
) -> (std::path::PathBuf, String, String, std::path::PathBuf) {
    deck.wait_for_string("No active sessions");
    write_executable(
        &deck.workdir().join("orchestrator-remit.sh"),
        ORCHESTRATOR_REMIT_SCRIPT,
    );

    open_orchestration(deck);
    deck.wait_for_absence("New Agent");

    let socket = deck.attach_socket_path().to_path_buf();
    let record = role_agent_record(&socket, "orchestrator");
    let pane_id = record
        .pane_id_env
        .clone()
        .expect("orchestrator role pane must have a DOT_AGENT_DECK_PANE_ID recorded");
    let agent_id = record.id.clone();

    let log = deck.workdir().join("orchestrator-prompt.log");
    let initial_delivered =
        common::wait_for_file_substr_count(&log, DELIVERED_POINTER, 1, DELIVERY_TIMEOUT);
    assert!(
        initial_delivered,
        "precondition failed: the spawn-time orchestrator prompt never reached the \
         start role's pane within {DELIVERY_TIMEOUT:?}\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    (socket, pane_id, agent_id, log)
}

/// Scenario: Open a real orchestration tab and let the start role's
/// spawn-time remit pointer deliver once, then inject a `Compacting` event
/// for that SAME start-role pane. The pointer must reach the pane's stdin a
/// second time — the orchestrator's remit re-asserting itself on compaction,
/// rather than only ever being delivered at spawn.
#[spec("orchestration/remit/001")]
#[test]
#[cfg(unix)]
fn orchestration_remit_001_start_role_compaction_reasserts_remit() {
    let deck = TuiDeck::launch_with_fixture("remit-reassert-orchestration");
    let (socket, pane_id, agent_id, log) = open_and_confirm_initial_delivery(&deck);

    inject_compacting(
        &deck,
        &socket,
        &pane_id,
        &agent_id,
        &format!("{agent_id}-remit001-session"),
    );

    let reasserted =
        common::wait_for_file_substr_count(&log, DELIVERED_POINTER, 2, DELIVERY_TIMEOUT);
    assert!(
        reasserted,
        "a Compacting event on the orchestrator start-role pane must re-deliver the \
         `{DELIVERED_POINTER}` remit pointer a second time; the log only shows it \
         once within {DELIVERY_TIMEOUT:?}.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: In the same orchestration, `Compacting` fires first on the
/// non-start `worker` role's pane — this must NOT re-deliver the remit
/// pointer to the start role. Then, as a positive control proving this is a
/// genuine scoping guard and not just an unimplemented feature vacuously
/// passing the negative check, `Compacting` fires on the orchestrator start
/// role itself, which MUST re-deliver. The guard against re-assertion
/// leaking into every pane of an orchestration (the settled scope: the
/// orchestrator start role only).
#[spec("orchestration/remit/002")]
#[test]
#[cfg(unix)]
fn orchestration_remit_002_non_start_role_compaction_reasserts_nothing() {
    let deck = TuiDeck::launch_with_fixture("remit-reassert-orchestration");
    let (socket, orch_pane_id, orch_agent_id, log) = open_and_confirm_initial_delivery(&deck);

    let worker_record = role_agent_record(&socket, "worker");
    let worker_pane_id = worker_record
        .pane_id_env
        .clone()
        .expect("worker role pane must have a DOT_AGENT_DECK_PANE_ID recorded");
    let worker_agent_id = worker_record.id.clone();

    inject_compacting(
        &deck,
        &socket,
        &worker_pane_id,
        &worker_agent_id,
        &format!("{worker_agent_id}-remit002-worker-session"),
    );

    let leaked_to_worker =
        common::wait_for_file_substr_count(&log, DELIVERED_POINTER, 2, Duration::from_millis(900));
    assert!(
        !leaked_to_worker,
        "a Compacting event on the non-start `worker` role's pane must not re-assert \
         the orchestrator's remit; the start role's delivery log reached a second \
         `{DELIVERED_POINTER}` line anyway.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    inject_compacting(
        &deck,
        &socket,
        &orch_pane_id,
        &orch_agent_id,
        &format!("{orch_agent_id}-remit002-orch-session"),
    );
    let reasserted_on_start_role =
        common::wait_for_file_substr_count(&log, DELIVERED_POINTER, 2, DELIVERY_TIMEOUT);
    assert!(
        reasserted_on_start_role,
        "control failed: a Compacting event on the orchestrator START role must still \
         re-deliver the remit pointer in this same orchestration — the negative check \
         above is only meaningful if this positive control also passes.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: `Compacting` fires on the start role while its pane currently
/// declares itself history-only (not writable). The re-assertion must NOT
/// write blindly — the pointer must stay undelivered, with the same
/// `History-only session cannot accept live input` feedback the spawn-time
/// seed already surfaces for a non-applied `SendResult`, until the SAME pane
/// later declares itself live again, at which point the deferred
/// re-assertion must complete. Proves re-assertion goes through the seed's
/// own readiness-gating and delivery-confirmation discipline rather than a
/// direct, unconfirmed write.
#[spec("orchestration/remit/003")]
#[test]
#[cfg(unix)]
fn orchestration_remit_003_reassertion_waits_for_confirmed_delivery() {
    let deck = TuiDeck::launch_with_fixture("remit-reassert-orchestration");
    let (socket, pane_id, agent_id, log) = open_and_confirm_initial_delivery(&deck);

    std::fs::write(deck.workdir().join("go-history-only"), "")
        .expect("trigger the fixture script's history-only phase");
    assert!(
        common::wait_until(Duration::from_secs(5), || {
            deck.workdir().join("history-only-emitted").exists()
        }),
        "the fixture script never emitted its history-only session_start within 5s"
    );

    // Reuse the fixture's own boot session id here, unlike `_001`/`_002`'s
    // synthetic per-call ids: a real `Compacting` hook carries the agent's
    // own session id (its `PreCompact` originates from that agent's own
    // process), so a differing synthetic id models an event shape that does
    // not occur in production. `_003` is the only test in this file whose
    // flow (history-only -> live-again) makes the resulting id-overwrite
    // observable, which is why only this call needs the boot id.
    inject_compacting(
        &deck,
        &socket,
        &pane_id,
        &agent_id,
        "remit-reassert-boot-session",
    );

    let wrote_blindly =
        common::wait_for_file_substr_count(&log, DELIVERED_POINTER, 2, Duration::from_millis(900));
    assert!(
        !wrote_blindly,
        "a Compacting-triggered re-assertion must not write to a history-only pane \
         before delivery is confirmed; the pointer reached the log a second time while \
         the pane was still history-only.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    let feedback = deck.wait_for_grid_string_within(
        "History-only session cannot accept live input",
        Duration::from_secs(5),
    );

    std::fs::write(deck.workdir().join("go-live-again"), "")
        .expect("trigger the fixture script's return-to-live phase");
    let delivered_once_live =
        common::wait_for_file_substr_count(&log, DELIVERED_POINTER, 2, DELIVERY_TIMEOUT);

    assert!(
        feedback,
        "a deferred re-assertion attempt against a history-only pane must surface the \
         same visible feedback the spawn-time seed uses for a non-applied SendResult\n\
         Final grid:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        delivered_once_live,
        "once the start-role pane reports itself live again, the deferred \
         re-assertion must complete and deliver the pointer a second time\n\
         Final grid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Open a real orchestration tab and let the start role's
/// spawn-time remit pointer deliver once, then inject a synthetic
/// `SessionStart` for that SAME start-role pane carrying the
/// `/clear`-originated marker (`CLEAR_SESSION_START_METADATA_KEY` /
/// `CLEAR_SESSION_START_METADATA_VALUE`). The pointer must reach the pane's
/// stdin a second time — the orchestrator's remit re-asserting itself on
/// `/clear`, exactly as it already re-asserts on compaction, via the same
/// reused delivery machinery.
#[spec("orchestration/remit/004")]
#[test]
#[cfg(unix)]
fn orchestration_remit_004_start_role_clear_reasserts_remit() {
    let deck = TuiDeck::launch_with_fixture("remit-reassert-orchestration");
    let (socket, pane_id, agent_id, log) = open_and_confirm_initial_delivery(&deck);

    inject_clear_session_start(
        &deck,
        &socket,
        &pane_id,
        &agent_id,
        &format!("{agent_id}-remit004-session"),
        AgentType::ClaudeCode,
    );

    let reasserted =
        common::wait_for_file_substr_count(&log, DELIVERED_POINTER, 2, DELIVERY_TIMEOUT);
    assert!(
        reasserted,
        "a `/clear`-originated SessionStart event on the orchestrator start-role pane \
         must re-deliver the `{DELIVERED_POINTER}` remit pointer a second time; the \
         log only shows it once within {DELIVERY_TIMEOUT:?}.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    // Pin non-repetition, not just arrival — a single `/clear`-originated
    // `SessionStart` event must deliver the pointer exactly once more, never
    // repeatedly. Mirrors the "stays put" shape `orchestration_remit_002`/
    // `_005` already use for their negative leak checks
    // (`!wait_for_file_substr_count(..., short bound)`), applied here to the
    // count staying AT 2 rather than never reaching 2.
    let repeated_beyond_two =
        common::wait_for_file_substr_count(&log, DELIVERED_POINTER, 3, Duration::from_millis(900));
    assert!(
        !repeated_beyond_two,
        "a single `/clear`-originated SessionStart event must not re-deliver the remit \
         pointer more than once; the log reached a third `{DELIVERED_POINTER}` line \
         within a bounded wait after the second.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: In the same orchestration, a `/clear`-originated `SessionStart`
/// fires first on the non-start `worker` role's pane — this must NOT
/// re-deliver the remit pointer to the start role. Then, as a positive
/// control proving this is a genuine scoping guard and not just an
/// unimplemented feature vacuously passing the negative check, the same
/// `/clear`-originated `SessionStart` fires on the orchestrator start role
/// itself, which MUST re-deliver. Mirrors `orchestration_remit_002`'s exact
/// pattern for the compaction trigger, extended to the `/clear` trigger: the
/// guard against re-assertion leaking into every pane of an orchestration
/// applies identically to both triggers.
#[spec("orchestration/remit/005")]
#[test]
#[cfg(unix)]
fn orchestration_remit_005_non_start_role_clear_reasserts_nothing() {
    let deck = TuiDeck::launch_with_fixture("remit-reassert-orchestration");
    let (socket, orch_pane_id, orch_agent_id, log) = open_and_confirm_initial_delivery(&deck);

    let worker_record = role_agent_record(&socket, "worker");
    let worker_pane_id = worker_record
        .pane_id_env
        .clone()
        .expect("worker role pane must have a DOT_AGENT_DECK_PANE_ID recorded");
    let worker_agent_id = worker_record.id.clone();

    inject_clear_session_start(
        &deck,
        &socket,
        &worker_pane_id,
        &worker_agent_id,
        &format!("{worker_agent_id}-remit005-worker-session"),
        AgentType::ClaudeCode,
    );

    let leaked_to_worker =
        common::wait_for_file_substr_count(&log, DELIVERED_POINTER, 2, Duration::from_millis(900));
    assert!(
        !leaked_to_worker,
        "a `/clear`-originated SessionStart event on the non-start `worker` role's pane \
         must not re-assert the orchestrator's remit; the start role's delivery log \
         reached a second `{DELIVERED_POINTER}` line anyway.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    inject_clear_session_start(
        &deck,
        &socket,
        &orch_pane_id,
        &orch_agent_id,
        &format!("{orch_agent_id}-remit005-orch-session"),
        AgentType::ClaudeCode,
    );
    let reasserted_on_start_role =
        common::wait_for_file_substr_count(&log, DELIVERED_POINTER, 2, DELIVERY_TIMEOUT);
    assert!(
        reasserted_on_start_role,
        "control failed: a `/clear`-originated SessionStart event on the orchestrator \
         START role must still re-deliver the remit pointer in this same orchestration \
         — the negative check above is only meaningful if this positive control also \
         passes.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: In the same orchestration, a `/clear`-originated `SessionStart`
/// fires on the orchestrator START role's own pane, but stamped with a
/// non-Claude-Code `agent_type` — this must NOT re-deliver the remit
/// pointer, since the `/clear` trigger's scope is Claude Code only
/// (`AgentType::ClaudeCode`). Deliberately negative-only: unlike
/// `orchestration_remit_002`/`_005`, this test does not chase the negative
/// check with a same-pane positive-control injection, because applying a
/// second `SessionStart` to the SAME pane — even one this guard correctly
/// filters from re-arming — legitimately advances `pane_hook_session`
/// (`src/state.rs`), the bookkeeping `delivery_target_changed` (`src/ui.rs`)
/// compares against, and reads the pane as a stale delivery
/// target after two hops, an artifact of the test's own two-hop injection
/// shape rather than anything a real pane (whose `agent_type` is fixed for
/// its whole life) can ever encounter. The "is this harness capable of
/// proving a positive case at all" concern a positive control exists to rule
/// out is already covered independently by
/// `orchestration_remit_004_start_role_clear_reasserts_remit`, a genuine
/// single-hop injection on this same pane shape proving the trigger fires —
/// the same relationship `_001`'s positive proof bears to `_002`'s negative
/// check on a different pane, applied here across the agent-type axis
/// instead of the pane-identity axis.
#[spec("orchestration/remit/006")]
#[test]
#[cfg(unix)]
fn orchestration_remit_006_non_claude_agent_type_clear_reasserts_nothing() {
    let deck = TuiDeck::launch_with_fixture("remit-reassert-orchestration");
    let (socket, pane_id, agent_id, log) = open_and_confirm_initial_delivery(&deck);

    inject_clear_session_start(
        &deck,
        &socket,
        &pane_id,
        &agent_id,
        &format!("{agent_id}-remit006-non-claude-session"),
        AgentType::Codex,
    );

    let leaked_for_non_claude_agent_type =
        common::wait_for_file_substr_count(&log, DELIVERED_POINTER, 2, Duration::from_millis(900));
    assert!(
        !leaked_for_non_claude_agent_type,
        "a `/clear`-originated SessionStart event stamped with a non-Claude-Code \
         `agent_type` must not re-assert the orchestrator's remit (the trigger's stated \
         scope is Claude Code only); the start role's delivery log reached a second \
         `{DELIVERED_POINTER}` line anyway.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}

/// The closing sentence `prepare_orchestrator_prompt` (`src/orchestrator_context.rs`)
/// emits only when a task is present — its ABSENCE from a re-assertion is what
/// `007` below exists to catch.
const CARRY_OUT_TASK_POINTER: &str = "Then carry out that task";

/// Scenario: Regression for the maintainer review on the fork's upstream PR
/// #789 ("Required 1"). Before this fix, both re-arm sites called
/// `prepare_orchestrator_prompt(config, cwd, None)` directly, which
/// unconditionally rewrote `.dot-agent-deck/orchestrator-context.md` with no
/// `## Your task` section and delivered the no-task "wait for instructions"
/// pointer — silently deleting a dispatched orchestration's task from disk on
/// every compaction, on exactly the long-running `dispatch --task` path this
/// feature exists to serve. This test seeds the context file with a `## Your
/// task` section the way `src/spawn.rs`'s dispatch path does at spawn, then
/// confirms a compaction re-assertion both re-delivers the TASK-CARRYING
/// pointer variant (not the wait-for-instructions one) and leaves the task
/// itself intact on disk.
#[spec("orchestration/remit/007")]
#[test]
#[cfg(unix)]
fn orchestration_remit_007_compaction_reassertion_preserves_a_dispatched_task() {
    let deck = TuiDeck::launch_with_fixture("remit-reassert-orchestration");
    let (socket, pane_id, agent_id, log) = open_and_confirm_initial_delivery(&deck);

    // Seed a `## Your task` section onto the context file the interactive
    // spawn path (`open_orchestration`) just wrote with none — reproducing,
    // byte-for-byte, the shape `prepare_orchestrator_prompt(config, cwd,
    // Some(task))` leaves on disk for a `dispatch --task` orchestration
    // (`src/spawn.rs`), without needing a second, separately-launched fixture
    // for the daemon dispatch path.
    const TASK_SENTINEL: &str = "SENTINEL-TASK-remit007: verify PR #500 and report.";
    let context_path = deck
        .workdir()
        .join(".dot-agent-deck")
        .join("orchestrator-context.md");
    let mut seeded =
        std::fs::read_to_string(&context_path).expect("read the spawn-written context file");
    seeded.push_str("\n## Your task\n\n");
    seeded.push_str(TASK_SENTINEL);
    seeded.push('\n');
    std::fs::write(&context_path, &seeded).expect("seed a dispatched task onto the context file");

    inject_compacting(
        &deck,
        &socket,
        &pane_id,
        &agent_id,
        &format!("{agent_id}-remit007-session"),
    );

    let reasserted_with_task =
        common::wait_for_file_substr_count(&log, CARRY_OUT_TASK_POINTER, 1, DELIVERY_TIMEOUT);
    assert!(
        reasserted_with_task,
        "a compaction re-assertion on a start role whose context file carries a `## Your \
         task` section must re-deliver the TASK-CARRYING pointer (containing \
         `{CARRY_OUT_TASK_POINTER}`), not the no-task \"wait for instructions\" variant; \
         the log never shows it within {DELIVERY_TIMEOUT:?}.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    let after_reassert = std::fs::read_to_string(&context_path)
        .expect("read the context file after the re-assertion rewrite");
    assert!(
        after_reassert.contains(TASK_SENTINEL),
        "the dispatched task must survive a compaction re-assertion rather than being wiped \
         by the no-task rewrite; context file after re-assertion:\n{after_reassert}"
    );
}
