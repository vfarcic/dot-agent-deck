#![cfg(all(feature = "e2e", feature = "e2e-live"))]

//! PTY-attached Codex wrapper coverage for PRD #20 M7. The synthetic case pins
//! deterministic plumbing; the real case runs a cheap Codex model against a
//! uniquely named fixture sentinel. Both assert the user-visible dashboard.

mod common;

use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::event::{
    AGENT_EVENT_SCHEMA_VERSION, AgentType, EventType, LiveTarget, SendResult, TargetKind, Writable,
};
use spec::spec;

const SENTINEL_NAME: &str = "codex_sentinel_a7c91f.txt";
const INTERACTIVE_PROOF_NAME: &str = "codex-interactive-proof.txt";
/// Codex's empty-composer placeholder. Same needle, and the same reasoning for
/// matching only the meaningful part of it, as `tests/e2e_codex_hooks.rs`.
const CODEX_COMPOSER_READY: &str = "Ask Codex to do anything";

/// Whether this host lets an unprivileged process create a user namespace,
/// which Codex's Linux `workspace-write` sandbox needs (it runs its tools
/// under `bwrap`). A host that refuses — Ubuntu's
/// `kernel.apparmor_restrict_unprivileged_userns=1` is the measured case —
/// makes every shell command and every patch Codex attempts fail with
/// `bwrap: setting up uid map: Permission denied`, so a test that needs Codex
/// to do real work there cannot pass whatever the deck does. Probed with
/// util-linux's `unshare -Ur true`; a host without `unshare` is not judged, so
/// the test runs and speaks for itself.
fn check_codex_sandbox_can_start() -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Ok(());
    }
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

fn path_with_binary_dir() -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bin_dir = std::path::Path::new(bin)
        .parent()
        .expect("test binary has a parent dir")
        .to_str()
        .expect("binary directory is UTF-8");
    format!("{bin_dir}:{}", std::env::var("PATH").unwrap_or_default())
}

/// Scenario: Restore a pane running `dot-agent-deck wrap --agent codex` around
/// a deterministic shell stand-in that emits realistic Codex JSONL turn-start
/// and turn-completed records. Subscribe to the real daemon event stream and
/// detach to the dashboard; events must carry the Codex identity and schema
/// version while the visible card moves Thinking → Idle and reads `Codex`.
/// Send identity-bound input and require it to reach the wrapped child exactly once.
#[spec("codex/wrap/001")]
#[test]
fn codex_wrap_001_synthetic_jsonl_reaches_dashboard() {
    let command = "dot-agent-deck wrap --agent codex -- /bin/sh codex-standin.sh";
    let deck = TuiDeck::builder()
        .with_pty_size(180, 45)
        .with_env("PATH", path_with_binary_dir())
        .with_continue_session("", command)
        .launch_with_fixture("codex-synthetic");

    deck.wait_for_string("[Command Mode Ctrl+D]");
    let events = deck.subscribe_events();
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");

    let working = events.wait_for(
        |event| event.agent_type == AgentType::Codex && event.event_type == EventType::Thinking,
        Duration::from_secs(15),
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
    let response = common::write_and_submit_with_identity_on(
        deck.attach_socket_path(),
        pane_id,
        "MANAGED-WRAPPER-WRITE",
        agent_id,
        Some(&working.session_id),
    )
    .expect("write through managed wrapper pane");
    assert_eq!(
        response.send_result,
        Some(SendResult::Applied),
        "dashboard writes to a managed wrapped Codex pane must be applied to its live PTY"
    );
    assert!(
        common::wait_for_file_substr_count(
            &deck.workdir().join("managed-wrapper-input.log"),
            "MANAGED-WRAPPER-WRITE",
            1,
            Duration::from_secs(15),
        ),
        "the managed wrapper declared Live but the submitted write never reached its child"
    );
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

    let idle = events.wait_for(
        |event| event.agent_type == AgentType::Codex && event.event_type == EventType::Idle,
        Duration::from_secs(15),
    );
    assert_eq!(idle.schema_version, Some(AGENT_EVENT_SCHEMA_VERSION));
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
        "Use the shell to list the current directory and confirm {SENTINEL_NAME} exists. Then write exactly {SENTINEL_NAME} followed by a newline to {INTERACTIVE_PROOF_NAME}. Do not modify any other file."
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
        .with_env("PATH", path_with_binary_dir())
        .with_env("DOT_AGENT_DECK_CONFIG", config_path.to_string_lossy())
        .with_imported_codex_credentials()
        .launch_with_fixture("codex-live");

    deck.wait_for_string("No active sessions");
    let events = deck.subscribe_events();
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("Tab: switch");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    assert!(
        deck.wait_for_grid_string_within(common::codex_test_model(), Duration::from_secs(30)),
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
        .with_env("PATH", path_with_binary_dir())
        .with_continue_session("tty-probe", command)
        .launch_with_fixture("codex-tty-probe");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    let record = deck.workdir().join("tty-probe.log");
    let started =
        common::wait_for_file_substr_count(&record, "isatty(2)=", 1, Duration::from_secs(10));

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
