#![cfg(all(feature = "e2e", feature = "e2e-live"))]

//! PTY-attached Codex wrapper coverage for PRD #20 M7. The synthetic case pins
//! deterministic plumbing; the real case runs a cheap Codex model against a
//! uniquely named fixture sentinel. Both assert the user-visible dashboard.

mod common;

use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::event::{
    AGENT_EVENT_SCHEMA_VERSION, AgentType, EventType, LiveTarget, TargetKind, Writable,
};
use spec::spec;

const SENTINEL_NAME: &str = "codex_sentinel_a7c91f.txt";
const INTERACTIVE_PROOF_NAME: &str = "codex-interactive-proof.txt";
/// Codex's empty-composer placeholder. Same needle, and the same reasoning for
/// matching only the meaningful part of it, as `tests/e2e_codex_hooks.rs`.
const CODEX_COMPOSER_READY: &str = "Ask Codex to do anything";

/// Upper bound, before [`common::load_scaled`], on the sandbox preflight's
/// `codex` calls together. Each is a Codex start with no model turn; measured at
/// about 0.3 s apiece on a quiet box (Codex 0.164.0, 2026-11-04).
const CODEX_SANDBOX_PROBE_BASE: Duration = Duration::from_secs(30);

/// The usage line of the `codex sandbox` form this preflight drives, which takes
/// the command to run directly (`codex sandbox [OPTIONS] [COMMAND]...`, Codex
/// 0.164.0). A Codex whose `sandbox --help` does not print it is treated as not
/// offering that form, and the preflight falls back to [`unshare_userns_probe`].
const CODEX_SANDBOX_USAGE: &str = "Usage: codex sandbox [OPTIONS] [COMMAND]...";

/// Whether Codex's Linux `workspace-write` sandbox can start a command on this
/// host, asked of Codex itself: `codex sandbox -c sandbox_mode="workspace-write"
/// -- /bin/true`, which spends no model turn. On a host where it cannot, the
/// shell commands and patches Codex attempts fail, so a test that needs Codex to
/// do real work there cannot pass whatever the deck does.
///
/// Asked of Codex rather than of the kernel because the answer depends on which
/// `bwrap` Codex picks. Traced on Ubuntu 26.04 with Codex 0.164.0
/// (`kernel.apparmor_restrict_unprivileged_userns=1`): with a `bwrap` on `PATH`
/// Codex runs it, and Ubuntu's shipped `bwrap-userns-restrict` AppArmor profile
/// lets `/usr/bin/bwrap` create the user namespace, so the sandbox works; with
/// none it runs its bundled copy, which that host refuses (`bwrap: setting up
/// uid map: Permission denied` run directly, `bwrap: loopback: Failed
/// RTM_NEWADDR: Operation not permitted` through Codex). The probe this replaced,
/// util-linux `unshare -Ur true`, is refused on that host whichever `bwrap` Codex
/// would use, so it skipped the test there for a reason Codex does not meet.
///
/// The probe runs the `codex` and the `bwrap` the deck's pane will resolve: the
/// `PATH` is [`common::path_with_binary_dir`], the one the test hands the deck,
/// which also leaves out the deck's telemetry shim. `HOME` and `CODEX_HOME` are a
/// harness tempdir, never the developer's `~/.codex`, and the cwd is a workspace
/// inside it, the directory `workspace-write` makes writable. The calls share one
/// deadline of [`common::load_scaled`] of [`CODEX_SANDBOX_PROBE_BASE`], and a call
/// still running there is killed by its own child handle.
///
/// A Codex without that `sandbox` form, as `sandbox --help` shows (its usage line
/// is not [`CODEX_SANDBOX_USAGE`], or `--help` fails), is judged by the old
/// `unshare` probe instead. Linux only, as before.
fn check_codex_sandbox_can_start() -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Ok(());
    }
    let path = common::path_with_binary_dir();
    let codex = std::env::split_paths(&path)
        .map(|dir| dir.join("codex"))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            format!(
                "no `codex` on the PATH the deck's pane gets ({path}), so its sandbox cannot \
                 be probed"
            )
        })?;
    let scratch = common::harness_tempdir()
        .map_err(|e| format!("could not create the Codex sandbox probe's home: {e}"))?;
    let codex_home = scratch.path().join(".codex");
    let workspace = scratch.path().join("workspace");
    for dir in [&codex_home, &workspace] {
        std::fs::create_dir(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    }
    let ceiling = common::load_scaled(CODEX_SANDBOX_PROBE_BASE);
    let deadline = std::time::Instant::now() + ceiling;
    let codex_call = |args: &[&str]| {
        let mut command = std::process::Command::new(&codex);
        command
            .args(args)
            .current_dir(&workspace)
            .env("PATH", &path)
            .env("HOME", scratch.path())
            .env("CODEX_HOME", &codex_home);
        common::run_codex_preflight(&mut command, deadline)
    };
    let probe_args = [
        "sandbox",
        "-c",
        "sandbox_mode=\"workspace-write\"",
        "--",
        "/bin/true",
    ];
    let shown = format!("{} {}", codex.display(), probe_args.join(" "));

    let (status, output) = codex_call(&probe_args)?;
    let observed = match status {
        Some(s) if s.success() => return Ok(()),
        Some(s) => format!("exited unsuccessfully ({s})"),
        // Judged as a refusal without asking `--help`: the shared deadline has
        // passed, so that call would be killed before it could answer.
        None => format!("did not finish within {ceiling:?}"),
    };
    if status.is_some() {
        let (help_status, help) = codex_call(&["sandbox", "--help"])?;
        if !help_status.is_some_and(|s| s.success()) || !help.contains(CODEX_SANDBOX_USAGE) {
            return unshare_userns_probe().map_err(|refused| {
                format!(
                    "this Codex does not offer `codex sandbox [COMMAND]...` (`codex sandbox \
                     --help` did not print {CODEX_SANDBOX_USAGE:?}), and the fallback probe \
                     refused: {refused}"
                )
            });
        }
    }
    let first_lines: Vec<&str> = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(5)
        .collect();
    Err(format!(
        "Codex's workspace-write sandbox cannot start a command on this host, so Codex cannot \
         run a shell or apply a patch here: `{shown}` {observed}. Its output began:\n{}",
        first_lines.join("\n")
    ))
}

/// The fallback for a Codex without the `codex sandbox [COMMAND]...` form:
/// whether this host lets an unprivileged process create a user namespace and
/// write its uid map, which a `bwrap` with no AppArmor allowance of its own
/// needs. Probed with util-linux's `unshare -Ur true`; a host without `unshare`
/// is not judged, so the test runs and speaks for itself. On a host that lets
/// only an allow-listed `bwrap` create user namespaces (Ubuntu's
/// `bwrap-userns-restrict`) this refuses even where Codex's sandbox works, which
/// is why it is only the fallback.
fn unshare_userns_probe() -> Result<(), String> {
    match std::process::Command::new("unshare")
        .args(["-Ur", "true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(status) if !status.success() => Err(
            "this host refuses unprivileged user namespaces (`unshare -Ur true` failed), so \
             Codex's workspace-write sandbox cannot start a shell or apply a patch here — \
             check `kernel.apparmor_restrict_unprivileged_userns`"
                .into(),
        ),
        _ => Ok(()),
    }
}

/// Scenario: Restore a pane running `dot-agent-deck wrap --agent codex` around
/// a deterministic shell stand-in that paints interactive-Codex-shaped text and
/// ends its turn through Codex's native `Stop` hook, as real Codex does.
/// Subscribe to the real daemon event stream and detach to the dashboard;
/// events must carry the Codex identity and schema version while the visible
/// card moves Thinking → Idle and reads `Codex`. Type input into the pane the
/// way a client does and require it to reach the wrapped child exactly once.
#[spec("codex/wrap/001")]
#[test]
fn codex_wrap_001_synthetic_codex_reaches_dashboard() {
    let command = "dot-agent-deck wrap --agent codex -- /bin/sh codex-standin.sh";
    let deck = TuiDeck::builder()
        .with_pty_size(180, 45)
        .with_env("PATH", common::path_with_binary_dir())
        .with_continue_session("", command)
        .launch_with_fixture("codex-synthetic");

    deck.wait_for_string("[Command Mode Ctrl+D]");
    let events = deck.subscribe_events();
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");

    // The stand-in sleeps 10 s before it paints anything, so a 15 s budget left
    // 5 s for the wrapper's boot and the event's round trip, which a starved
    // machine overran (seen while working on issue #1493).
    let working = events.wait_for(
        |event| event.agent_type == AgentType::Codex && event.event_type == EventType::Thinking,
        Duration::from_secs(40),
    );
    assert_eq!(working.schema_version, Some(AGENT_EVENT_SCHEMA_VERSION));
    assert_eq!(working.agent_type, AgentType::Codex);
    assert_eq!(
        working.live_target,
        Some(LiveTarget {
            kind: TargetKind::Pty,
            writable: Writable::Live,
        }),
        "a wrapper running inside a daemon-managed pane is backed by that live PTY, not a standalone history-only process"
    );
    let pane_id = working
        .pane_id
        .as_deref()
        .expect("managed wrapper event carries its pane id");
    let agent_id = working
        .agent_id
        .as_deref()
        .expect("managed wrapper event carries its agent id");
    // Held open until the child has the line: the daemon reads an attach
    // stream's input only after answering it, so closing at once can drop it.
    let mut input = common::AttachInput::attach(deck.attach_socket_path(), agent_id);
    input.write("MANAGED-WRAPPER-WRITE\r");
    assert!(
        common::wait_for_file_substr_count(
            &deck.workdir().join("managed-wrapper-input.log"),
            "MANAGED-WRAPPER-WRITE",
            1,
            Duration::from_secs(15),
        ),
        "the managed wrapper declared Live but the submitted write never reached its child"
    );
    drop(input);
    assert!(
        deck.wait_for_stream_string_within("Thinking", Duration::from_secs(10)),
        "the wrapped Codex card never visibly entered Thinking:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        deck.wait_for_grid_string_within("Codex ·", Duration::from_secs(10)),
        "the live dashboard card did not show the Codex identity:\n{}",
        deck.snapshot_grid()
    );

    // Issue #1493: a wrapper that could not get Codex's hooks trusted (no
    // `codex` on this host's PATH) also reports quiet output as a classified
    // Idle, so wait for the hook's.
    let idle = events.wait_for(
        |event| {
            event.agent_type == AgentType::Codex
                && event.event_type == EventType::Idle
                && !event.is_wrapper_output_classified()
        },
        Duration::from_secs(15),
    );
    // Issue #540: the turn ends through Codex's native `Stop` hook, as it does
    // for real Codex — the wrapper's classifier reads no printed line as Idle,
    // because the interactive TUI prints none that means it. The hook path
    // stamps no wrapper schema version; the wrapper's own events are checked
    // for that above.
    assert!(
        !idle.is_wrapper_output_classified() && idle.pane_id.as_deref() == Some(pane_id),
        "the Idle must be the native hook's, on this pane: {idle:?}"
    );
    assert!(
        deck.wait_for_grid_string_within("Idle", Duration::from_secs(10)),
        "the wrapped Codex card never visibly completed its turn:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: With Codex authentication copied into an isolated HOME, submit a
/// bare interactive `codex` command through the normal Ctrl+N new-pane flow on
/// the cheap model, type a prompt into its live pane, and wait for it to create a
/// proof file naming the fixture sentinel. Detach to the dashboard and observe
/// the automatically wrapped Codex card transition visibly from Thinking to Idle.
#[spec("codex/live/001")]
#[test]
fn codex_live_001_real_interactive_new_pane_runs_and_reports_status() {
    skip_unless!(common::check_codex_available());
    skip_unless!(check_codex_sandbox_can_start());

    let prompt = format!(
        "Use the shell to run pwd, then list exactly the directory it prints and confirm {SENTINEL_NAME} exists. Then write exactly {SENTINEL_NAME} followed by a newline to {INTERACTIVE_PROOF_NAME} in that same directory. Do not modify any other file."
    );
    let command = format!(
        "codex --model {} --sandbox workspace-write --ask-for-approval never -c 'model_reasoning_effort=\"low\"'",
        common::codex_test_model(),
    );
    let config_dir = common::harness_tempdir().expect("Codex new-pane config");
    let config_path = config_dir.path().join("config.toml");
    std::fs::write(&config_path, format!("default_command = {command:?}\n"))
        .expect("write bare Codex default command");
    let deck = TuiDeck::builder()
        .with_pty_size(180, 45)
        .with_env("PATH", common::path_with_binary_dir())
        .with_env("DOT_AGENT_DECK_CONFIG", config_path.to_string_lossy())
        .with_imported_codex_credentials()
        .launch_with_fixture("codex-live");

    deck.wait_for_string("No active agents");
    let events = deck.subscribe_events();
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("Tab: switch");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    // Case-insensitive: Codex paints the model's display name (`GPT-5.6-Luna`
    // for `--model gpt-5.6-luna`), not the id it was launched with.
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(30), |grid| grid
            .to_ascii_lowercase()
            .contains(&common::codex_test_model().to_ascii_lowercase())),
        "the bare interactive Codex UI never became ready in the new pane:\n{}",
        deck.snapshot_grid()
    );
    // Typing before Codex's composer has painted loses the payload, and the
    // first Enter after it is dropped while Codex is still initialising — both
    // measured and written up in `codex_hooks_001` (`tests/e2e_codex_hooks.rs`),
    // which gates and retries exactly as below. This test pressed Enter once, and
    // in each of three runs against Codex 0.156.1 on 2026-09-24 the prompt sat
    // unsubmitted in the composer until the proof-file wait expired.
    assert!(
        deck.wait_for_grid_string_within(CODEX_COMPOSER_READY, Duration::from_secs(90)),
        "Codex's composer never painted {CODEX_COMPOSER_READY:?} within 90s:\n{}",
        deck.snapshot_grid()
    );
    deck.send_keys(prompt.as_bytes());
    deck.wait_for_string(SENTINEL_NAME);
    // Retry on the OUTCOME: the placeholder returns only once the composer has
    // been emptied into a turn, and an Enter landing on an empty composer does
    // nothing, so a repeat after the submit is harmless.
    let submit_deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut attempts = 0_usize;
    let submitted = loop {
        deck.send_keys(b"\r");
        attempts += 1;
        if deck.wait_for_grid_string_within(CODEX_COMPOSER_READY, Duration::from_secs(2)) {
            break true;
        }
        if std::time::Instant::now() >= submit_deadline {
            break false;
        }
    };
    assert!(
        submitted,
        "after {attempts} Enter(s) over 60s the prompt is still sitting unsubmitted \
         in Codex's composer:\n{}",
        deck.snapshot_grid()
    );

    let thinking = events.wait_for(
        |event| event.agent_type == AgentType::Codex && event.event_type == EventType::Thinking,
        Duration::from_secs(120),
    );
    assert_eq!(thinking.agent_type, AgentType::Codex);
    // Polled on CONTENT, not existence: a shell redirect creates the proof file
    // before it writes into it, so waiting only for the path to appear can read
    // an empty string (PRD #225). Same exact-match semantics as before —
    // trimmed contents must equal the sentinel name.
    //
    // Bounded at 120s, not the old 180s: 180s IS nextest's kill window for the
    // whole test, so a wait that long can never actually elapse — the harness
    // kills the test first and the diagnostics below are never printed. 120s
    // leaves headroom for this assertion to fail *with* its message (measured
    // in isolation: the whole test is ~16s).
    let proof_path = deck.workdir().join(INTERACTIVE_PROOF_NAME);
    if let Err(observed) =
        common::wait_for_file_trimmed_eq(&proof_path, SENTINEL_NAME, Duration::from_secs(120))
    {
        panic!(
            "interactive Codex never completed the requested shell work; observed: {observed}; \
             final grid:\n{}",
            deck.snapshot_grid()
        );
    }
    deck.send_keys(b"/exit");
    deck.wait_for_string("/exit");
    deck.send_keys(b"\r");
    let idle = events.wait_for(
        |event| event.agent_type == AgentType::Codex && event.event_type == EventType::Idle,
        Duration::from_secs(120),
    );
    assert_eq!(idle.agent_type, AgentType::Codex);

    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");
    assert!(
        deck.wait_for_grid_string_within("Codex", Duration::from_secs(30)),
        "the automatically wrapped interactive session never rendered a Codex card:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        deck.wait_for_grid_string_within("Idle", Duration::from_secs(30)),
        "the live interactive Codex card never visibly completed its turn:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Run a deterministic terminal probe beneath `dot-agent-deck wrap`
/// in a daemon-managed pane, resize the outer PTY, send a line, and press Ctrl+C.
/// The child must see all three descriptors as TTYs, receive SIGWINCH plus input,
/// and observe SIGINT without losing transparent terminal behavior.
#[spec("codex/wrap/002")]
#[test]
fn codex_wrap_002_preserves_tty_resize_input_and_interrupt() {
    let command = "dot-agent-deck wrap --agent codex -- ./tty-probe.sh";
    let mut deck = TuiDeck::builder()
        .with_env("PATH", common::path_with_binary_dir())
        .with_continue_session("tty-probe", command)
        .launch_with_fixture("codex-tty-probe");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    let record = deck.workdir().join("tty-probe.log");
    // Wait for the probe's traps, not its first output: the isatty lines are
    // written BEFORE the traps are installed, and a resize landing in that gap
    // is delivered to a shell that still ignores SIGWINCH, so the WINCH wait
    // below failed under load.
    let started =
        common::wait_for_file_substr_count(&record, "TRAPS-READY", 1, Duration::from_secs(10));

    deck.resize(150, 50);
    let resized = common::wait_for_file_substr_count(&record, "WINCH", 1, Duration::from_secs(5));
    deck.send_keys(b"transparent-input\r");
    let input = common::wait_for_file_substr_count(
        &record,
        "INPUT=transparent-input",
        1,
        Duration::from_secs(5),
    );
    deck.send_keys(b"\x03");
    let interrupted = common::wait_for_file_substr_count(&record, "INT", 1, Duration::from_secs(5));
    let observed = std::fs::read_to_string(&record).unwrap_or_default();

    assert!(
        started,
        "the wrapped TTY probe never started; record={observed:?}"
    );
    assert!(
        observed.contains("isatty(0)=true")
            && observed.contains("isatty(1)=true")
            && observed.contains("isatty(2)=true"),
        "the wrapper must preserve TTY identity on stdin/stdout/stderr; observed:\n{observed}"
    );
    assert!(
        resized,
        "the wrapped child did not receive SIGWINCH after resize; observed:\n{observed}"
    );
    assert!(
        input,
        "ordinary input did not transparently reach the wrapped child; observed:\n{observed}"
    );
    assert!(
        interrupted,
        "Ctrl+C did not reach the wrapped child as SIGINT; observed:\n{observed}"
    );
}
