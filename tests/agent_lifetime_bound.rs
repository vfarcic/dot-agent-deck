#![cfg(unix)]

//! Issue #668: the wrapped agent child's lifetime bound, on the spawn path the
//! harness actually uses.
//!
//! `wrap` has carried a hard bound on its child's process group since #661 —
//! [`arm_child_group_backstop`] forks a reaper that holds the deadline
//! independently of the wrapper, so an uncatchable `SIGKILL` of the wrapper no
//! longer strands the child. That backstop is env-gated on
//! `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS`, exactly so a production wrapper
//! forks nothing.
//!
//! The gap this file guards is that the gate was never *armed* on the path most
//! of the suite spawns through. `TuiDeck` and `DaemonProc` pin the variable
//! explicitly after their `env_clear`, but the in-process `AgentPtyRegistry`
//! path — 15 test files, including every `spawn_inprocess_daemon` caller —
//! inherited a test-process environment that had never had the variable in it.
//! Measured on a live wrapper spawned by `delegate_007`: zero `MAX_LIFETIME`
//! matches anywhere in its `/proc/<pid>/environ`. So #661 was mis-armed rather
//! than unreachable, and the fix is one variable pinned once in
//! `common::init_test_env` — `agent_pty::spawn` scrubs named deck vars but does
//! not `env_clear`, so a single value in the test process reaches every child of
//! every spawn shape, present and future.
//!
//! Named for what it contains, not for the issue that produced it (CLAUDE.md
//! rule 3).
//!
//! [`arm_child_group_backstop`]: https://github.com/vfarcic/dot-agent-deck/pull/661

mod common;

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use dot_agent_deck::agent_pty::{AgentPtyRegistry, SpawnOptions};

/// A hook endpoint nothing can ever be listening on, so a probe wrapper's events
/// cannot post into a developer's live deck. Mirrors `tests/wrap_io.rs`.
const UNREACHABLE_HOOK_SOCKET: &str = "/nonexistent/dot-agent-deck-lifetime-tests.sock";

/// The variable that arms both `arm_wrap_self_defense` (the wrapper) and #661's
/// `arm_child_group_backstop` (the wrapped child's group).
const MAX_LIFETIME_VAR: &str = "DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS";

/// What the stand-in writes when the variable is missing from its environment —
/// a value that can never be confused with a parsed cap.
const UNSET_MARKER: &str = "<unset>";

fn write_executable(path: &std::path::Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, contents).expect("write synthetic agent executable");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod synthetic agent executable");
}

/// `$PATH` with the stand-in's directory and the freshly-built deck ahead of the
/// ambient one, so `codex` resolves to the stand-in and the `wrap` rewrite
/// resolves to the build under test rather than an installed release.
fn path_with_built_deck(bin_dir: &std::path::Path) -> String {
    let deck_dir = std::path::Path::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .parent()
        .expect("built deck binary has a parent directory");
    format!(
        "{}:{}:{}",
        bin_dir.display(),
        deck_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// Scenario: Spawn a Codex stand-in through a bare in-process `AgentPtyRegistry`
/// exactly as the suite's 15 registry-driven test files do, with the stand-in
/// recording its own `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS` to a file before it
/// blocks. Assert the recorded value is a real cap, so the wrapper underneath it
/// arms both its own self-defence and #661's child-group reaper.
///
/// Issue #668, the regression guard for the *arming* gap — the half no
/// behavioural test can catch, because a child that dies of its own hangup dies
/// whether or not a cap was ever armed. Deliberately an environment assertion
/// rather than a timing one: it costs milliseconds and cannot flake. Reading the
/// child's environment is also literally how the gap was diagnosed.
#[test]
fn in_process_registry_spawn_arms_the_wrapped_child_lifetime_bound() {
    common::init_test_env();

    let fixture = common::harness_tempdir().expect("create lifetime-arming fixture");
    let bin_dir = fixture.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("create synthetic Codex bin dir");
    let record = fixture.path().join("cap.txt");
    write_executable(
        &bin_dir.join("codex"),
        // `${VAR-<unset>}` (not `:-`) so an empty value is reported as empty
        // rather than silently reading as absent. `exec cat` keeps the stand-in
        // alive and is the exact shape whose orphans #668 censused.
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"${{{MAX_LIFETIME_VAR}-{UNSET_MARKER}}}\" > \"$CAP_RECORD\"\nexec cat\n"
        ),
    );

    let cwd = fixture.path().to_string_lossy().into_owned();
    let registry = Arc::new(AgentPtyRegistry::new());
    let agent_id = registry
        .spawn_agent(SpawnOptions {
            command: Some("codex"),
            cwd: Some(&cwd),
            env: vec![
                ("PATH".to_string(), path_with_built_deck(&bin_dir)),
                ("CAP_RECORD".to_string(), record.display().to_string()),
                (
                    "DOT_AGENT_DECK_SOCKET".to_string(),
                    UNREACHABLE_HOOK_SOCKET.to_string(),
                ),
            ],
            ..SpawnOptions::default()
        })
        .expect("spawn wrapped Codex stand-in through the in-process registry");

    let recorded = common::wait_until(Duration::from_secs(20), || record.is_file())
        .then(|| std::fs::read_to_string(&record).unwrap_or_default())
        .map(|s| s.trim().to_string());

    // Tear the registry down before asserting, so a failure cannot also leak the
    // very orphan this file is about.
    registry.shutdown_all();

    let recorded = recorded.unwrap_or_else(|| {
        panic!("the wrapped Codex stand-in (agent {agent_id}) never recorded its environment")
    });
    let parsed = recorded.parse::<u64>().ok().filter(|secs| *secs > 0);
    assert!(
        parsed.is_some(),
        "a wrapped agent spawned through the in-process registry carries \
         {MAX_LIFETIME_VAR}={recorded:?}, so `arm_wrap_self_defense` and #661's \
         `arm_child_group_backstop` are both no-ops for it. Every path that ends \
         the wrapper without letting it reap then strands the child, `setsid`'d \
         into its own session where nothing above it can signal its group \
         (issue #668)."
    );
}

/// Drive one `trap`-armoured probe under a wrapper that is then SIGKILL'd, and
/// report whether the child was still alive at the end of `budget`.
///
/// `cap` is passed through to the wrapper's environment verbatim; `None` removes
/// the variable, which is the control case that shows the cap is what does the
/// work here.
fn term_and_hup_resistant_child_survives(cap: Option<&str>, budget: Duration) -> bool {
    let fixture = common::harness_tempdir().expect("create resistant-child fixture");
    let pid_path = fixture.path().join("child.pid");

    let mut command = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
    command
        .args([
            "wrap",
            "--agent",
            "codex",
            "--",
            "/bin/sh",
            "-c",
            // Ignored dispositions survive `exec`, so the loop below inherits
            // both traps. A shell loop rather than `exec cat` on purpose: `cat`
            // ends on its terminal hanging up whatever its signal dispositions
            // are, which is the fd fix's doing and not this test's subject. This
            // child reads nothing, so no hangup can reach it and only a signal
            // can end it.
            "trap '' TERM; trap '' HUP; \
             printf '%s\\n' \"$$\" > \"$WRAP_CHILD_PID_FILE\"; \
             while :; do sleep 1; done",
        ])
        .env("WRAP_CHILD_PID_FILE", &pid_path)
        .env("DOT_AGENT_DECK_SOCKET", UNREACHABLE_HOOK_SOCKET)
        .env_remove("DOT_AGENT_DECK_EXIT_WHEN_ORPHANED")
        .env_remove("DOT_AGENT_DECK_PANE_ID")
        .env_remove("DOT_AGENT_DECK_AGENT_ID")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match cap {
        Some(secs) => command.env(MAX_LIFETIME_VAR, secs),
        None => command.env_remove(MAX_LIFETIME_VAR),
    };

    let mut wrapper = command.spawn().expect("spawn resistant-child probe");
    let read_pid = |path: &std::path::Path| -> Option<libc::pid_t> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|contents| contents.trim().parse().ok())
    };
    let recorded = common::wait_until(Duration::from_secs(10), || read_pid(&pid_path).is_some());
    if !recorded {
        let _ = wrapper.kill();
        let _ = wrapper.wait();
        panic!("wrapper never recorded its child's pid");
    }
    let child_pid = read_pid(&pid_path).expect("child pid recorded");

    // SIGKILL, not SIGTERM: the wrapper gets no chance to reap, so the only
    // thing that can still end the child is a bound the child's own group owns.
    let wrapper_pid = wrapper.id() as libc::pid_t;
    // SAFETY: the wrapper pid came from this test's live `Child`; ending it
    // uncleanly is the behavior under test.
    assert_eq!(
        unsafe { libc::kill(wrapper_pid, libc::SIGKILL) },
        0,
        "deliver SIGKILL to wrapper pid {wrapper_pid}"
    );
    let _ = wrapper.wait();

    let gone = common::wait_until(budget, || !common::process_running(child_pid));

    // Never leak this test's own probe, whatever the outcome above — otherwise a
    // regression in the code under test would itself mint the orphans #668 is
    // about.
    if common::process_running(child_pid) {
        // SAFETY: best-effort cleanup of a pid this test created.
        unsafe {
            libc::kill(-child_pid, libc::SIGKILL);
            libc::kill(child_pid, libc::SIGKILL);
        }
    }
    !gone
}

/// Scenario: Run a wrapped child that ignores SIGTERM and SIGHUP and never reads
/// its terminal, then SIGKILL the wrapper so no reap loop runs. With no lifetime
/// cap in the environment it must still be alive after the budget; with a
/// one-second cap it must be gone inside it.
///
/// Issue #668: this is the test that says out loud why the cap is kept once the
/// fd fix lands. The fd fix ends a wrapped child by hanging its terminal up, and
/// that covers the whole measured orphan population — but a child that ignores
/// SIGHUP and reads nothing cannot be hung up, and this repo really spawns that
/// shape (`tests/idle_worker_detector.rs`'s `trap '' TERM; exec cat`). For those,
/// the only thing left is #661's forked reaper, and the only thing that arms it
/// is the variable `common::init_test_env` now pins.
#[test]
fn a_term_resistant_wrapped_child_is_still_bounded_by_the_cap() {
    // Control: nothing armed, so nothing can end it. Kept deliberately short —
    // it is asserting survival, so every second is spent proving a negative.
    assert!(
        term_and_hup_resistant_child_survives(None, Duration::from_secs(5)),
        "precondition failed: a TERM/HUP-ignoring wrapped child that reads \
         nothing died with no lifetime cap armed, so the assertion below would \
         pass for a reason other than the cap"
    );
    // Armed: 1 s cap + one 250 ms backstop poll + WRAP_TERMINATE_GRACE (1.5 s).
    // 30 s is loose headroom for a loaded host, not an expected duration.
    assert!(
        !term_and_hup_resistant_child_survives(Some("1"), Duration::from_secs(30)),
        "a SIGKILL'd wrapper stranded a TERM/HUP-ignoring child that no hangup \
         can reach, with the lifetime cap armed — this is the orphan that pins \
         its e2e temp root against `clean-e2e-tmp` forever (issues #657, #668)"
    );
}
