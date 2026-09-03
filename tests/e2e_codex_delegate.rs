#![cfg(all(feature = "e2e", feature = "e2e-live"))]

//! L2 PTY-attached REAL-agent proof for PRD #225 M5: a `clear = true` delegate
//! to a **Codex** worker delivers the prompt and the worker acts on it.
//!
//! This is the empirical end of the fix that no synthetic test can reach. The
//! fast/synthetic siblings (`orchestration/delegate/007`, `/008`,
//! `codex/spawn/007`) pin the mechanism — a wrapper's fork-time
//! card-surfacing `SessionStart` must not release the delegate readiness gate
//! for an agent that still owes a native one, and the launch shape must not
//! mutate across respawn — by injecting hand-built events around a `cat`
//! stand-in. What they cannot reproduce is the thing that actually broke in
//! production: a REAL Codex whose wrapper forks seconds before the Codex TUI
//! exists, so the prompt lands in a PTY where only the launcher is running and
//! the line discipline echoes it away. Only a real boot sequence has that gap.
//!
//! What the user sees here, end to end and unattended:
//!   - The deck opens an ORCHESTRATION tab from the normal Ctrl+N new-pane
//!     form, with two role panes: an `orchestrator` and a `clear = true`
//!     `coder` worker running a REAL interactive cheap-model Codex (NO `-p`,
//!     no stand-in) through the production wrapper seam — the role command's
//!     basename is `codex`, so `AgentType::from_command` resolves it and the
//!     pane is wrapped from its FIRST spawn, which is exactly the shape that
//!     hits the readiness race even with PRD #225's Defect 2 fixed.
//!   - The worker's role pane is focused (digit-jump, as a user does) and the
//!     REAL Codex TUI is seen coming up in it — its header naming the pinned
//!     model. That is the readiness precondition: the orchestrator only
//!     delegates once the agent, not just the launcher, is on screen.
//!   - The orchestrator runs the real `dot-agent-deck delegate --to coder`
//!     CLI. The daemon writes the worker task file, RESPAWNS the worker
//!     (`clear = true`), waits for the replacement's genuine readiness, and
//!     injects the single-line task pointer.
//!   - The worker's card visibly enters `Thinking`, the daemon broadcasts the
//!     worker's GENUINE native Codex `SessionStart` (no wrapper-fork origin
//!     marker — the distinction this PRD introduced) and a `Thinking` carrying
//!     the injected pointer as its `user_prompt`. Only Codex's native
//!     `UserPromptSubmit` hook sets that field (the wrapper's line classifier
//!     always leaves it `None`), so it is proof the prompt was really submitted
//!     inside the agent rather than echoed into a launcher's line discipline.
//!     The worker then creates the uniquely named sentinel file. Pre-fix the
//!     prompt is swallowed, the pane sits at an empty composer, and no sentinel
//!     ever appears.
//!
//! ## Why readiness is asserted on the TUI, and the native events afterwards
//! codex-cli 0.145.0 posts its native `SessionStart` when the first TURN
//! starts — i.e. *after* a prompt is submitted — not when the TUI comes up. So
//! gating the delegate on that native event would deadlock: the event is caused
//! by the very delegate it would gate (measured: fork-time `SessionStart` at
//! T+0, native one only at T+30s, the full `SESSION_START_WAIT_TIMEOUT`
//! fallback, immediately followed by the prompt's own `UserPromptSubmit`).
//! Hence the pre-delegate precondition is the user-visible one — the Codex TUI
//! is up in the pane — exactly as `codex/live/001` does it, and the native
//! events are asserted AFTER the delegate, where they legitimately prove
//! delivery reached the agent.
//!
//! That used to carry a consequence — for Codex the readiness gate never
//! fast-paths, so every `clear = true` delegate paid the full 30 s timeout
//! fallback. **Issue #243 removed it**: `dot-agent-deck wrap` now watches the
//! child's terminal and emits a second `SessionStart` marked
//! `wrapper_interface_ready` once the Codex TUI takes the inner PTY out of
//! cooked mode, and the gate releases on that instead. The precondition above is
//! unchanged (it is still asserted on the user-visible TUI, and Codex's NATIVE
//! `SessionStart` is still caused by the prompt), but the pointer now arrives a
//! few seconds after the replacement becomes ready rather than 31 s later —
//! which is why this test carries [`READY_TO_SUBMIT_BUDGET`] below. Without it,
//! it passes identically on the fixed and the broken path.
//!
//! **Round 3: raw mode releases the gate, it does not prove input-readiness.**
//! This module said "within a second", because the strong fact used to SKIP the
//! post-readiness buffer. It does not any more, and this test is part of why:
//! `/009` recorded the wrapper observing raw mode at fork + 100 ms on both the
//! original worker and its replacement, wrote on it, and lost the pointer into
//! an unsubmitted composer — a full-screen TUI takes the terminal at INIT,
//! before it will accept a submit. The strong fact is now priced at
//! `WRAPPER_INTERFACE_READINESS_BUFFER` (5000 ms, measured against codex-cli's
//! initialisation), which is what this test pins and what its budget is derived
//! against.
//!
//! ## Why the orchestrator is a script and the worker is the real agent
//! The defect lives entirely on the WORKER side of the delegate: prompt
//! delivery into a respawned wrapped agent. The orchestrator's only job is to
//! invoke `dot-agent-deck delegate`, which is exactly what a real orchestrator
//! agent does — so the script runs the SAME production CLI over the SAME hook
//! socket and the daemon path under test is byte-for-byte the one a real
//! orchestrator drives. Making the orchestrator a second LLM would add a flaky
//! link (and a second bill) in front of the assertion without exercising one
//! additional line of the code this PRD changed. The script waits for a
//! test-written trigger file so the delegate fires only once the worker is
//! genuinely up — production timing, not a boot race.
//!
//! Decision 23 cost: one short cheap-model Codex turn (read a task file, run a
//! single `printf`) — well under the <$0.05/run bound. Local-only (Decision 8 /
//! CLAUDE.md rule 5): gated behind the `e2e` feature so CI's `cargo test-fast`
//! never compiles it. Flaky-tolerant (real LLM) per rule 4 — run once, not
//! looped. Decision 26 runtime-skip when the Codex CLI / credentials are absent.

mod common;

use std::path::Path;
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::event::{AgentType, EventType};
use dot_agent_deck::state::DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS;
use spec::spec;

/// The orchestration name declared in the generated project config — also the
/// orchestration TAB label, and the `[Orch: …]` chip the new-pane form shows.
const ORCH_NAME: &str = "codex-delegate";
/// The `clear = true` worker role the orchestrator delegates to.
const WORKER_ROLE: &str = "coder";

/// The file the delegated worker must create. Uniquely named (rule 4) so the
/// assertion survives LLM phrasing/tool variance and can never collide with a
/// pre-existing file, another fixture's sentinel, or another test's workdir.
const SENTINEL_NAME: &str = "prd225-codex-delegate-6f21ba.txt";
/// Exact contents the directive asks for — asserted trimmed, since a model may
/// append a newline no matter how the prompt is worded.
const SENTINEL_CONTENT: &str = "PRD225_DELEGATE_OK";

/// Issue #243: the post-readiness buffer this test pins, mirroring the deck's own
/// `WRAPPER_INTERFACE_READINESS_BUFFER` because that constant is `pub(crate)`.
///
/// **The harness pins the buffer to `0` for every e2e test
/// (`tests/common/mod.rs`), and this test used to inherit that.** `/014` and
/// `/015` opt back in and say why; `/009` did not, and its own budget comment
/// cited the `0` as a reason the number could be tight. That was defensible only
/// while the strong interface fact skipped the buffer anyway — a wrapped Codex at
/// `0` and at the default were the same run. Since `56c10dd` they are not: this
/// is the ONE agent for which the buffer is now load-bearing, and 5000 ms of it
/// is what stands between the write and a composer that is still initialising.
/// Left at `0` this test exercised a configuration the deck does not ship, which
/// is exactly what its own red measured — the pointer written at fork + 100 ms
/// and parked, unsubmitted, with no turn ever starting.
///
/// So it opts in for the reason `/014` and `/015` cite (#663: `SessionStart`
/// means "a session exists", not "the TUI interprets `\r` as submit"), and it
/// opts in at the PRODUCTION value rather than at their 1000 ms. Their agents
/// resolve `delegate_readiness_buffer()`; a wrapped Codex that released on the
/// strong fact resolves `wrapper_interface_readiness_buffer()`, and pinning 1000
/// here would use guard 3's operator override to buy back the very race this
/// issue's third round exists to close.
const READINESS_BUFFER_MS: &str = "5000";

/// Issue #243: how long the REPLACEMENT worker may take to get from "the
/// wrapper observed its interface" to "the pointer was submitted inside Codex".
///
/// This test needs a latency bound because without one it cannot tell the fixed
/// path from the broken one. It passed for months while the delegate burned the
/// full [`SESSION_START_WAIT_TIMEOUT`](dot_agent_deck::state) — the timeout
/// fallback delivered eventually, every assertion below held, and the defect
/// #243 fixed was invisible here. It would keep passing straight through a
/// silent regression to that behaviour.
///
/// **Fifteen seconds, and round 3 moved one term rather than re-guessing it.**
/// This was 10 s while the harness's `DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS=0`
/// meant no buffer sat inside the interval at all, leaving the whole 10 s for the
/// part the deck does not control. [`READINESS_BUFFER_MS`] now puts a known,
/// pinned 5000 ms inside it, so the budget is 5 s wider and the uncontrolled
/// share is unchanged.
///
/// *Below:* half of the ~30.6 s a run that still pays the dead wait would
/// measure here — the fallback delivers at ~31 s from the respawn while this
/// interval starts at the interface fact, which a warm Codex produces at
/// ~390 ms. It is also under `SESSION_START_WAIT_TIMEOUT` outright. Nothing that
/// pays the fallback can slip past this as a false green, which is the entire
/// point. The production Codex delegates in the issue sat at 31.2 / 31.2 / 31.7
/// / 31.7 / 32.3 s.
///
/// *Above:* the deck's own contribution is now large but exactly known — the
/// 5000 ms buffer, plus `SUBMIT_DELAY` and a PTY write. The wrapper's
/// `InterfaceWatch` polls on the same 50 ms loop it already runs, and a real
/// Codex TUI releases on the STRONG fact (it clears `ICANON`/`ECHO` rather than
/// merely going quiet). What is left inside this budget is what the deck does not
/// control: codex-cli accepting the keystrokes and its native `UserPromptSubmit`
/// hook reaching the daemon. That hook fires at SUBMIT — it is not a model round
/// trip, and this test separately measures the whole model turn at ~5 s from
/// submission to the sentinel landing — so ~10 s of headroom over the deck's
/// share is roughly what it was before and still leaves seconds for a cold hook
/// exec on a loaded runner. Widen it if a healthy run is ever seen near it; do
/// NOT widen it past ~25 s, where it stops distinguishing the two paths at all.
///
/// **Measured 2026-08-26 against real codex-cli 0.149.0 on `gpt-5.6-luna`:
/// 5.758 s** — the 5000 ms buffer plus 758 ms of codex-cli taking the keystrokes
/// and its native `UserPromptSubmit` hook reaching the daemon. That is 9.2 s of
/// headroom under this budget and 5.3x under the ~30.6 s a dead wait would
/// measure, and it lands where the constant's own derivation predicted a warm
/// Codex would (~5.6 s).
const READY_TO_SUBMIT_BUDGET: Duration = Duration::from_secs(15);

/// Test-written trigger the orchestrator script blocks on, so the delegate
/// fires only after the worker is genuinely up (production timing).
const DELEGATE_TRIGGER: &str = "delegate-now";
/// The delegated task body, passed to the real CLI via `--task-file` so the
/// directive lives in this file (single source of truth) and no shell quoting
/// can mangle it.
const DELEGATE_TASK_FILE: &str = "delegate-task.md";
/// The orchestrator role's command, written into the workdir by the test.
const ORCHESTRATOR_SCRIPT: &str = "orchestrator-delegate.sh";
/// Where the orchestrator script records the `delegate` CLI's output + exit
/// status, so a failure can say whether the delegate even reached the daemon.
const ORCHESTRATOR_LOG: &str = "orchestrator-delegate.log";

/// The orchestrator role: wait for the test's trigger, invoke the REAL
/// `dot-agent-deck delegate` CLI (same binary, same `DOT_AGENT_DECK_SOCKET` /
/// `DOT_AGENT_DECK_PANE_ID` env a real orchestrator agent uses), then park on
/// stdin so the role pane stays alive for the rest of the run.
const ORCHESTRATOR_BODY: &str = r#"#!/bin/sh
# PRD #225 M5 — deterministic orchestrator for `orchestration/delegate/009`.
# The defect under test is on the WORKER side; this role exists only to drive
# the genuine delegate CLI at a production-like moment.
while [ ! -f delegate-now ]; do sleep 0.2; done
dot-agent-deck delegate --to coder --task-file delegate-task.md \
    >> orchestrator-delegate.log 2>&1
printf 'delegate exit=%s\n' "$?" >> orchestrator-delegate.log
# Park on stdin: keeps the role pane alive (and harmlessly absorbs any
# spawn-time prompt the deck injects) until the deck tears the PTY down.
exec cat > /dev/null
"#;

/// PATH for the spawned deck (→ daemon → agents) with the freshly-built
/// `dot-agent-deck` binary's dir prepended to the host PATH: the wrapper seam
/// (`dot-agent-deck wrap --agent codex -- codex …`) and the orchestrator's
/// `dot-agent-deck delegate` both have to resolve it, while the rest of the
/// host PATH is preserved so the real `codex` still resolves.
fn path_with_binary_dir() -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bin_dir = Path::new(bin)
        .parent()
        .expect("test binary has a parent dir")
        .to_str()
        .expect("binary directory is UTF-8");
    format!("{bin_dir}:{}", std::env::var("PATH").unwrap_or_default())
}

#[cfg(unix)]
fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, contents).expect("write orchestrator role script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod orchestrator role script");
}

/// The `[[orchestrations]]` block the new-pane form (and, later, the daemon's
/// `handle_delegate` role lookup) reads from the workdir. Generated rather than
/// shipped in the fixture so the worker command interpolates the pinned cheap
/// model instead of duplicating it where it would silently drift.
///
/// The worker command's BASENAME is `codex`, so `AgentType::from_command`
/// resolves it and the pane is wrapped from its first spawn — the shape PRD
/// #225 calls out as still racing the readiness gate even once the launch shape
/// is stable, and therefore the one this test has to use.
fn orchestration_toml(worker_command: &str) -> String {
    format!(
        "[[orchestrations]]\n\
         name = \"{ORCH_NAME}\"\n\n\
         [[orchestrations.roles]]\n\
         name = \"orchestrator\"\n\
         command = \"./{ORCHESTRATOR_SCRIPT}\"\n\
         start = true\n\n\
         [[orchestrations.roles]]\n\
         name = \"{WORKER_ROLE}\"\n\
         command = {worker_command:?}\n\
         clear = true\n"
    )
}

/// The delegated task. Directive and single-step (rule 4): an exact shell
/// command against a uniquely named sentinel, so success does not depend on how
/// the model phrases anything or which editing tool it prefers.
fn delegate_task() -> String {
    format!(
        "Create the file {SENTINEL_NAME} in the current working directory with the exact \
         contents {SENTINEL_CONTENT} and no trailing newline. Run exactly this shell command: \
         printf '{SENTINEL_CONTENT}' > {SENTINEL_NAME}. Do not use apply_patch and do not \
         modify any other file. That is the entire task."
    )
}

/// Drive the new-pane dialog to open the generated orchestration. With no
/// `[[modes]]` in the config the Mode chip row is `[No mode] [Orch: …]
/// [schedule]`, so ONE Right selects the orchestration; selecting one HIDES the
/// Command field, so the second Enter submits the form.
fn open_orchestration(deck: &TuiDeck) {
    deck.send_keys(b"\x0e"); // Ctrl+N → directory picker
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" "); // Space → confirm the deck's cwd → new-pane form
    deck.wait_for_string("No mode"); // form up, Mode field focused
    deck.send_keys(b"\x1b[C"); // Right → [Orch: codex-delegate]
    deck.wait_for_absence("Command:"); // an orchestration is selected
    deck.send_keys(b"\r"); // Mode → Name
    deck.send_keys(b"\r"); // submit
}

/// Scenario: Launch the real deck, generate an orchestration whose
/// `orchestrator` role runs the genuine `dot-agent-deck delegate` CLI and whose
/// `clear = true` `coder` role runs a REAL interactive cheap-model Codex
/// (wrapped from its first spawn, because the command's basename resolves to
/// Codex), and open it through the normal Ctrl+N new-pane form so both role
/// panes appear in a live orchestration tab. Detach to Normal mode, press `2`
/// to jump into the `coder` role pane, and wait until the REAL Codex TUI is
/// visibly up in it (its header names the pinned model) — the readiness
/// precondition a user can actually see; Codex posts its native `SessionStart`
/// only once a turn starts, so waiting for that event here would deadlock on
/// the delegate it gates. Detach back so the role cards and their live status
/// badges are on screen, then release the orchestrator, which delegates a
/// directive task. The daemon must write the worker task file, respawn the
/// worker for `clear = true`, wait for the REPLACEMENT to be genuinely ready,
/// and inject the single-line task pointer: the worker's card must visibly
/// enter `Thinking`, the daemon must broadcast the worker's GENUINE native
/// Codex `SessionStart` (not the wrapper's marked fork-time card-surfacing one)
/// plus a `Thinking` whose `user_prompt` is the injected pointer — a field only
/// Codex's native `UserPromptSubmit` hook sets, so the prompt was submitted
/// inside the agent rather than echoed away by a launcher's line discipline —
/// and the worker must create the uniquely named sentinel file with the
/// requested contents. Pre-fix the wrapper's fork-time event released the gate
/// ~4s before the Codex TUI existed, the prompt was lost, and no sentinel ever
/// appeared. The delegate must also be PROMPT (issue #243): the wrapper's
/// interface-ready `SessionStart` for the replacement is captured as the anchor,
/// and no more than ten seconds may pass between it and the pointer's submission
/// — without that bound this test passes identically whether the gate released
/// on the readiness signal or gave up after the 30 s fallback. Reel-eligible
/// (PTY-attached real agent, records a `full-stream.cast`); flaky-tolerant (real
/// LLM) — run once, not looped.
#[spec("orchestration/delegate/009")]
#[test]
#[cfg(unix)]
fn delegate_009_real_codex_worker_acts_on_clear_true_delegate() {
    skip_unless!(common::check_codex_available());

    let worker_command = format!(
        "codex --model {} --sandbox workspace-write --ask-for-approval never -c 'sandbox_workspace_write.network_access=true' -c 'model_reasoning_effort=\"low\"'",
        common::codex_test_model(),
    );

    let deck = TuiDeck::builder()
        // 180 cols → the 3-column card grid, so both role cards (and their
        // status badges) are on screen at once — the surface being asserted.
        .with_pty_size(180, 45)
        .with_env("PATH", path_with_binary_dir())
        // Opt back into the production readiness buffer the harness pins to `0`
        // for every other e2e test — see `READINESS_BUFFER_MS`. This is the one
        // agent for which that buffer is load-bearing.
        .with_env(
            DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS,
            READINESS_BUFFER_MS,
        )
        // Real Codex auth in the isolated HOME; also marks the deck's workdir
        // (the role panes' shared cwd) trusted, so no first-run gate can
        // swallow the injected delegate prompt.
        .with_imported_codex_credentials()
        .launch_with_fixture("codex-delegate");
    deck.wait_for_string("No active sessions");

    // Written BEFORE the new-pane form picks the directory:
    // `load_project_config` runs at directory-pick time, and the daemon
    // re-reads the same file when resolving the delegated role's `clear` flag.
    let work = deck.workdir().to_path_buf();
    std::fs::write(
        work.join(".dot-agent-deck.toml"),
        orchestration_toml(&worker_command),
    )
    .expect("write the generated delegate orchestration config");
    std::fs::write(work.join(DELEGATE_TASK_FILE), delegate_task())
        .expect("write the delegated task body");
    write_executable(&work.join(ORCHESTRATOR_SCRIPT), ORCHESTRATOR_BODY);

    // Subscribed before the orchestration opens so the worker's boot events
    // cannot be missed.
    let events = deck.subscribe_events();

    open_orchestration(&deck);
    // The worker role card is on the live orchestration tab — what the user
    // sees before anything is delegated.
    deck.wait_for_string(WORKER_ROLE);
    // Detach the focused role pane (the orchestration opens focused on its
    // `start = true` role) to Normal mode; the Normal-mode button bar is the
    // positive confirmation, so the digit below can never be typed into the
    // orchestrator's stdin instead of being consumed by the deck.
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Pane Ctrl+N]");
    // Jump into the SECOND role card — the `coder` worker — exactly as a user
    // does to watch a worker boot. This focuses its pane and expands it, so the
    // real Codex TUI is the visible surface.
    deck.send_bytes(b"2");

    // THE READINESS PRECONDITION, on the surface the user can actually see: the
    // worker's real Codex TUI is up (its header names the pinned model), so the
    // delegate below lands in a booted agent and not in the launcher that owns
    // the PTY for the first seconds. This is the `codex/live/001` pattern.
    //
    // It deliberately does NOT wait for a native Codex `SessionStart`:
    // codex-cli 0.145.0 posts that when the first TURN starts, which for this
    // worker only happens BECAUSE of the delegate this wait would gate — a
    // circular precondition. The native events are asserted after the delegate
    // instead (below), where they prove delivery rather than deadlock on it.
    assert!(
        deck.wait_for_grid_string_within(common::codex_test_model(), Duration::from_secs(60)),
        "the `clear = true` worker's REAL Codex TUI never came up in its role pane (no {:?} \
         header within 60s) — nothing has been delegated yet, so this is a boot/auth failure, \
         not a delivery failure.\nFinal grid:\n{}",
        common::codex_test_model(),
        deck.snapshot_grid()
    );

    // Detach again so the orchestration deck's role cards (and their live
    // status badges) are the visible surface while the delegate runs.
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Pane Ctrl+N]");

    // Release the orchestrator: it runs the real `dot-agent-deck delegate`.
    //
    // Stamped so the two waits below can be scoped to events broadcast AFTER
    // this instant. `EventSub::wait_for` scans everything the subscription has
    // collected since it opened, and this run's FIRST worker also came up
    // wrapped — so an unscoped predicate would happily match that worker's
    // interface-ready event from the boot precondition above and measure a
    // negative interval.
    let delegate_released_at = chrono::Utc::now();
    std::fs::write(work.join(DELEGATE_TRIGGER), "").expect("release the orchestrator's delegate");

    let orchestrator_log = || {
        std::fs::read_to_string(work.join(ORCHESTRATOR_LOG))
            .unwrap_or_else(|e| format!("<unreadable: {e}>"))
    };
    let task_pointer = work
        .join(".dot-agent-deck")
        .join(format!("worker-task-{WORKER_ROLE}.md"));

    // ISSUE #243's readiness fact, on the wire: `dot-agent-deck wrap` watched
    // the REPLACEMENT worker's Codex interface come up and said so. This is the
    // instant the gate releases on — the anchor the latency bound below is
    // measured from — and before #243 no such event existed at all, which is
    // why this test could not tell a healthy delegate from a 31 s dead wait.
    //
    // Matched on EITHER interface fact (`is_wrapper_interface_session_start`),
    // not just the strong one, because either releases the gate and the point
    // here is to anchor on whatever actually did. A real Codex TUI clears
    // `ICANON`/`ECHO` and so releases on the strong fact; accepting the settled
    // fact too means a Codex that somehow released on the weak one is measured
    // rather than mistaken for "the wrapper never observed anything", which
    // would fail here with a misleading cause.
    //
    // The 90 s window is boot, not gate: it covers the replacement Codex cold
    // starting from scratch, which is why it is generous where the bound below
    // is tight.
    let interface_ready = events.wait_for(
        |event| {
            event.event_type == EventType::SessionStart
                && event.is_wrapper_interface_session_start()
                && event.timestamp >= delegate_released_at
        },
        Duration::from_secs(90),
    );

    // The event PRD #225's readiness gate is about, now where it can actually
    // happen: the respawned worker's GENUINE native Codex `SessionStart` — no
    // wrapper origin marker of EITHER kind, so it is Codex itself and neither the
    // wrapper's fork-time card-surfacing event nor (issue #243) its
    // interface-ready one. It arrives only once the injected pointer
    // starts a turn, so seeing it at all means the daemon respawned the worker
    // and delivered into the live agent. A miss panics with every observed
    // event, which distinguishes "the delegate never reached the daemon" from
    // "it did, but nothing was ever submitted inside Codex".
    events.wait_for(
        |event| {
            event.event_type == EventType::SessionStart
                && event.agent_type == AgentType::Codex
                && !event.is_wrapper_session_start()
        },
        Duration::from_secs(75),
    );

    // The strongest proof the pointer was submitted INSIDE the agent: a Codex
    // `Thinking` whose `user_prompt` is the injected pointer. Only Codex's
    // native `UserPromptSubmit` hook populates that field — the wrapper's line
    // classifier (which also maps output activity to `Thinking`) always leaves
    // it `None` — so this cannot be satisfied by the worker's boot output.
    // Pre-fix the bytes went into the launcher's line discipline and were
    // echoed away, and no such event ever followed.
    let submitted = events.wait_for(
        |event| {
            event.event_type == EventType::Thinking
                && event.agent_type == AgentType::Codex
                && event.user_prompt.is_some()
                && event.timestamp >= delegate_released_at
        },
        Duration::from_secs(30),
    );
    assert!(
        submitted
            .user_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.contains(&format!("worker-task-{WORKER_ROLE}.md"))),
        "the respawned Codex worker submitted a prompt, but not the delegated task pointer: \
         {:?}",
        submitted.user_prompt
    );

    // ISSUE #243's latency bound, and the reason the two events above are
    // captured rather than merely awaited: how long the deck sat on the pointer
    // AFTER the wrapper told it the replacement's interface was up.
    //
    // Everything else in this test is satisfied by the pre-fix path too — the
    // 30 s fallback delivered the pointer eventually, Codex acted on it, and the
    // sentinel appeared. This is the one assertion that distinguishes "released
    // by the readiness signal" from "released by giving up", so it is what keeps
    // a silent regression to the dead wait from shipping green. See
    // `READY_TO_SUBMIT_BUDGET` for how 15 s is derived from both ends, and
    // `READINESS_BUFFER_MS` for the 5000 ms of it this test deliberately pins
    // rather than inheriting the harness's `0`.
    //
    // Measured on the daemon's own clock (both events' `timestamp`s) rather than
    // on this thread's `Instant`, because `EventSub::wait_for` returns from a
    // buffer that may already hold the match — a wall-clock reading taken here
    // would include however long the assertions above spent, not the interval
    // being bounded.
    let ready_to_submit = submitted.timestamp - interface_ready.timestamp;
    assert!(
        ready_to_submit
            <= chrono::Duration::from_std(READY_TO_SUBMIT_BUDGET)
                .expect("READY_TO_SUBMIT_BUDGET fits a chrono Duration"),
        "the delegated pointer reached the REAL Codex worker {ready_to_submit} after the wrapper \
         announced its interface was ready, against a budget of {READY_TO_SUBMIT_BUDGET:?}. The \
         delegate is paying a dead wait again: the readiness signal issue #243 added is on the \
         wire (it is what this measurement starts from), so the gate is not releasing on it and \
         the pointer is arriving via the 30 s SESSION_START_WAIT_TIMEOUT fallback instead. \
         interface_ready={:?} submitted={:?}",
        interface_ready.timestamp,
        submitted.timestamp
    );

    // USER-VISIBLE counterpart of the event above: the worker's card carries
    // the `Thinking` status on the orchestration deck the user is looking at.
    let visibly_thinking = deck.wait_for_stream_string_within("Thinking", Duration::from_secs(30));
    assert!(
        visibly_thinking,
        "the delegated Codex worker never visibly entered `Thinking` — the injected task \
         pointer did not reach the respawned agent. task_pointer_written={} \
         orchestrator_log={:?}\nFinal grid:\n{}",
        task_pointer.exists(),
        orchestrator_log(),
        deck.snapshot_grid()
    );

    // The load-bearing assertion: the worker ACTED on the delegated prompt —
    // the sentinel exists AND carries exactly the requested contents.
    //
    // Polled on CONTENT, not existence. The directive asks for a shell redirect
    // (`printf 'PRD225_DELEGATE_OK' > …`), and `>` CREATES the file before
    // `printf` writes into it: a reader that waits only for the path to appear
    // can win that race and read an empty string. That is a flaw in how this
    // test OBSERVES the result, not in what it proves — the exact-match
    // semantics below are unchanged (trimmed contents must equal the sentinel).
    //
    // Bounded to fit inside nextest's 180s cap for this test (measured: the
    // sentinel lands ~36s after the trigger, ~5s after the prompt is submitted).
    let sentinel = work.join(SENTINEL_NAME);
    if let Err(observed) =
        common::wait_for_file_trimmed_eq(&sentinel, SENTINEL_CONTENT, Duration::from_secs(60))
    {
        panic!(
            "the `clear = true` Codex worker never produced {SENTINEL_NAME:?} with the exact \
             contents {SENTINEL_CONTENT:?} — the delegated prompt was not delivered to (or not \
             acted on by) the respawned agent. observed: {observed}; \
             task_pointer_written={} orchestrator_log={:?}\nFinal grid:\n{}",
            task_pointer.exists(),
            orchestrator_log(),
            deck.snapshot_grid()
        );
    }

    // Soft narrative (NOT gating — the work-done leg is covered by
    // `codex/worker/001` and is too LLM-dependent to hard-gate here): did the
    // worker also follow the task file's completion footer back to the daemon?
    // Kept short so a worker that never signals cannot push the run past
    // nextest's 180s cap.
    let work_done = work
        .join(".dot-agent-deck")
        .join(format!("work-done-{WORKER_ROLE}.md"));
    eprintln!(
        "soft: worker signalled work-done through the hook socket = {}",
        common::wait_for_path(&work_done, Duration::from_secs(30))
    );
}
