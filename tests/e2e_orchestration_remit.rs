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
    EventType, Writable,
};
use spec::spec;

const DELIVERED_POINTER: &str = "Read .dot-agent-deck/orchestrator-context.md";

/// How long the spawn-time remit pointer has to reach the start role's pane.
///
/// This covers a whole cold start — a two-role orchestration spawn, both
/// PTYs, the fixture script's own `python3` readiness emit, the deck's
/// readiness gate and the PTY write — rather than one round trip, which is
/// why it sits well above the harness-wide 10s `WAIT_TIMEOUT`
/// (`tests/common/mod.rs`) this call site used to merely restate. The 10s
/// version timed out on `main` in CI on a runner executing 8107 tests in
/// parallel (issue #832; `e2e-deterministic` run 33571127245,
/// `orchestration_remit_007`) while the same wait needs ~2s on an idle
/// 16-core box, so the budget — unlike the barrier below — genuinely was
/// too small for the machine rather than racing anything.
const SPAWN_DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a RE-ASSERTION's delivery has to reach the start role's pane —
/// the positive `wait_for_file_substr_count` bounds, the ones asserting a
/// delivery *did* happen.
///
/// Carried forward from issue #818's containment round, which raised every
/// positive wait in this file from a bare 10s (a restatement of the
/// harness-wide `WAIT_TIMEOUT`, `tests/common/mod.rs`) to 30s after
/// `orchestration_remit_001` was observed on a runner failing at BOTH this
/// bound and [`SPAWN_DELIVERY_TIMEOUT`] in different runs. That bound is kept
/// rather than reverted: this measures a real PTY write plus the fixture's
/// own read, so a loaded runner genuinely can be slow here, and widening a
/// POSITIVE wait costs only failure latency.
///
/// The deliberate short bounds elsewhere in this file
/// (`Duration::from_millis(900)`, always under `assert!(!...)`) assert a
/// delivery did NOT happen; raising those would invert what the test proves
/// while making it slower. Leave them alone.
const REASSERTION_DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the fixture's delivery confirmation has to reach the daemon's
/// APPLIED state. Paid on the fixture's `python3` fork, so it is a process
/// start under whatever load the tier is carrying, not an IPC round trip.
const CONFIRMATION_APPLIED_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an event THIS FILE has already written to the hook socket has to
/// show up in the daemon's applied state: one `ListAgents` round trip plus
/// the daemon's own apply. Deliberately left at the harness-wide 10s default
/// rather than widened — with [`wait_for_applied`]'s barriers in place
/// nothing else is competing for the status by then, so a timeout here is
/// evidence of a real defect rather than of a slow runner, and widening it
/// would only delay that report.
///
/// This is the ONE bound issue #818's containment round raised (to 30s, as
/// `STATE_APPLIED_TIMEOUT`) that this file does not carry forward, so read it
/// as a deliberate resolution rather than a lost merge. The containment was
/// measured against code where a still-in-flight confirmation could overwrite
/// the injected status permanently — a failure no budget recovers from, which
/// is why 30s failed there too. The barriers below remove that competitor, and
/// the branch's own `e2e-deterministic` run was green at 10s under full runner
/// parallelism. Every POSITIVE delivery wait in this file does keep the
/// widened bound ([`REASSERTION_DELIVERY_TIMEOUT`]), because those measure
/// real work rather than a single apply.
const INJECTED_EVENT_APPLIED_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the delivery log's pointer count must hold steady before this
/// file trusts it as the baseline every later assertion counts from. Must
/// exceed the deck's own `unconfirmed_retry_delay(1)` — 500ms
/// (`src/prompt_delivery.rs`) — since that is the window in which an
/// unconfirmed delivery earns its one automatic replacement payload write.
const DELIVERY_SETTLE_QUIET_WINDOW: Duration = Duration::from_millis(1500);

/// Ceiling on how long [`settled_pointer_count`] will wait for the count to
/// stop moving. Generous because the thing it is waiting out is a `python3`
/// fork on a loaded machine, not an IPC round trip.
const DELIVERY_SETTLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The most [`DELIVERED_POINTER`] lines ONE logical delivery can legitimately
/// leave in the log, and therefore the ceiling [`settled_pointer_count`]
/// refuses to settle above.
///
/// Three, from `src/prompt_delivery.rs`: `MAX_PAYLOAD_SUBMISSIONS` is 2 — the
/// write itself plus the one bounded REPLACEMENT payload — and issue #666's
/// `AgentStartRearm` allows ONE further payload write on positive evidence the
/// payload was destroyed, which that module documents as a hard cap of three.
/// Every attempt past that falls back to a submit-only probe, which writes a
/// bare CR the fixture logs as an empty line rather than as a pointer, so it
/// cannot inflate this count.
///
/// Greptile P2 on PR #837, first half. A settled count with no ceiling accepts
/// whatever it happens to observe, so a caller that meant "one more delivery"
/// silently accepts five. Bounding it does NOT on its own pin exactly-once —
/// a count cannot separate one delivery that retried from two that did not,
/// which is what [`ContextRewriteWatcher`] exists for.
const MAX_POINTER_LINES_PER_DELIVERY: usize = 3;

/// The minimum gap [`ContextRewriteWatcher`] requires between two observed
/// modification times before it counts them as two SEPARATE rewrites.
///
/// `prepare_orchestrator_prompt` writes the context file with
/// `std::fs::write`, which truncates and then writes, so ONE logical rewrite
/// can bump the mtime twice microseconds apart. Two genuine re-arms cannot be
/// anywhere near that close: the second cannot start until the first
/// delivery has actually written its payload (`PromptDelivery::attempts > 0`,
/// `src/ui.rs`), which puts a whole readiness gate and PTY write between them.
/// Measured against the deliberate double-fire mutation used to verify this
/// detector: **~680ms** apart, across three runs (679ms, 688ms, 688ms).
const CONTEXT_REWRITE_DEBOUNCE: Duration = Duration::from_millis(8);

/// Ceiling on how long [`ContextRewriteWatcher`]'s sampling thread stays
/// alive. Comfortably above the sum of every bound the watched region can
/// spend ([`REASSERTION_DELIVERY_TIMEOUT`] + [`DELIVERY_SETTLE_TIMEOUT`] +
/// the 900ms hold), because a watcher that times out early would silently
/// UNDER-count. [`ContextRewriteWatcher::stop`] asserts it did not.
const CONTEXT_WATCH_MAX: Duration = Duration::from_secs(180);

/// Block until the daemon's own `ListAgents`/live-status join reports `pred`
/// for `pane_id`'s live session, or `timeout` elapses; returns whether it
/// did.
///
/// Every barrier in this file goes through here rather than through one of
/// the fixture script's marker files, and the difference is load-bearing.
/// A marker file (`initial-live-emitted`, `history-only-emitted`) proves only
/// that a hook event was *written to the socket* — and the daemon hands each
/// hook connection to its own `tokio::spawn` (`run_hook_loop`,
/// `src/daemon.rs`), so "sent" carries no ordering guarantee whatsoever
/// against an event this file writes afterwards on a different connection.
/// The daemon's own applied state is what this file polls instead, since that
/// is the thing every assertion here is downstream of anyway.
#[cfg(unix)]
fn wait_for_applied(
    socket: &std::path::Path,
    pane_id: &str,
    timeout: Duration,
    pred: impl Fn(&dot_agent_deck::state::SessionSnapshot) -> bool,
) -> bool {
    common::wait_until(timeout, || {
        common::agent_records_on(socket).into_iter().any(|r| {
            r.pane_id_env.as_deref() == Some(pane_id) && r.live.as_ref().is_some_and(&pred)
        })
    })
}

/// The number of [`DELIVERED_POINTER`] lines in the delivery log once that
/// count has stopped moving for [`DELIVERY_SETTLE_QUIET_WINDOW`] — the
/// BASELINE every later count assertion in this file measures from, in place
/// of the literal `2`/`3` they used to hardcode.
///
/// Hardcoding those literals assumed the spawn-time delivery had produced
/// exactly ONE pointer line, which holds only while the fixture's
/// `confirm_submission` beats the deck's 500ms `unconfirmed_retry_delay(1)`.
/// Under load it does not: the deck then writes its one automatic
/// REPLACEMENT payload (`MAX_PAYLOAD_SUBMISSIONS` is 2,
/// `src/prompt_delivery.rs`) and the fixture logs that as a second pointer
/// line with no re-assertion behind it. So every negative check here ("must
/// NOT reach a second line") reddens on a payload retry that is designed
/// behaviour rather than on the leak it exists to catch, and every positive
/// check greens without a re-assertion having happened. Measured on this
/// branch under a 64-way CPU load: 8 such failures across 6 module runs,
/// spread over `_002`, `_003`, `_004`, `_005` and `_006` — a different
/// failure class from the status clobber issues #818/#832 were, and reachable
/// at a higher load than the CI runner has so far shown.
///
/// Counting from a settled baseline makes each assertion measure what it says
/// it measures — one more pointer line than the spawn-time delivery left
/// behind, whatever that delivery cost — instead of asserting a literal that
/// silently encodes "and the retry did not fire".
///
/// `ceiling` is the largest count this call will accept as settled, and it is
/// the answer to Greptile P2's first half on PR #837: without it a caller
/// takes whatever count happens to hold still and hands it on as the baseline
/// for a check that then measures from a figure it never inspected. Every
/// caller knows how many deliveries are legitimately behind the count it is
/// settling — one — so every caller can name
/// [`MAX_POINTER_LINES_PER_DELIVERY`] more than it started from. A count that
/// blows past that is a defect regardless of what happens next, and saying so
/// here reports it against the delivery that caused it rather than as a
/// confusing off-by-N in the assertion behind it.
#[cfg(unix)]
fn settled_pointer_count(deck: &TuiDeck, log: &std::path::Path, ceiling: usize) -> usize {
    let last = std::cell::Cell::new(usize::MAX);
    let stable_since = std::cell::Cell::new(std::time::Instant::now());
    let settled = common::wait_until(DELIVERY_SETTLE_TIMEOUT, || {
        let count = common::count_file_substr(log, DELIVERED_POINTER);
        if count != last.get() {
            last.set(count);
            stable_since.set(std::time::Instant::now());
            return false;
        }
        stable_since.get().elapsed() >= DELIVERY_SETTLE_QUIET_WINDOW
    });
    assert!(
        settled,
        "the delivery log's `{DELIVERED_POINTER}` count never held steady for \
         {DELIVERY_SETTLE_QUIET_WINDOW:?} within {DELIVERY_SETTLE_TIMEOUT:?} (last seen: \
         {}) — something is still writing the pointer, so no later count assertion in \
         this file can be attributed to a re-assertion.\nFinal grid:\n{}",
        last.get(),
        deck.snapshot_grid()
    );
    let settled_count = last.get();
    assert!(
        settled_count <= ceiling,
        "the delivery log settled at {settled_count} `{DELIVERED_POINTER}` line(s), above \
         the {ceiling} one logical delivery can account for \
         ({MAX_POINTER_LINES_PER_DELIVERY} payload writes at most — \
         `MAX_PAYLOAD_SUBMISSIONS` plus issue #666's one evidence-based rearm, \
         `src/prompt_delivery.rs`). More than one delivery therefore ran, so this count \
         cannot serve as a baseline for anything.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
    settled_count
}

/// Count how many times the orchestrator context file is REWRITTEN between
/// [`ContextRewriteWatcher::start`] and [`ContextRewriteWatcher::stop`] — the
/// exactly-once detector `orchestration_remit_004` needs and that no pointer
/// count can provide.
///
/// Greptile P2 on PR #837. Counting pointer lines cannot tell ONE re-assertion
/// whose delivery earned its replacement payload write (`baseline + 2` lines,
/// designed behaviour, `MAX_PAYLOAD_SUBMISSIONS` in `src/prompt_delivery.rs`)
/// from a genuine DOUBLE FIRE whose two deliveries were each confirmed first
/// time (`baseline + 2` lines, the regression). Both land inside
/// [`DELIVERY_SETTLE_QUIET_WINDOW`], so a settled baseline absorbs the second
/// one and the "stays put" check behind it then measures from a baseline that
/// already contains the defect.
///
/// The context FILE separates them, because the two writes have different
/// authors. A re-arm calls `reassert_orchestrator_prompt` (`src/ui.rs`), which
/// goes through `prepare_orchestrator_prompt` (`src/orchestrator_context.rs`)
/// and `std::fs::write`s the whole file. A payload retry re-sends the
/// ALREADY-PREPARED prompt string and never touches the file at all. So the
/// number of rewrites IS the number of logical re-assertions, whatever the
/// pointer count did.
///
/// **Why a background sampler rather than reading the file at each step.** The
/// first version of this check stamped a sentinel comment into the file,
/// waited for the re-assertion's log line, and re-stamped. That misses,
/// measured: against a deliberate double-fire mutation it caught only 3 of 5
/// runs, because the SECOND re-arm fires one frame after the first payload
/// write — i.e. at essentially the same moment as the log line the test is
/// waiting on, and usually before the test can react to it. Sampling from
/// before the trigger removes the reaction entirely: there is no moment the
/// test has to be quick enough to catch.
#[cfg(unix)]
struct ContextRewriteWatcher {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: std::thread::JoinHandle<(usize, bool)>,
}

#[cfg(unix)]
impl ContextRewriteWatcher {
    fn start(context_path: &std::path::Path) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let path = context_path.to_path_buf();
        let thread_stop = std::sync::Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mtime = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
            let last = std::cell::Cell::new(mtime(&path));
            // Start the debounce already elapsed, so the FIRST rewrite always
            // counts however soon after `start` it arrives.
            let last_counted =
                std::cell::Cell::new(std::time::Instant::now() - CONTEXT_REWRITE_DEBOUNCE);
            let rewrites = std::cell::Cell::new(0usize);
            // The pacing comes from `common::wait_until` — the harness's own
            // bounded poll — rather than from a sleep in this file. That is
            // Decision 21 (no sleeps in an e2e test body), and the cadence is
            // right on the measurement rather than by luck: `wait_until` polls
            // every 50ms (`tests/common/mod.rs`) against a ~680ms gap between
            // two genuine re-arms, so ~13x margin.
            let stopped_cleanly = common::wait_until(CONTEXT_WATCH_MAX, || {
                let now = mtime(&path);
                if now.is_some() && now != last.get() {
                    last.set(now);
                    if last_counted.get().elapsed() >= CONTEXT_REWRITE_DEBOUNCE {
                        rewrites.set(rewrites.get() + 1);
                        last_counted.set(std::time::Instant::now());
                    }
                }
                thread_stop.load(std::sync::atomic::Ordering::Relaxed)
            });
            (rewrites.get(), stopped_cleanly)
        });
        Self { stop, handle }
    }

    fn stop(self) -> usize {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let (rewrites, stopped_cleanly) =
            self.handle.join().expect("context rewrite watcher thread");
        assert!(
            stopped_cleanly,
            "the context-rewrite watcher hit its own {CONTEXT_WATCH_MAX:?} ceiling before the \
             test asked it to stop, so it stopped sampling early and its count of \
             {rewrites} rewrite(s) is a floor rather than a total — do not read an \
             exactly-once verdict off it."
        );
        rewrites
    }
}

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
/// **Timing hazard 1, the one that bit** (issues #818, #832): `confirm_submission`
/// runs AFTER the log line is appended, in a forked `python3`, so the log line
/// every test in this file waits on does not imply the confirmation has
/// happened — let alone been applied. The confirmation is a `thinking` event,
/// and `AppState::apply_event` (`src/state.rs`) asserts `session.status`
/// unconditionally for that event type, so a confirmation still in flight when
/// a test injects `Compacting` or a `/clear` `SessionStart` overwrites the
/// injected status permanently. That reddened `e2e-deterministic` on `main` and
/// on every open PR from the merge of #789 onward. It is closed by
/// [`open_and_confirm_initial_delivery`]'s barrier, which waits for the
/// confirmation's own applied footprint rather than for the log line; read that
/// comment before adding a test here, and reach for [`wait_for_applied`] rather
/// than a marker file for any new phase this script grows.
///
/// **Timing hazard 2, the second one**: `confirm_submission` completing inside
/// `unconfirmed_retry_delay(1)` — 500ms (`src/prompt_delivery.rs`) — of the
/// initial write is not enforced by anything, and under load it does not.
/// `MAX_PAYLOAD_SUBMISSIONS` there is 2, so a delivery still unconfirmed past
/// that window earns one automatic *replacement* payload write, appending a
/// second `DELIVERED_POINTER` line to the log with no re-assertion behind it.
/// The hardcoded counts this file used to assert read that as a false GREEN
/// wherever they wanted 2, and as a false RED on every "must NOT reach a second
/// line" check and on `orchestration_remit_004`'s "must not reach a third".
/// Measured under a 64-way CPU load: 8 such failures across 6 module runs. The
/// retry itself is designed behaviour and still happens; what changed is that
/// [`settled_pointer_count`] gives every assertion a settled baseline to count
/// from, so a replacement write no longer reads as a re-assertion. It is a
/// different failure class from hazard 1 and it is NOT what #818/#832 were.
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

    let applied = wait_for_applied(socket, pane_id, INJECTED_EVENT_APPLIED_TIMEOUT, |s| {
        s.status == dot_agent_deck::state::SessionStatus::Compacting
    });
    assert!(
        applied,
        "the daemon's own ListAgents/live-status join never reported Compacting for pane \
         {pane_id} (agent_id {agent_id}) within {INJECTED_EVENT_APPLIED_TIMEOUT:?}. \
         `SessionStatus` is a LEVEL, so the two candidate causes are that \
         AppState::apply_event rejected the write (or applied it to the wrong session), \
         or that a later event on this same pane overwrote the status before this poll \
         sampled it — `EventType::Thinking` and `EventType::SessionStart` both assert \
         `session.status` unconditionally (`src/state.rs`). Check the second first: it \
         is what issues #818/#832 turned out to be, and no wait budget can recover from \
         it, so a wider bound here is not the fix.",
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
/// Drive `pane_id`'s live session onto a UNIQUELY-NAMED active tool and block
/// until the daemon has applied it — the primer that makes
/// [`inject_clear_session_start`]'s own barrier a genuine transition rather
/// than a predicate that may already be true.
///
/// Greptile P1 on PR #837. `SessionStatus::Idle` is what
/// `AppState::apply_event`'s `EventType::SessionStart` arm sets, and it is
/// also the resting value of a session that has done nothing in particular —
/// so "wait for Idle" is not a barrier on the injected event, it is a barrier
/// on a VALUE the injected event happens to produce. Anything else that
/// reaches that value first satisfies it, and every assertion behind it (the
/// 900ms negative checks in `_005`/`_006` most of all) then runs against state
/// the injection has not reached. That is TRIVIALITY, not impatience: no wider
/// bound rescues a predicate that was already true, which is the same shape of
/// mistake as the bounds #818 first proposed for the status clobber.
///
/// Measured on this branch before the primer existed, instrumenting all three
/// call sites: the start-role pane read `Thinking` (the spawn confirmation's
/// own footprint) and the `_005` worker pane had NO live session at all, so
/// `wait_for_applied`'s `is_some_and` was false and every wait did in fact
/// barrier. So the finding is LATENT rather than currently firing — the
/// predicate is sound today only because of what the fixture happens to leave
/// on those panes, and this makes it sound by construction instead.
///
/// `EventType::ToolStart` is the primer because it is the only arm that writes
/// a value THIS CALL CHOOSES into the snapshot — `active_tool.name`
/// (`src/state.rs`) — so its own barrier cannot be satisfied by anything the
/// pane was already doing. It is inert for the feature under test: the two
/// re-assertion triggers are a `Compacting` status and a `/clear`-shaped
/// `SessionStart` (`should_reassert_orchestrator_remit` /
/// `orchestrator_remit_pane_latest_clear_session_start`, `src/ui.rs`), and the
/// `Working` this sets is neither. It carries the caller's own `agent_type` so
/// it cannot change the session's recorded one relative to the injection that
/// follows — `apply_event` fixes a session's `agent_type` from its first
/// non-`None` event and never changes it afterwards.
#[cfg(unix)]
fn prime_active_tool(
    deck: &TuiDeck,
    socket: &std::path::Path,
    pane_id: &str,
    agent_id: &str,
    session_id: &str,
    agent_type: &AgentType,
    tool_marker: &str,
) {
    let event = AgentEvent {
        session_id: session_id.to_string(),
        agent_type: agent_type.clone(),
        event_type: EventType::ToolStart,
        tool_name: Some(tool_marker.to_string()),
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
    let line = serde_json::to_string(&event).expect("serialize synthetic ToolStart AgentEvent");
    common::write_hook_line(deck.hook_socket_path(), &line)
        .expect("inject synthetic ToolStart AgentEvent over hook socket");

    let primed = wait_for_applied(socket, pane_id, INJECTED_EVENT_APPLIED_TIMEOUT, |s| {
        s.active_tool
            .as_ref()
            .is_some_and(|t| t.name == tool_marker)
    });
    assert!(
        primed,
        "the daemon's own ListAgents/live-status join never reported the priming tool \
         `{tool_marker}` for pane {pane_id} (agent_id {agent_id}) within \
         {INJECTED_EVENT_APPLIED_TIMEOUT:?}. The tool name is unique to this call, so \
         nothing else can produce it: either AppState::apply_event rejected the event \
         (admission control — is this pane owned?) or it applied it to a different \
         session.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}

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
        agent_type: agent_type.clone(),
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

    // See [`prime_active_tool`]. The barrier below cannot be a wait for
    // `SessionStatus::Idle`: that is a VALUE this event produces, not a
    // footprint of this event, and it is also where a quiet pane already sits.
    let tool_marker = format!("remit-clear-primer-{session_id}");
    prime_active_tool(
        deck,
        socket,
        pane_id,
        agent_id,
        session_id,
        &agent_type,
        &tool_marker,
    );

    common::write_hook_line(deck.hook_socket_path(), &line)
        .expect("inject synthetic clear-originated SessionStart AgentEvent over hook socket");

    // `active_tool` CLEARED, not `status == Idle`, and the difference is two
    // separate things.
    //
    // Non-triviality: the primer above established `Some(tool_marker)` and had
    // its own applied barrier, so this predicate was demonstrably false a
    // moment ago. Nothing in this fixture emits a second tool event.
    //
    // Latching: `Idle` is TRANSIENT here. On a pane whose re-assertion this
    // event triggers, the delivery's own `thinking` confirmation overwrites
    // `Idle` within roughly one `wait_until` poll interval (50ms,
    // `tests/common/mod.rs`), so a `status == Idle` wait can miss the window
    // entirely and time out on a correct system — trading a predicate that
    // passes too easily for one that fails at random. `active_tool` stays
    // `None` once cleared, and the only events that clear it here are this
    // `SessionStart` or a `Thinking` that is causally DOWNSTREAM of it
    // (`src/state.rs`), so either way this becoming true means the injection
    // was applied.
    let applied = wait_for_applied(socket, pane_id, INJECTED_EVENT_APPLIED_TIMEOUT, |s| {
        s.active_tool.is_none()
    });
    assert!(
        applied,
        "the daemon's own ListAgents/live-status join still reported the priming tool \
         `{tool_marker}` for pane {pane_id} (agent_id {agent_id}) \
         {INJECTED_EVENT_APPLIED_TIMEOUT:?} after injecting a synthetic \
         clear-originated SessionStart, so that SessionStart was never applied. \
         `EventType::SessionStart` clears `active_tool` unconditionally \
         (`src/state.rs`), so the candidate causes are that AppState::apply_event \
         rejected the write or applied it to the wrong session.",
    );
}

/// Open the orchestration, write and launch the orchestrator's synthetic
/// script, and confirm the spawn-time remit pointer lands once. Returns the
/// daemon socket path, the start role's `(pane_id, agent_id)`, the log
/// path every test in this file asserts delivery counts against, and the
/// SETTLED baseline count of pointer lines that delivery left in it — both
/// the script and the log live directly under `deck.workdir()`, the directory
/// the orchestrator role pane actually runs in.
fn open_and_confirm_initial_delivery(
    deck: &TuiDeck,
) -> (
    std::path::PathBuf,
    String,
    String,
    std::path::PathBuf,
    usize,
) {
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
        common::wait_for_file_substr_count(&log, DELIVERED_POINTER, 1, SPAWN_DELIVERY_TIMEOUT);
    assert!(
        initial_delivered,
        "precondition failed: the spawn-time orchestrator prompt never reached the \
         start role's pane within {SPAWN_DELIVERY_TIMEOUT:?}\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    // THE BARRIER (issues #818, #832). The wait above returns the moment the
    // log line lands, and the fixture script's read loop appends that line
    // BEFORE forking the `python3` that reports the delivery back as a
    // `thinking` event — so on its own it leaves that confirmation in flight.
    // Every caller's next move is to inject a `Compacting` or a
    // `/clear`-originated `SessionStart` for this same pane, and
    // `AppState::apply_event`'s `EventType::Thinking` arm (`src/state.rs`)
    // sets `session.status` unconditionally: the in-flight confirmation lands
    // second and destroys the status the caller is about to poll for.
    //
    // That is not a slow-runner problem and it is not recoverable by waiting
    // longer, which is why the fix is here rather than in the bounds. Measured
    // on an idle 16-core box, `orchestration_remit_001` failed 2 of 5 solo
    // runs before this barrier existed; under a 32-way CPU load, 2 of 4 — and
    // an instrumented poll of the failing runs showed the pane sitting on
    // `Thinking` with the delivered pointer as its `last_user_prompt` for the
    // entire 10s, the injected `Compacting` never once sampled. Which side
    // wins is decided by `python3` start-up latency against this process's own
    // `ListAgents` round trip, so a saturated runner simply loses it more
    // often than a dev box does (issue #818's bisect: five green lane-1 runs,
    // then red on every branch from the merge of #789 onward).
    //
    // `last_user_prompt` is the confirmation's own applied footprint: the
    // fixture's `confirm_submission` sends the delivered line as the event's
    // `user_prompt`, and `apply_event` records it on the session. Once it is
    // visible here the confirmation has been APPLIED, not merely sent, so the
    // caller's injection is the last word on this pane's status.
    let confirmation_applied =
        wait_for_applied(&socket, &pane_id, CONFIRMATION_APPLIED_TIMEOUT, |s| {
            s.last_user_prompt
                .as_deref()
                .is_some_and(|p| p.contains(DELIVERED_POINTER))
        });
    assert!(
        confirmation_applied,
        "precondition failed: the fixture script confirmed the spawn-time pointer over \
         the hook socket, but the daemon never reported it as the start role's \
         `last_user_prompt` within {CONFIRMATION_APPLIED_TIMEOUT:?} — without that the \
         confirmation is still in flight and will overwrite whatever status the caller \
         injects next.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    // The settled baseline (see [`settled_pointer_count`]). Taken here, after
    // the confirmation barrier above, so it accounts for the one replacement
    // payload write an unconfirmed spawn-time delivery earns under load —
    // which every caller would otherwise mistake for a re-assertion. The
    // ceiling counts from ZERO because exactly one delivery — the spawn-time
    // seed — has run by this point.
    let baseline = settled_pointer_count(deck, &log, MAX_POINTER_LINES_PER_DELIVERY);

    (socket, pane_id, agent_id, log, baseline)
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
    let (socket, pane_id, agent_id, log, baseline) = open_and_confirm_initial_delivery(&deck);

    inject_compacting(
        &deck,
        &socket,
        &pane_id,
        &agent_id,
        &format!("{agent_id}-remit001-session"),
    );

    let reasserted = common::wait_for_file_substr_count(
        &log,
        DELIVERED_POINTER,
        baseline + 1,
        REASSERTION_DELIVERY_TIMEOUT,
    );
    assert!(
        reasserted,
        "a Compacting event on the orchestrator start-role pane must re-deliver the \
         `{DELIVERED_POINTER}` remit pointer once more; the log never rose past the \
         {baseline} line(s) the spawn-time delivery settled at, within \
         {REASSERTION_DELIVERY_TIMEOUT:?}.\n\
         Final grid:\n{}",
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
    let (socket, orch_pane_id, orch_agent_id, log, baseline) =
        open_and_confirm_initial_delivery(&deck);

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

    let leaked_to_worker = common::wait_for_file_substr_count(
        &log,
        DELIVERED_POINTER,
        baseline + 1,
        Duration::from_millis(900),
    );
    assert!(
        !leaked_to_worker,
        "a Compacting event on the non-start `worker` role's pane must not re-assert \
         the orchestrator's remit; the start role's delivery log rose past its settled \
         {baseline} `{DELIVERED_POINTER}` line(s) anyway.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    inject_compacting(
        &deck,
        &socket,
        &orch_pane_id,
        &orch_agent_id,
        &format!("{orch_agent_id}-remit002-orch-session"),
    );
    let reasserted_on_start_role = common::wait_for_file_substr_count(
        &log,
        DELIVERED_POINTER,
        baseline + 1,
        REASSERTION_DELIVERY_TIMEOUT,
    );
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
    let (socket, pane_id, agent_id, log, baseline) = open_and_confirm_initial_delivery(&deck);

    std::fs::write(deck.workdir().join("go-history-only"), "")
        .expect("trigger the fixture script's history-only phase");
    assert!(
        common::wait_until(Duration::from_secs(5), || {
            deck.workdir().join("history-only-emitted").exists()
        }),
        "the fixture script never emitted its history-only session_start within 5s"
    );
    // The marker file above proves only that the fixture WROTE that
    // `session_start` to the socket, and its `EventType::SessionStart` arm
    // asserts `session.status = Idle` just as unconditionally as the
    // confirmation's `thinking` does — so injecting while it is still in
    // flight loses the same race [`open_and_confirm_initial_delivery`]'s
    // barrier closes. Wait for the phase change the emit actually declares
    // (`live_target.writable`), which is what this test needs to be true
    // anyway: the whole point is a re-assertion arriving while the pane is
    // history-only.
    assert!(
        wait_for_applied(&socket, &pane_id, CONFIRMATION_APPLIED_TIMEOUT, |s| {
            s.live_target.map(|t| t.writable) == Some(Writable::HistoryOnly)
        }),
        "the daemon never applied the fixture's history-only live_target for pane \
         {pane_id} within {CONFIRMATION_APPLIED_TIMEOUT:?}; the pane is still writable, \
         so the injection below would exercise the live path this test is not about.\n\
         Final grid:\n{}",
        deck.snapshot_grid()
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

    let wrote_blindly = common::wait_for_file_substr_count(
        &log,
        DELIVERED_POINTER,
        baseline + 1,
        Duration::from_millis(900),
    );
    assert!(
        !wrote_blindly,
        "a Compacting-triggered re-assertion must not write to a history-only pane \
         before delivery is confirmed; the log rose past its settled {baseline} \
         `{DELIVERED_POINTER}` line(s) while the pane was still history-only.\n\
         Final grid:\n{}",
        deck.snapshot_grid()
    );

    let feedback = deck.wait_for_grid_string_within(
        "History-only session cannot accept live input",
        Duration::from_secs(5),
    );

    std::fs::write(deck.workdir().join("go-live-again"), "")
        .expect("trigger the fixture script's return-to-live phase");
    let delivered_once_live = common::wait_for_file_substr_count(
        &log,
        DELIVERED_POINTER,
        baseline + 1,
        REASSERTION_DELIVERY_TIMEOUT,
    );

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
         re-assertion must complete and deliver the pointer once more (past the \
         settled {baseline} line(s))\n\
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
/// reused delivery machinery. It must then re-assert EXACTLY once: a sentinel
/// comment is stamped into `.dot-agent-deck/orchestrator-context.md` before
/// the trigger, the re-assertion's own rewrite of that file must destroy it
/// (proving the sentinel detects a re-arm at all), and a freshly stamped one
/// must then survive the settle-and-hold window that follows.
#[spec("orchestration/remit/004")]
#[test]
#[cfg(unix)]
fn orchestration_remit_004_start_role_clear_reasserts_remit() {
    let deck = TuiDeck::launch_with_fixture("remit-reassert-orchestration");
    let (socket, pane_id, agent_id, log, baseline) = open_and_confirm_initial_delivery(&deck);

    // Arm the exactly-once detector BEFORE the trigger (see
    // [`ContextRewriteWatcher`]). It has to be running already: the second
    // re-arm of a double-fire regression lands one frame after the first
    // payload write, so any detector that only starts looking once the test
    // has OBSERVED that write is racing something it loses more often than it
    // wins.
    let context_path = deck
        .workdir()
        .join(".dot-agent-deck")
        .join("orchestrator-context.md");
    let rewrites = ContextRewriteWatcher::start(&context_path);

    inject_clear_session_start(
        &deck,
        &socket,
        &pane_id,
        &agent_id,
        &format!("{agent_id}-remit004-session"),
        AgentType::ClaudeCode,
    );

    let reasserted = common::wait_for_file_substr_count(
        &log,
        DELIVERED_POINTER,
        baseline + 1,
        REASSERTION_DELIVERY_TIMEOUT,
    );
    assert!(
        reasserted,
        "a `/clear`-originated SessionStart event on the orchestrator start-role pane \
         must re-deliver the `{DELIVERED_POINTER}` remit pointer once more; the log \
         never rose past the {baseline} line(s) the spawn-time delivery settled at, \
         within {REASSERTION_DELIVERY_TIMEOUT:?}.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    // Pin non-repetition, not just arrival — a single `/clear`-originated
    // `SessionStart` event must deliver the pointer exactly once more, never
    // repeatedly. Mirrors the "stays put" shape `orchestration_remit_002`/
    // `_005` already use for their negative leak checks
    // (`!wait_for_file_substr_count(..., short bound)`), applied here to the
    // count staying PUT rather than never rising.
    //
    // Settled first, for the same reason the baseline is: the re-assertion's
    // OWN delivery earns a replacement payload write if the fixture's
    // confirmation misses the deck's 500ms window, and counting from an
    // unsettled figure would read that designed retry as a second
    // re-assertion. Under a 64-way CPU load this reddened here specifically,
    // on the literal "must not reach a third line".
    let settled_after_reassertion =
        settled_pointer_count(&deck, &log, baseline + MAX_POINTER_LINES_PER_DELIVERY);
    let repeated_beyond_one_reassertion = common::wait_for_file_substr_count(
        &log,
        DELIVERED_POINTER,
        settled_after_reassertion + 1,
        Duration::from_millis(900),
    );
    assert!(
        !repeated_beyond_one_reassertion,
        "a single `/clear`-originated SessionStart event must not re-deliver the remit \
         pointer more than once; the log rose past its settled \
         {settled_after_reassertion} `{DELIVERED_POINTER}` lines within a bounded wait \
         after the re-assertion.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    // The half the pointer count cannot do (Greptile P2 on PR #837). Settling
    // absorbs everything written inside its quiet window, and a count cannot
    // separate ONE delivery that earned its replacement payload write from TWO
    // deliveries that were each confirmed first time — both read as
    // `baseline + 2`, so the check above would take a genuine double fire as
    // its baseline and then confirm the count stayed put at it. Rewrites of
    // the context file are not a count of deliveries, they are a count of
    // RE-ARMS: only `reassert_orchestrator_prompt` writes that file and a
    // payload retry never does.
    //
    // Stopped here rather than earlier so the window covers everything above —
    // the delivery, its settle, and the hold.
    let rewrites = rewrites.stop();
    assert_eq!(
        rewrites,
        1,
        "a single `/clear`-originated SessionStart event must trigger exactly ONE \
         logical re-assertion, but {} was rewritten {rewrites} time(s) while this test \
         watched it. 0 means no re-arm ran at all and the delivery above came from \
         somewhere else, so this detector is blind — check it before anything else. 2 \
         or more is a double fire: only `reassert_orchestrator_prompt` (`src/ui.rs` -> \
         `src/orchestrator_context.rs`) writes that file, so a second write IS a second \
         re-arm, and the edge detection on `orchestration_remit_clear_reasserted_at` is \
         what to look at. The pointer count cannot see this at all — two confirmed \
         deliveries and one retried delivery leave the same number of lines.\n\
         Final grid:\n{}",
        context_path.display(),
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
    let (socket, orch_pane_id, orch_agent_id, log, baseline) =
        open_and_confirm_initial_delivery(&deck);

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

    let leaked_to_worker = common::wait_for_file_substr_count(
        &log,
        DELIVERED_POINTER,
        baseline + 1,
        Duration::from_millis(900),
    );
    assert!(
        !leaked_to_worker,
        "a `/clear`-originated SessionStart event on the non-start `worker` role's pane \
         must not re-assert the orchestrator's remit; the start role's delivery log \
         rose past its settled {baseline} `{DELIVERED_POINTER}` line(s) anyway.\n\
         Final grid:\n{}",
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
    let reasserted_on_start_role = common::wait_for_file_substr_count(
        &log,
        DELIVERED_POINTER,
        baseline + 1,
        REASSERTION_DELIVERY_TIMEOUT,
    );
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
    let (socket, pane_id, agent_id, log, baseline) = open_and_confirm_initial_delivery(&deck);

    inject_clear_session_start(
        &deck,
        &socket,
        &pane_id,
        &agent_id,
        &format!("{agent_id}-remit006-non-claude-session"),
        AgentType::Codex,
    );

    let leaked_for_non_claude_agent_type = common::wait_for_file_substr_count(
        &log,
        DELIVERED_POINTER,
        baseline + 1,
        Duration::from_millis(900),
    );
    assert!(
        !leaked_for_non_claude_agent_type,
        "a `/clear`-originated SessionStart event stamped with a non-Claude-Code \
         `agent_type` must not re-assert the orchestrator's remit (the trigger's stated \
         scope is Claude Code only); the start role's delivery log rose past its \
         settled {baseline} `{DELIVERED_POINTER}` line(s) anyway.\nFinal grid:\n{}",
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
    // `_baseline` unused here alone: this test counts [`CARRY_OUT_TASK_POINTER`],
    // a needle the spawn-time delivery never writes — so a replacement payload
    // retry of that delivery cannot inflate it and there is nothing to count
    // from. Every other test in this file counts [`DELIVERED_POINTER`], which
    // the spawn-time delivery does write, and therefore needs the baseline.
    let (socket, pane_id, agent_id, log, _baseline) = open_and_confirm_initial_delivery(&deck);

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

    let reasserted_with_task = common::wait_for_file_substr_count(
        &log,
        CARRY_OUT_TASK_POINTER,
        1,
        REASSERTION_DELIVERY_TIMEOUT,
    );
    assert!(
        reasserted_with_task,
        "a compaction re-assertion on a start role whose context file carries a `## Your \
         task` section must re-deliver the TASK-CARRYING pointer (containing \
         `{CARRY_OUT_TASK_POINTER}`), not the no-task \"wait for instructions\" variant; \
         the log never shows it within {REASSERTION_DELIVERY_TIMEOUT:?}.\nFinal grid:\n{}",
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
