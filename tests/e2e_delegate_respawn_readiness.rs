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
use dot_agent_deck::agent_pty::TabMembership;
use dot_agent_deck::delegate_retry::{
    DEFAULT_RETRY_SCHEDULE_MS, DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS,
};
use dot_agent_deck::event::{AgentType, EventType};
use dot_agent_deck::state::DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS;
use spec::spec;

const ORCH_NAME: &str = "delegate-respawn";
const WORKER_ROLE: &str = "coder";
const DELEGATE_TRIGGER: &str = "delegate-now";
const DELEGATE_TASK_FILE: &str = "delegate-task.md";
const ORCHESTRATOR_SCRIPT: &str = "orchestrator-delegate.sh";
const ORCHESTRATOR_LOG: &str = "orchestrator-delegate.log";
/// Issue #1243: the launcher a [`RealDelegateCase::declared_launcher`] case puts
/// in front of its worker. Its basename names no agent, exactly like `devbox`.
const WORKER_LAUNCHER: &str = "run-worker.sh";

const CLAUDE_MODEL: &str = "claude-haiku-4-5-20251001";
const CLAUDE_SENTINEL: &str = "prd249-claude-respawn-4d37c1.txt";
const CLAUDE_SENTINEL_CONTENT: &str = "PRD249_CLAUDE_RESPAWN_OK";

const OPENCODE_SENTINEL: &str = "prd249-opencode-respawn-8a62f4.txt";
const OPENCODE_SENTINEL_CONTENT: &str = "PRD249_OPENCODE_RESPAWN_OK";
const OPENCODE_RETRY_SENTINEL: &str = "delegate-retry-fixture-6d3e9a.sentinel";

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
    /// Grid substrings meaning "this agent's composer is up and listening",
    /// matched ANY-of: the first one to reach the rendered grid opens the gate.
    ///
    /// **Every member must be free of decorative glyphs, and that is the rule
    /// this field exists to carry rather than a style preference.** Issues #878
    /// / #921: this was a single `&str` pinned to `"Ask anything..."` — three
    /// ASCII periods, bytes `2e 2e 2e` — while OpenCode paints `Ask anything…`
    /// with one U+2026, bytes `e2 80 a6`. The wait underneath is a plain
    /// `contains`, so the two could never match: `/015` burned its full 120 s
    /// and died in this PRECONDITION on a run where OpenCode had booted fine
    /// and was visibly sitting at its composer, having never delegated
    /// anything. Nothing in CI runs this file (lane 2, CLAUDE.md rule 5), so it
    /// stayed red from whenever OpenCode changed that one glyph.
    ///
    /// Swapping in `"Ask anything…"` would have been exactly as fragile — the
    /// next upstream composer touch drifts it again, and the run after that is
    /// another 120 s wait pointing at credentials. So each needle is the part of
    /// the string that carries the MEANING and the decoration is left out of it.
    /// The slice is any-of so a future rendering that is *not* a substring of
    /// the current one can be ADDED here rather than replacing the one that
    /// still works on someone else's version.
    input_ready_needles: &'a [&'a str],
    sentinel_name: &'a str,
    sentinel_content: &'a str,
    /// The maximum time this case's worker may take to get from the delegate
    /// being released to the task pointer being submitted inside the
    /// replacement agent — or `None` to assert only that it happens.
    ///
    /// `Some` for OpenCode (`/015`), whose readiness gate is a fixed hold
    /// because it declares `PrePromptReadiness::NoSignal`: the bound is that
    /// hold plus two in-place re-sends (issue #1381; see
    /// [`opencode_delegate_to_submit_budget`]). It was introduced by #243 to
    /// catch a regression to the 30 s `SESSION_START_WAIT_TIMEOUT` dead wait;
    /// `/015` now checks that in the daemon log instead, because a recovered
    /// delivery takes about as long.
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
    /// Issue #1243: `Some(agent)` runs the worker through [`WORKER_LAUNCHER`], a
    /// script that `exec`s the real command, and declares `agent = "<agent>"` on
    /// the role — the `devbox run oc-big` shape this repository's own config was
    /// measured paying the full 30 s readiness timeout under on every delegation,
    /// because the deck could not see the agent behind the launcher. `None`
    /// runs the command directly.
    declared_launcher: Option<&'a str>,
}

/// `/015`'s bound on delegate release → task pointer submitted: the no-signal
/// hold in force ([`opencode_hold_ms`]), the first two waits of the shipped re-send
/// schedule, and 10 s of slack for the probe grace, the echo gate and OpenCode
/// posting `session.prompt`.
///
/// **Issue #1381 changed what this bound is for.** Until then it was 20 s and
/// had one job: sit a full 11 s under the ~31 s a run still paying #243's dead
/// wait (the 30 s `SessionStart` timeout plus the buffer) would take, so a
/// regression to the fallback could not pass. That only worked because the test
/// ran with the in-place re-send off, which is a path production no longer has
/// — and on a loaded box it went red with #1381's own symptom, the pointer lost
/// in OpenCode's boot with nothing to recover it. Now the re-send is on, a
/// recovered delivery lands about 30 s after release, inside the range the old
/// bound used to separate, so the dead-wait check moved to the daemon's own log
/// (`delegate_015` asserts the declared-no-signal line and the absence of the
/// timeout fallback). What is left for this bound is the #1381 guarantee itself:
/// the task reaches the worker within the hold and two re-sends.
///
/// The 2026-08-26 measurement that sized the hold is in
/// `state::NO_SIGNAL_READINESS_BUFFER`: OpenCode accepts input from its
/// `Ask anything` paint (quoted without the trailing ellipsis, per issues
/// #878/#921), measured at 2.5 s idle, 4.5 s contended and 12 s at 4x
/// oversubscription.
fn opencode_delegate_to_submit_budget() -> Duration {
    let [first, second, _] = DEFAULT_RETRY_SCHEDULE_MS;
    Duration::from_millis(opencode_hold_ms() + first + second + 10_000)
}

/// The no-signal hold `/015` runs with: its 8000 ms pin, mirroring
/// `state::NO_SIGNAL_READINESS_BUFFER`, or the value
/// [`E2E_READINESS_BUFFER_OVERRIDE`] forwards in its place.
fn opencode_hold_ms() -> u64 {
    std::env::var(E2E_READINESS_BUFFER_OVERRIDE)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(8000)
}

/// The shipped re-send schedule, spelled out because the harness base pins the
/// variable to `0`. That an unset variable resolves to it is pinned at L1 by
/// `retry_schedule_unset_is_the_default`.
fn default_retry_schedule() -> String {
    DEFAULT_RETRY_SCHEDULE_MS.map(|ms| ms.to_string()).join(",")
}

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

fn orchestration_toml(worker_command: &str, declared_agent: Option<&str>) -> String {
    let agent_line = declared_agent
        .map(|agent| format!("agent = {agent:?}\n"))
        .unwrap_or_default();
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
         {agent_line}\
         clear = true\n"
    )
}

/// The sentinel is named by its absolute path: measured on 2026-10-03, a real
/// OpenCode worker on a starved box ran the requested `printf` in the parent of
/// its working directory, so "the current working directory" left the model a
/// choice the assertion does not.
fn delegate_task(case: &RealDelegateCase<'_>, work: &Path) -> String {
    let path = work.join(case.sentinel_name);
    format!(
        "Create the file {path} with the exact contents {contents} and no trailing newline. \
         Use the shell to run exactly this command: printf '{contents}' > {path}. Do not \
         modify any other file. That is the entire task.",
        path = path.display(),
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

fn run_real_clear_true_delegate(deck: &TuiDeck, worker_command: &str, case: RealDelegateCase<'_>) {
    deck.wait_for_string("No active agents");

    let work = deck.workdir().to_path_buf();
    let role_command = match case.declared_launcher {
        Some(_) => {
            write_executable(
                &work.join(WORKER_LAUNCHER),
                &format!("#!/bin/sh\nexec {worker_command}\n"),
            );
            let launcher = format!("./{WORKER_LAUNCHER}");
            assert_eq!(
                AgentType::from_command(Some(&launcher)),
                None,
                "control: the launcher must hide the agent from command inference, or this is \
                 not the configuration issue #1243 measured"
            );
            launcher
        }
        None => worker_command.to_string(),
    };
    std::fs::write(
        work.join(".dot-agent-deck.toml"),
        orchestration_toml(&role_command, case.declared_launcher),
    )
    .expect("write delegate orchestration config");
    std::fs::write(work.join(DELEGATE_TASK_FILE), delegate_task(&case, &work))
        .expect("write delegated task body");
    write_executable(&work.join(ORCHESTRATOR_SCRIPT), ORCHESTRATOR_BODY);

    let events = deck.subscribe_events();
    open_orchestration(deck);
    deck.wait_for_string(WORKER_ROLE);

    // The orchestration opens focused on its start role. Detach, then jump to
    // the second role card so the real worker TUI itself is visibly on screen.
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Agent Ctrl+N]");
    deck.send_bytes(b"2");
    // Any-of over `input_ready_needles`, via the existing grid-predicate helper
    // so this stays in the test file rather than widening shared harness surface.
    let needles = case.input_ready_needles;
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(120), |grid| needles
            .iter()
            .any(|needle| grid.contains(needle))),
        "the REAL interactive {name} worker never showed an input-ready marker within 120 s \
         (none of {needles:?} reached the rendered grid), so this run stopped in a PRECONDITION \
         and never delegated anything.\n\
         \n\
         Two unrelated causes land on this line and the grid below tells them apart — read it \
         before concluding either (issues #878/#921 were misdiagnosed for exactly this reason, \
         because this message used to assert the first cause outright):\n\
         \n\
         * The grid carries no {name} UI at all, or a CLI/auth/model error: the agent never \
         booted. Credentials, the CLI on PATH, or an unreachable model.\n\
         * The grid shows a live {name} UI sitting at its composer: the agent is FINE and the \
         marker has drifted upstream. Read `RealDelegateCase::input_ready_needles`' doc comment \
         before touching it — the repair is not to paste in whatever glyph you now see.\n\
         \n\
         Final grid:\n{grid}",
        name = case.agent_name,
        needles = needles,
        grid = deck.snapshot_grid()
    );

    // Return to the role-card surface before releasing the orchestrator. The
    // status sequence below is captured only from this point forward, so it is
    // the delegated turn rather than boot-time card paint.
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Agent Ctrl+N]");
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
             delegate was released, against a budget of {budget:?}: the readiness buffer plus the \
             first two in-place re-sends. Neither the first write nor either re-send was taken \
             up, so the task was lost to this worker for longer than delivery recovery allows \
             (issues #1381, #1383). released={delegate_released_at:?} submitted={:?}",
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
        &deck,
        &worker_command,
        RealDelegateCase {
            agent_name: "Claude Code",
            agent_type: AgentType::ClaudeCode,
            input_ready_needles: &["? for shortcuts"],
            sentinel_name: CLAUDE_SENTINEL,
            sentinel_content: CLAUDE_SENTINEL_CONTENT,
            // Deliberately unbounded — see the field's own doc comment. Claude's
            // readiness path is untouched by #243 and is its healthy baseline,
            // not one of its victims.
            delegate_to_submit_budget: None,
            declared_launcher: None,
        },
    );
}

/// Scenario: Open an orchestration through the real PTY-attached deck with a `clear = true` worker running interactive OpenCode on a cheap mini model through a launcher script the deck cannot see through, declared `agent = "opencode"` on the role (issue #1243), visibly wait for its TUI, and release a script that invokes the real delegate CLI. The replacement worker must submit its task pointer, visibly traverse Thinking and Working with its shell tool, and create the uniquely named sentinel requested by the delegated task; the run pins the shipped 8000 ms no-signal readiness buffer with the shipped in-place re-send on, and a test-only env seam can repoint that buffer to any other value so the same scenario can be re-bracketed on a slower or busier box. The submission must land within the buffer plus two re-sends (issue #1381), the daemon log must show the declared-no-signal hold and no 30 s `SessionStart` fallback (issue #243), and OpenCode's transcript must hold the task pointer as exactly one user turn.
#[spec("orchestration/delegate/015")]
#[test]
#[cfg(unix)]
fn delegate_015_real_opencode_worker_acts_on_clear_true_delegate() {
    skip_unless!(common::check_opencode_available());

    let worker_command = format!("opencode --model {} --auto", common::opencode_test_model());
    let log_dir = common::harness_tempdir().expect("delegate daemon log directory");
    let daemon_log_path = log_dir.path().join("delegate-retry.log");
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
        // Issue #1381: the in-place re-send production runs, which the harness
        // base pins off. Without it this test exercised a delivery path no user
        // has: on a loaded box the 8 s hold alone loses the pointer, and only
        // the re-send recovers it.
        .with_env(
            DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS,
            default_retry_schedule(),
        )
        // The readiness decision's own debug lines are what tell the fixed path
        // from #243's dead wait now that the budget allows for a re-send.
        .with_env(
            "DOT_AGENT_DECK_LOG",
            daemon_log_path.to_str().expect("UTF-8 log path"),
        )
        .with_env("RUST_LOG", "dot_agent_deck::state=debug")
        .with_imported_opencode_credentials();
    let deck = maybe_forward_readiness_override(builder).launch_with_fixture("minimal");
    let _preserve_log = PreserveDelegateLogOnFailure(daemon_log_path.clone());

    run_real_clear_true_delegate(
        &deck,
        &worker_command,
        RealDelegateCase {
            agent_name: "OpenCode",
            agent_type: AgentType::OpenCode,
            // Deliberately NOT "Ask anything..." and deliberately NOT
            // "Ask anything…" — issues #878/#921. The trailing ellipsis is
            // decoration that has already drifted once between OpenCode
            // releases, and matching either spelling only re-arms the same
            // 120 s dead wait. See `RealDelegateCase::input_ready_needles`.
            input_ready_needles: &["Ask anything"],
            sentinel_name: OPENCODE_SENTINEL,
            sentinel_content: OPENCODE_SENTINEL_CONTENT,
            delegate_to_submit_budget: Some(opencode_delegate_to_submit_budget()),
            // Issue #1243: behind a launcher, declared — the configuration that
            // was measured paying the 30 s fallback on every delegation. The
            // bare-binary form's type inference is pinned at L1 by
            // `orchestration/delegate/030`'s bare arm, so this spends the one
            // real OpenCode turn on the shape that actually broke. Undeclared,
            // the readiness checks below fail on the 30 s `SessionStart` wait.
            declared_launcher: Some("opencode"),
        },
    );

    // Issue #243, by the daemon's own account rather than by elapsed time: the
    // declared-no-signal path skipped the dead wait and held for the pinned
    // buffer, and the 30 s `SessionStart` fallback never ran.
    let daemon_log = std::fs::read_to_string(&daemon_log_path)
        .unwrap_or_else(|error| format!("<daemon log unavailable: {error}>"));
    let expected_buffer = format!("buffer_ms={}", opencode_hold_ms());
    assert!(
        daemon_log.lines().any(|line| line
            .contains("holding the task prompt for the no-signal readiness buffer")
            && line.contains(&expected_buffer)),
        "the delegate did not take the declared-no-signal path with {expected_buffer}, so the \
         role's `agent` declaration did not reach the readiness decision (issues #243, #1243); \
         log={daemon_log}"
    );
    assert!(
        !daemon_log
            .lines()
            .any(|line| line.contains("SessionStart wait timed out")),
        "the delegate waited out SESSION_START_WAIT_TIMEOUT for an event OpenCode cannot send \
         and delivered through the fallback (issue #243); log={daemon_log}"
    );
    // A recovered delivery is still one task: OpenCode's transcript holds the
    // pointer as exactly one user turn, however many copies the deck sent.
    let transcript_prompts =
        opencode_user_prompt_count(deck.home_dir(), "Read .dot-agent-deck/worker-task-coder.md")
            .expect("OpenCode transcript must be readable for this test");
    let probes = daemon_log
        .lines()
        .filter(|line| line.contains("pressed Enter first"))
        .count();
    let retypes = daemon_log
        .lines()
        .filter(|line| line.contains("re-typed the pointer into the same process"))
        .count();
    eprintln!(
        "delegate_015: {probes} submit-only probe(s) and {retypes} pointer re-type(s) before the \
         worker's first turn; {transcript_prompts} user turn(s) for the task pointer"
    );
    assert_eq!(
        transcript_prompts, 1,
        "OpenCode transcript contains {transcript_prompts} user turns for this task pointer \
         ({probes} probe(s), {retypes} re-type(s))"
    );
}

fn worker_agent_id(deck: &TuiDeck) -> Option<String> {
    common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|record| {
            matches!(
                &record.tab_membership,
                Some(TabMembership::Orchestration { role_name, .. })
                    if role_name == WORKER_ROLE
            )
        })
        .map(|record| record.id)
}

struct PreserveDelegateLogOnFailure(std::path::PathBuf);

impl Drop for PreserveDelegateLogOnFailure {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let target = common::current_test_recordings_dir();
            if std::fs::create_dir_all(&target).is_ok() {
                let _ = std::fs::copy(&self.0, target.join("delegate-retry.log"));
            }
        }
    }
}

fn opencode_user_prompt_count(home: &std::path::Path, pointer: &str) -> Result<usize, String> {
    let db = home.join(".local/share/opencode/opencode.db");
    let script = r#"import json, sqlite3, sys
db = sqlite3.connect(sys.argv[1])
pointer = sys.argv[2]
count = 0
for message, part in db.execute('select message.data, part.data from part join message on part.message_id = message.id'):
    message, part = json.loads(message), json.loads(part)
    if message.get('role') == 'user' and part.get('type') == 'text' and pointer in part.get('text', ''):
        count += 1
print(count)
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script])
        .arg(&db)
        .arg(pointer)
        .output()
        .map_err(|error| {
            format!(
                "could not read OpenCode transcript {}: {error}",
                db.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "could not query OpenCode transcript {}: {}",
            db.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<usize>()
        .map_err(|error| format!("OpenCode transcript count was invalid: {error}"))
}

/// Scenario: Open a PTY-attached orchestration with a real interactive OpenCode worker, then delegate while its replacement is still booting by removing the readiness buffer. The worker must report a uniquely named fixture file through `work-done` in the same process; after its first Thinking or ToolStart proof, the deck must never probe or retype, and the OpenCode transcript must contain exactly one user task prompt.
#[spec("orchestration/delegate/046")]
#[test]
#[cfg(unix)]
fn delegate_046_real_opencode_recovers_early_pointer_in_place() {
    skip_unless!(common::check_opencode_available());

    let worker_command = format!("opencode --model {} --auto", common::opencode_test_model());
    let log_dir = common::harness_tempdir().expect("delegate retry log directory");
    let daemon_log_path = log_dir.path().join("delegate-retry.log");
    let deck = TuiDeck::builder()
        .with_pty_size(180, 45)
        .with_env("PATH", path_with_binary_dir())
        .with_env(
            "DOT_AGENT_DECK_LOG",
            daemon_log_path.to_str().expect("UTF-8 log path"),
        )
        .with_env(DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS, "0")
        // The shipped schedule (issue #1381), so a red here means production
        // would miss this boot too. A shorter test-only schedule (5/15/30 s)
        // ran out before OpenCode booted on a starved box, 3 of 3 on
        // 2026-10-03, which is a horizon no user has.
        .with_env(
            DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS,
            default_retry_schedule(),
        )
        // Above this test's 360 s nextest window (`.config/nextest.toml`), so
        // a slow run fails on its own assertion instead of its daemon being
        // reaped at the 300 s harness default mid-wait (Qodo on PR #1523).
        .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "420")
        .with_imported_opencode_credentials()
        .launch_with_fixture("minimal");
    let _preserve_log = PreserveDelegateLogOnFailure(daemon_log_path.clone());
    deck.wait_for_string("No active agents");

    let work = deck.workdir();
    std::fs::write(
        work.join(OPENCODE_RETRY_SENTINEL),
        "fixture for the real worker\n",
    )
    .expect("write uniquely named fixture sentinel");
    std::fs::write(
        work.join(".dot-agent-deck.toml"),
        orchestration_toml(&worker_command, None),
    )
    .expect("write real OpenCode orchestration config");
    std::fs::write(
        work.join(DELEGATE_TASK_FILE),
        format!(
            "Use your shell to list the files in {}. Find the one filename beginning with \
             delegate-retry-fixture- and ending with .sentinel. Then run dot-agent-deck \
             work-done --task with a short report containing that exact filename. Do not guess \
             the filename and do not stop before work-done succeeds.\n",
            work.display()
        ),
    )
    .expect("write delegated file-list task");
    write_executable(&work.join(ORCHESTRATOR_SCRIPT), ORCHESTRATOR_BODY);

    open_orchestration(&deck);
    deck.wait_for_string(WORKER_ROLE);
    assert!(
        common::wait_until(Duration::from_secs(20), || worker_agent_id(&deck).is_some()),
        "the real worker role never acquired a daemon record; grid:\n{}",
        deck.snapshot_grid()
    );
    let initial_id = worker_agent_id(&deck).expect("initial worker checked above");

    // The initial OpenCode need not finish booting. Delegation respawns it with
    // clear=true, and the zero buffer sends the first pointer into that new
    // process as early as the deck permits.
    std::fs::write(work.join(DELEGATE_TRIGGER), "").expect("release delegate trigger");
    let delegate_log = work.join(ORCHESTRATOR_LOG);
    assert!(
        common::wait_until(Duration::from_secs(30), || {
            std::fs::read_to_string(&delegate_log).is_ok_and(|log| log.contains("delegate exit=0"))
        }),
        "the real delegate CLI did not finish; orchestrator_log={:?}; grid:\n{}",
        std::fs::read_to_string(&delegate_log).unwrap_or_default(),
        deck.snapshot_grid()
    );
    assert!(
        common::wait_until(Duration::from_secs(20), || {
            worker_agent_id(&deck).is_some_and(|id| id != initial_id)
        }),
        "replacement worker never acquired a new daemon record; grid:\n{}",
        deck.snapshot_grid()
    );
    let replacement_id = worker_agent_id(&deck).expect("replacement worker checked above");
    let replacement_pane = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|record| record.id == replacement_id)
        .and_then(|record| record.pane_id_env)
        .expect("replacement worker has a pane id");
    assert_ne!(
        initial_id, replacement_id,
        "clear=true must replace the first worker before the task pointer is sent"
    );

    // Focus the real worker pane so the recording and failure grid show what
    // OpenCode did with the delegated pointer, not only the role card.
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Agent Ctrl+N]");
    deck.send_bytes(b"2");
    let work_done = work.join(".dot-agent-deck/work-done-coder.md");
    let completed = common::wait_until(Duration::from_secs(240), || {
        std::fs::read_to_string(&work_done)
            .is_ok_and(|report| report.contains(OPENCODE_RETRY_SENTINEL))
    });
    let daemon_log = std::fs::read_to_string(&daemon_log_path)
        .unwrap_or_else(|error| format!("<daemon log unavailable: {error}>"));
    let retry_lines: Vec<&str> = daemon_log
        .lines()
        .filter(|line| line.contains("re-typed the pointer into the same process"))
        .collect();
    let submit_probes = daemon_log
        .lines()
        .filter(|line| line.contains("pressed Enter first"))
        .count();
    let delivery_evidence: Vec<&str> = daemon_log
        .lines()
        .filter(|line| {
            line.contains("delegate retry: no proof the worker received its task pointer")
                || (line.contains("Received event") && line.contains("UserPromptSubmit"))
                || line.contains("Received ack")
        })
        .collect();
    let log_lines: Vec<&str> = daemon_log.lines().collect();
    let proof_index = log_lines.iter().position(|line| {
        line.contains("Received event")
            && line.contains(&replacement_pane)
            && (line.contains("event_type=Thinking") || line.contains("event_type=ToolStart"))
    });
    eprintln!("delegate_046 delivery evidence: {delivery_evidence:?}");
    assert!(
        completed,
        "the REAL OpenCode worker did not report the listed sentinel through work-done; \
         first pointer was sent during boot, in-place retries={}; report={:?}; \
         orchestrator_log={:?}; retry_lines={retry_lines:?}; grid:\n{}",
        retry_lines.len(),
        std::fs::read_to_string(&work_done).unwrap_or_default(),
        std::fs::read_to_string(&delegate_log).unwrap_or_default(),
        deck.snapshot_grid()
    );
    assert_eq!(
        worker_agent_id(&deck).as_deref(),
        Some(replacement_id.as_str()),
        "the worker must complete inside the same process that received the first pointer"
    );
    let proof_index = proof_index.unwrap_or_else(|| {
        panic!(
            "no Thinking or ToolStart proof matched worker pane {replacement_pane}; \
             delivery_evidence={delivery_evidence:?}; log={daemon_log}"
        )
    });
    let after_proof: Vec<&str> = log_lines[proof_index + 1..]
        .iter()
        .copied()
        .filter(|line| {
            line.contains("re-typed the pointer into the same process")
                || line.contains("pressed Enter first")
        })
        .collect();
    assert!(
        after_proof.is_empty(),
        "pointer retype or Enter-only probe followed first worker proof \
         ({:?}); later retries={after_proof:?}; log={daemon_log}",
        log_lines[proof_index]
    );
    let transcript_prompts =
        opencode_user_prompt_count(deck.home_dir(), "Read .dot-agent-deck/worker-task-coder.md")
            .expect("OpenCode transcript must be readable for this test");
    assert_eq!(
        transcript_prompts, 1,
        "OpenCode transcript contains {transcript_prompts} user turns for this task pointer"
    );
    eprintln!(
        "delegate_046: {submit_probes} submit-only probe(s) and {} in-place pointer \
         re-delivery attempt(s) occurred before work-done; first proof={:?}; \
         a retype {} the likely landing copy",
        retry_lines.len(),
        log_lines[proof_index],
        if retry_lines.is_empty() {
            "was not"
        } else {
            "was"
        }
    );
}
