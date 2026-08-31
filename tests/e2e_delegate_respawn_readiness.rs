#![cfg(all(feature = "e2e", feature = "e2e-live"))]

//! PTY-attached real-agent coverage for PRD #249's `clear = true` delegate path.
//!
//! The deterministic slow-readiness stand-in in `orchestration/delegate/012`
//! pins the race itself. These tests cover the user-visible happy path that a
//! stand-in cannot: the real deck opens an orchestration tab, a real interactive
//! worker boots in its role pane, the production delegate CLI respawns it, its
//! native hook reports submission and real status, and the worker acts on the
//! delegated task.
//!
//! Both tests are local-only, flaky-tolerant pre-PR e2es. They intentionally
//! have no reel marker because PRD #249 is a bug fix rather than a showcase.

mod common;

use std::path::Path;
use std::time::Duration;

use common::{TuiDeck, TuiDeckBuilder};
use dot_agent_deck::event::{AgentType, EventType};
use dot_agent_deck::state::DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS;
use spec::spec;

const ORCH_NAME: &str = "delegate-respawn";
const WORKER_ROLE: &str = "coder";
const DELEGATE_TRIGGER: &str = "delegate-now";
const DELEGATE_TASK_FILE: &str = "delegate-task.md";
const ORCHESTRATOR_SCRIPT: &str = "orchestrator-delegate.sh";
const ORCHESTRATOR_LOG: &str = "orchestrator-delegate.log";

const CLAUDE_MODEL: &str = "claude-haiku-4-5-20251001";
const CLAUDE_SENTINEL: &str = "prd249-claude-respawn-4d37c1.txt";
const CLAUDE_SENTINEL_CONTENT: &str = "PRD249_CLAUDE_RESPAWN_OK";

const OPENCODE_SENTINEL: &str = "prd249-opencode-respawn-8a62f4.txt";
const OPENCODE_SENTINEL_CONTENT: &str = "PRD249_OPENCODE_RESPAWN_OK";

/// Test-only forwarding seam for the M2 observation run, and since issue #243
/// round 4 for re-bracketing the no-signal buffer on a machine the shipped value
/// was not measured on. `TuiDeck` scrubs the host environment, so setting this
/// variable on `cargo test-e2e delegate_015` explicitly maps its value to the
/// production readiness-buffer variable in the spawned deck and daemon,
/// REPLACING the 8000 ms `/015` pins below. Normal runs leave it unset and get
/// that pin, which mirrors `state::NO_SIGNAL_READINESS_BUFFER`.
const E2E_READINESS_BUFFER_OVERRIDE: &str = "DOT_AGENT_DECK_E2E_DELEGATE_READINESS_BUFFER_MS";

const ORCHESTRATOR_BODY: &str = r#"#!/bin/sh
while [ ! -f delegate-now ]; do sleep 0.2; done
dot-agent-deck delegate --to coder --task-file delegate-task.md \
    >> orchestrator-delegate.log 2>&1
printf 'delegate exit=%s\n' "$?" >> orchestrator-delegate.log
exec cat > /dev/null
"#;

struct RealDelegateCase<'a> {
    agent_name: &'a str,
    agent_type: AgentType,
    input_ready_needle: &'a str,
    sentinel_name: &'a str,
    sentinel_content: &'a str,
    /// Issue #243: the maximum time this case's worker may take to get from the
    /// delegate being released to the task pointer being submitted inside the
    /// replacement agent — or `None` to assert only that it happens.
    ///
    /// `Some` exactly where the agent's readiness path is one #243 CHANGED and
    /// where the test would otherwise pass identically on the fixed and the
    /// broken path. That is OpenCode (`/015`): it declares
    /// `PrePromptReadiness::NoSignal`, so before #243 every `clear = true`
    /// delegate to it sat out the full 30 s `SESSION_START_WAIT_TIMEOUT` waiting
    /// for an event measured never to arrive, and the timeout fallback delivered.
    /// Every assertion in the shared body below held throughout that, which is
    /// precisely the problem — a silent regression to the dead wait ships green.
    ///
    /// `None` for Claude Code (`/014`) deliberately, not by omission. Claude
    /// declares `NativeSessionStart` and its gate is byte-for-byte what it was
    /// before #243: wait for the genuine `SessionStart` (which really does arrive
    /// early in boot), then hold for the readiness buffer. There is no dead wait
    /// to regress into, so a bound there would guard nothing while adding a
    /// timing constraint to a real-LLM test. Claude is #243's healthy BASELINE —
    /// the 3.80 / 3.85 / 3.96 / 4.39 s end-to-end delegates its budgets are
    /// derived from — not one of its victims.
    delegate_to_submit_budget: Option<Duration>,
}

/// Issue #243: `/015`'s bound, derived from both ends the same way
/// `orchestration/delegate/029`'s `READY_TO_POINTER_BUDGET` is.
///
/// *Below:* under the 30 s `SESSION_START_WAIT_TIMEOUT`, and a full 11 s under
/// the ~31 s (timeout + readiness buffer) the pre-fix path burned before the
/// fallback wrote anything — `orchestration/delegate/030` measures exactly that
/// 31 s for this agent's configuration in virtual time, and the issue's own
/// scheduler measurement of an OpenCode cold spawn was 30.3 s. So a run that
/// still pays the dead wait cannot pass this, which is the whole reason the
/// bound exists.
///
/// *Above:* the deck's own contribution is small and known — the orchestrator
/// script's 0.2 s trigger poll, one `dot-agent-deck delegate` CLI round trip,
/// the `clear = true` respawn, and then exactly the 8000 ms readiness buffer
/// this test pins via `DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS`, mirroring
/// the `NO_SIGNAL_READINESS_BUFFER` a declared-`NoSignal` agent now resolves in
/// production. Measured directly: `delegate→submit ≈ buffer + 0.26 s`, so the
/// deck's own share of this leg is **257 ms** and everything else inside the
/// budget is the buffer plus real OpenCode booting far enough to consume the
/// keystrokes it was handed and post `session.prompt`.
///
/// **The caveat this used to carry is discharged.** For two rounds it read
/// "nobody has measured how long a REPLACEMENT OpenCode takes to reach that
/// point, since before #243 it always had the whole 30 s wait to boot in and now
/// it has 1 s". That was measured on 2026-08-26, first as a red run of this test
/// at 1000 ms and then, properly, across **176 runs** against a real
/// `opencode --model … --auto` 1.18.23 — and the answer is a single observable
/// boundary: the instant OpenCode paints its `Ask anything...` composer.
/// Written before it the payload is gone; written after it, every run delivered.
/// That boundary is **2.5 s on an idle box, 4.5 s with the cores
/// oversubscribed, and 12 s at 4x oversubscription**, so the replacement's
/// requirement is now a number rather than an open question. It is answered on
/// the PRODUCT side, in `state::NO_SIGNAL_READINESS_BUFFER`, which is where the
/// full derivation lives — this constant only has to stay clear of it.
///
/// **The bound stays 20 s and must not be widened.** At the shipped 8000 ms the
/// leg costs ~8.3 s, leaving 11.7 s of margin, and the 20 s figure keeps doing
/// the one job it was created for: it is a full 11 s under the ~31 s a run that
/// still pays the dead wait would take, so a silent regression to the timeout
/// fallback cannot pass. Widening it past ~25 s stops separating the two paths
/// and makes this test green on precisely the defect it exists to catch. If a
/// slower box needs more room, the answer is the operator override
/// (`DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS`, which this test already
/// pins), not a bigger budget — and note the two move together, since the buffer
/// is paid inside this leg.
///
/// **What the 2026-08-26 measurement found, kept because it is why the product
/// changed.** With the 1000 ms `DELEGATE_READINESS_BUFFER` — the ONLY thing then
/// standing between a `clear = true` respawn and the write for a
/// `PrePromptReadiness::NoSignal` agent — the pointer was written into a
/// replacement OpenCode still bringing its TUI up and the bytes were swallowed
/// outright. Not parked: **zero of 176 runs** left the payload sitting in the
/// composer, so this is PRD #225's Defect 1 shape (a write into a line
/// discipline that is not yet the agent's) rather than #663's unsubmitted-payload
/// shape, and a longer interval introduces no second failure mode here. The
/// worker never entered `Thinking`, never ran a tool and never created the
/// sentinel; the run died at the 120 s status wait with a pane idle since boot.
/// This bound was never reached on those runs — the deck delivered promptly
/// (`delegate exit=0` and the pointer file on disk inside 5 s, every run) — which
/// is why widening it would not have moved the failure by a millisecond and why
/// it was not widened.
///
/// The fix is `df11513`: a third default, `NO_SIGNAL_READINESS_BUFFER` at
/// 8000 ms, for exactly the declared-`NoSignal` path — 1.78x the contended
/// requirement and 3.2x the idle one, **19/19 delivered** at the shipped value.
/// It is a PRODUCT change, not a test-tuning one; this test's part is to pin
/// that value at the seam and to keep the 20 s bound that tells a prompt
/// delivery from the dead wait.
const OPENCODE_DELEGATE_TO_SUBMIT_BUDGET: Duration = Duration::from_secs(20);

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

fn delegate_task(case: &RealDelegateCase<'_>) -> String {
    format!(
        "Create the file {name} in the current working directory with the exact contents \
         {contents} and no trailing newline. Use the shell to run exactly this command: \
         printf '{contents}' > {name}. Do not modify any other file. That is the entire task.",
        name = case.sentinel_name,
        contents = case.sentinel_content,
    )
}

fn open_orchestration(deck: &TuiDeck) {
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C");
    deck.wait_for_absence("Command:");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
}

fn maybe_forward_readiness_override(builder: TuiDeckBuilder) -> TuiDeckBuilder {
    match std::env::var(E2E_READINESS_BUFFER_OVERRIDE) {
        Ok(value) => builder.with_env(DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS, value),
        Err(_) => builder,
    }
}

fn run_real_clear_true_delegate(deck: TuiDeck, worker_command: &str, case: RealDelegateCase<'_>) {
    deck.wait_for_string("No active sessions");

    let work = deck.workdir().to_path_buf();
    std::fs::write(
        work.join(".dot-agent-deck.toml"),
        orchestration_toml(worker_command),
    )
    .expect("write delegate orchestration config");
    std::fs::write(work.join(DELEGATE_TASK_FILE), delegate_task(&case))
        .expect("write delegated task body");
    write_executable(&work.join(ORCHESTRATOR_SCRIPT), ORCHESTRATOR_BODY);

    let events = deck.subscribe_events();
    open_orchestration(&deck);
    deck.wait_for_string(WORKER_ROLE);

    // The orchestration opens focused on its start role. Detach, then jump to
    // the second role card so the real worker TUI itself is visibly on screen.
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Pane Ctrl+N]");
    deck.send_bytes(b"2");
    assert!(
        deck.wait_for_grid_string_within(case.input_ready_needle, Duration::from_secs(120)),
        "the REAL interactive {} worker never became visibly input-ready (missing {:?}) \
         before delegation; this is a boot/auth failure, not a delegate failure.\nFinal grid:\n{}",
        case.agent_name,
        case.input_ready_needle,
        deck.snapshot_grid()
    );

    // Return to the role-card surface before releasing the orchestrator. The
    // status sequence below is captured only from this point forward, so it is
    // the delegated turn rather than boot-time card paint.
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Pane Ctrl+N]");
    // Issue #243: stamped so the submission wait below can be scoped to events
    // broadcast AFTER the delegate, and so the latency bound has an anchor. The
    // FIRST worker has been up for a while by now and `EventSub::wait_for` scans
    // everything collected since the subscription opened, so an unscoped
    // predicate could match a pre-delegate event and measure nonsense.
    let delegate_released_at = chrono::Utc::now();
    std::fs::write(work.join(DELEGATE_TRIGGER), "").expect("release delegate trigger");

    // Real native status on the user-visible card: prompt submission enters
    // Thinking, the requested shell operation enters Working, and its tool name
    // renders. This rolling-stream wait starts after the trigger, so old frames
    // cannot satisfy it.
    deck.wait_for_strings_in_order_then_any_within(
        &["Thinking", "Working"],
        &["Bash", "bash"],
        Duration::from_secs(120),
    );

    // Native hook proof that the pointer was submitted inside the replacement
    // agent, rather than merely echoed into a booting PTY. Both Claude Code's
    // UserPromptSubmit hook and OpenCode's session.prompt event populate this
    // field with the actual submitted text.
    let submitted = events.wait_for(
        |event| {
            event.event_type == EventType::Thinking
                && event.agent_type == case.agent_type
                && event.timestamp >= delegate_released_at
                && event
                    .user_prompt
                    .as_deref()
                    .is_some_and(|prompt| prompt.contains(&format!("worker-task-{WORKER_ROLE}.md")))
        },
        Duration::from_secs(90),
    );
    assert!(
        submitted
            .user_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.contains(&format!("worker-task-{WORKER_ROLE}.md"))),
        "the REAL {} worker emitted a prompt event, but it was not the delegated task pointer: {:?}",
        case.agent_name,
        submitted.user_prompt
    );

    // Issue #243: the delegate must also be PROMPT, for the cases where #243
    // changed what "prompt" means. Everything above is satisfied by the pre-fix
    // path too — the 30 s fallback delivered the pointer eventually and the
    // worker acted on it — so for an agent that used to pay a dead wait, this is
    // the ONLY assertion that tells the fixed path from the broken one. See
    // `RealDelegateCase::delegate_to_submit_budget` for why `/014` carries none.
    //
    // Measured between the test's own stamp and the daemon's event timestamp
    // rather than on a wall clock read here, because `EventSub::wait_for` returns
    // from a buffer that may already hold the match — the grid wait above it
    // would otherwise be counted into the interval.
    if let Some(budget) = case.delegate_to_submit_budget {
        let delegate_to_submit = submitted.timestamp - delegate_released_at;
        assert!(
            delegate_to_submit
                <= chrono::Duration::from_std(budget)
                    .expect("delegate_to_submit_budget fits a chrono Duration"),
            "the delegated pointer reached the REAL {} worker {delegate_to_submit} after the \
             delegate was released, against a budget of {budget:?}. This agent declares it emits \
             NO pre-prompt readiness signal, so the gate should skip straight to the bounded \
             readiness buffer — a delay in this range means it is waiting out the 30 s \
             SESSION_START_WAIT_TIMEOUT for an event that cannot arrive, and only the fallback is \
             delivering (issue #243). released={delegate_released_at:?} submitted={:?}",
            case.agent_name,
            submitted.timestamp
        );
    }

    let sentinel = work.join(case.sentinel_name);
    if let Err(observed) =
        common::wait_for_file_trimmed_eq(&sentinel, case.sentinel_content, Duration::from_secs(90))
    {
        let task_pointer = work
            .join(".dot-agent-deck")
            .join(format!("worker-task-{WORKER_ROLE}.md"));
        let orchestrator_log = std::fs::read_to_string(work.join(ORCHESTRATOR_LOG))
            .unwrap_or_else(|error| format!("<unreadable: {error}>"));
        panic!(
            "the REAL {} worker never created {:?} with exact contents {:?}; observed: {}; \
             task_pointer_written={} orchestrator_log={:?}\nFinal grid:\n{}",
            case.agent_name,
            case.sentinel_name,
            case.sentinel_content,
            observed,
            task_pointer.exists(),
            orchestrator_log,
            deck.snapshot_grid()
        );
    }
}

/// Scenario: Open an orchestration through the real PTY-attached deck with a `clear = true` worker running interactive Claude Code on Haiku, visibly wait for its prompt editor, and release a script that invokes the real delegate CLI. The replacement worker must submit its task pointer, visibly traverse Thinking and Working with Bash, and create the uniquely named sentinel requested by the delegated task.
#[spec("orchestration/delegate/014")]
#[test]
#[cfg(unix)]
fn delegate_014_real_claude_worker_acts_on_clear_true_delegate() {
    skip_unless!(common::check_claude_available());

    // `Write` is here for the footer, not the assertion (#303). The sentinel is
    // created with Bash, so this test would still pass without it — but the task
    // file's `## When done` footer then sends the worker into a `Write` approval
    // prompt it cannot answer, and this case is `[reel]`-marked, so that dialog
    // would be the last thing on its recorded cast.
    let worker_command = format!("claude --model {CLAUDE_MODEL} --allowedTools Bash Read Write");
    let deck = TuiDeck::builder()
        .with_pty_size(180, 45)
        .with_env("PATH", path_with_binary_dir())
        .with_env(DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS, "1000")
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .launch_with_fixture("minimal");

    run_real_clear_true_delegate(
        deck,
        &worker_command,
        RealDelegateCase {
            agent_name: "Claude Code",
            agent_type: AgentType::ClaudeCode,
            input_ready_needle: "? for shortcuts",
            sentinel_name: CLAUDE_SENTINEL,
            sentinel_content: CLAUDE_SENTINEL_CONTENT,
            // Deliberately unbounded — see the field's own doc comment. Claude's
            // readiness path is untouched by #243 and is its healthy baseline,
            // not one of its victims.
            delegate_to_submit_budget: None,
        },
    );
}

/// Scenario: Open an orchestration through the real PTY-attached deck with a `clear = true` worker running interactive OpenCode on a cheap mini model, visibly wait for its TUI, and release a script that invokes the real delegate CLI. The replacement worker must submit its task pointer, visibly traverse Thinking and Working with its shell tool, and create the uniquely named sentinel requested by the delegated task; the run pins the shipped 8000 ms no-signal readiness buffer, and a test-only env seam can repoint that buffer to any other value so the same scenario can be re-bracketed on a slower or busier box. The submission must also land within twenty seconds of the delegate being released (issue #243), which is the one assertion that separates the fixed path from the dead wait an agent declaring no pre-prompt readiness signal used to pay: every other assertion here held under that 30 s `SessionStart` timeout too.
#[spec("orchestration/delegate/015")]
#[test]
#[cfg(unix)]
fn delegate_015_real_opencode_worker_acts_on_clear_true_delegate() {
    skip_unless!(common::check_opencode_available());

    let worker_command = format!("opencode --model {} --auto", common::opencode_test_model());
    let builder = TuiDeck::builder()
        .with_pty_size(180, 45)
        .with_env("PATH", path_with_binary_dir())
        // MIRRORS `state::NO_SIGNAL_READINESS_BUFFER` (8000 ms), which is the
        // default a declared-`NoSignal` agent resolves in production. Pinning is
        // not optional here and REMOVING the pin is the wrong repair: the
        // harness base pins this variable to `0` for every `TuiDeck`
        // (`tests/common/mod.rs`), so an unpinned run buys no buffer at all and
        // fails harder than a mis-sized one. Kept as a literal for the same
        // reason `/014` keeps `1000` (Claude's shipped `DELEGATE_READINESS_BUFFER`)
        // and `orchestration/delegate/009` keeps `5000` in its own
        // `READINESS_BUFFER_MS` (the shipped `WRAPPER_INTERFACE_READINESS_BUFFER`)
        // — all three constants are `pub(crate)`, so the mirrors are maintained
        // by hand; change this whenever that constant changes.
        .with_env(DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS, "8000")
        .with_imported_opencode_credentials();
    let deck = maybe_forward_readiness_override(builder).launch_with_fixture("minimal");

    run_real_clear_true_delegate(
        deck,
        &worker_command,
        RealDelegateCase {
            agent_name: "OpenCode",
            agent_type: AgentType::OpenCode,
            input_ready_needle: "Ask anything...",
            sentinel_name: OPENCODE_SENTINEL,
            sentinel_content: OPENCODE_SENTINEL_CONTENT,
            delegate_to_submit_budget: Some(OPENCODE_DELEGATE_TO_SUBMIT_BUDGET),
        },
    );
}
