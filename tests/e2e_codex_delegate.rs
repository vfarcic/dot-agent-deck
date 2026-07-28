#![cfg(feature = "e2e")]

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
//! delivery reached the agent. A consequence worth knowing: for Codex the M3
//! readiness gate never fast-paths, every `clear = true` delegate pays the full
//! timeout fallback (documented in `docs/develop/agent-adapters.md`).
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
/// appeared. Reel-eligible (PTY-attached real agent, records a
/// `full-stream.cast`); flaky-tolerant (real LLM) — run once, not looped.
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
    std::fs::write(work.join(DELEGATE_TRIGGER), "").expect("release the orchestrator's delegate");

    let orchestrator_log = || {
        std::fs::read_to_string(work.join(ORCHESTRATOR_LOG))
            .unwrap_or_else(|e| format!("<unreadable: {e}>"))
    };
    let task_pointer = work
        .join(".dot-agent-deck")
        .join(format!("worker-task-{WORKER_ROLE}.md"));

    // The event PRD #225's readiness gate is about, now where it can actually
    // happen: the respawned worker's GENUINE native Codex `SessionStart` — no
    // `wrapper_fork` origin marker, so it is Codex itself and not the wrapper's
    // fork-time card-surfacing event. It arrives only once the injected pointer
    // starts a turn, so seeing it at all means the daemon respawned the worker
    // and delivered into the live agent. A miss panics with every observed
    // event, which distinguishes "the delegate never reached the daemon" from
    // "it did, but nothing was ever submitted inside Codex".
    events.wait_for(
        |event| {
            event.event_type == EventType::SessionStart
                && event.agent_type == AgentType::Codex
                && !event.is_wrapper_fork_session_start()
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

    // The load-bearing assertion: the worker ACTED on the delegated prompt.
    // Bounded to fit inside nextest's 180s cap for this test (measured: the
    // sentinel lands ~36s after the trigger, ~5s after the prompt is submitted).
    let sentinel = work.join(SENTINEL_NAME);
    let created = common::wait_for_path(&sentinel, Duration::from_secs(60));
    assert!(
        created,
        "the `clear = true` Codex worker never created {SENTINEL_NAME:?} — the delegated \
         prompt was not delivered to (or not acted on by) the respawned agent. \
         task_pointer_written={} orchestrator_log={:?}\nFinal grid:\n{}",
        task_pointer.exists(),
        orchestrator_log(),
        deck.snapshot_grid()
    );
    assert_eq!(
        std::fs::read_to_string(&sentinel)
            .expect("read the delegated sentinel")
            .trim(),
        SENTINEL_CONTENT,
        "the Codex worker created the sentinel with unexpected contents"
    );

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
