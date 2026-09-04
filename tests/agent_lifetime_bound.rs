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
//! than unreachable, and the fix is one variable pinned once by
//! `tests/common/child_lifetime_bound.rs`'s `arm()` — `agent_pty::spawn` scrubs
//! named deck vars but does not `env_clear`, so a single value in the test
//! process reaches every child of every spawn shape, present and future.
//!
//! This file reaches it through `common::init_test_env()`, which calls the same
//! `arm()`. The three spawning files that deliberately do not link the harness
//! (`rehydration.rs`, `daemon_protocol.rs`, `shell_activity.rs`)
//! `#[path]`-include that one small file instead, and linkage-check rule 10
//! fails the build when a file under `tests/` builds an `AgentPtyRegistry` or
//! calls `run_daemon_with` without arming either way.
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

/// The per-spawn tag `agent_pty::spawn` and `wrap` export beside the cap, which
/// is how a reaper finds a descendant that has left the process group it was
/// armed on (issue #861). Named here rather than imported so this file states
/// the wire name it depends on, like [`MAX_LIFETIME_VAR`] above.
const LIFETIME_TAG_VAR: &str = "DOT_AGENT_DECK_TEST_LIFETIME_TAG";

/// The largest cap a child may **inherit**, mirroring
/// `child_lifetime_bound::CHILD_MAX_LIFETIME_SECS`. Keeping an ambient value
/// down here is a deletion-safety measure rather than a preference: every cap
/// is a window in which an orphan may still be writing under a temp root
/// `cargo xtask clean-e2e-tmp --apply` might reap.
///
/// Inherit, not carry: the clamp bounds *ambient* values only. A harness path
/// that re-pins the variable after an `env_clear` — `TuiDeck`'s `extra_env`,
/// applied last — bypasses it deliberately, and #665 uses that to pin 900 on
/// `orchestration_dispatch_002`. What keeps the reaper safe against those
/// explicit pins is not this ceiling but linkage-check rule 11, which fails the
/// build when any pin under `tests/` exceeds
/// `clean_tmp::MAX_PINNED_ORPHAN_CAP_SECS` — the number the reaper's dead-owner
/// floor is derived from (issue #679, which closed the 600-900 s window this
/// comment used to point at).
const MAX_LIFETIME_CEILING_SECS: u64 = 300;

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
    // The UPPER bound, which is the half `cargo xtask clean-e2e-tmp` depends on
    // and which "is it positive?" cannot see. That reaper deletes a root whose
    // owning test process is dead once the root is 30 minutes old, and picks
    // that as 2x the LONGEST cap written anywhere under `tests/`
    // (`clean_tmp::MAX_PINNED_ORPHAN_CAP_SECS`) — so a descendant entitled to
    // keep writing for longer than that turns the reaper's margin into a
    // deficit. An ambient `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS=3600` in a
    // developer's shell reached the child unchallenged before `clamped()`
    // existed, and this run proves it no longer does. Ambient is the whole
    // scope: this spawn inherits the test process's environment, so it is the
    // clamped path. A `TuiDeck` child whose `with_env` re-pins the cap after
    // its `env_clear` is not, by design — that path is guarded by
    // linkage-check rule 11 instead (issue #679).
    assert!(
        parsed.is_some_and(|secs| secs <= MAX_LIFETIME_CEILING_SECS),
        "a wrapped agent spawned through the in-process registry carries \
         {MAX_LIFETIME_VAR}={recorded:?}, above the {MAX_LIFETIME_CEILING_SECS} s \
         ceiling this file mirrors from `child_lifetime_bound`. `--apply` can \
         then delete a root out from under this child while it is still \
         nominally entitled to write there — `clean-e2e-tmp`'s dead-owner floor \
         bounds the caps `tests/` WRITES, not one a shell exports \
         (issue #668)."
    );
}

/// Scenario: Spawn a stand-in named `claude` — a `NativeHooks` agent, so
/// `wrap_launch_command` leaves it bare — through a bare in-process
/// `AgentPtyRegistry`, with the stand-in recording its own
/// `DOT_AGENT_DECK_TEST_LIFETIME_TAG` to a file before it blocks. Assert it
/// carries a non-empty tag, so the reaper armed for that pane has something to
/// find its `setsid`'d descendants by.
///
/// Issue #861, and the agent this is named for is the point. The orphan measured
/// there was a Claude Code Bash-tool shell, and Claude Code is **not** wrapped:
/// only `IntegrationStrategy::Wrapper` agents are (Codex), so `wrap`'s reaper
/// and `wrap`'s tag injection are both absent from that pane's spawn. The tag
/// therefore has to come from `agent_pty::spawn`, which every launch path
/// funnels through, and this is the test that says so — the test above covers
/// the wrapped path and would stay green if the unwrapped one lost its tag
/// entirely.
///
/// Deliberately an environment assertion rather than a timing one, for the same
/// reason as the test above: it costs milliseconds, it cannot flake, and reading
/// a child's environment is literally how #861 was diagnosed.
#[test]
fn an_unwrapped_agent_spawn_carries_a_lifetime_tag() {
    common::init_test_env();

    let fixture = common::harness_tempdir().expect("create lifetime-tag fixture");
    let bin_dir = fixture.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("create synthetic Claude bin dir");
    let record = fixture.path().join("tag.txt");
    write_executable(
        &bin_dir.join("claude"),
        // `${VAR-<unset>}` (not `:-`) so an empty value reports as empty rather
        // than silently reading as absent — an empty tag would build a needle
        // that matches every process on the box, which is the one outcome worse
        // than none.
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"${{{LIFETIME_TAG_VAR}-{UNSET_MARKER}}}\" > \"$TAG_RECORD\"\nexec cat\n"
        ),
    );

    let cwd = fixture.path().to_string_lossy().into_owned();
    let registry = Arc::new(AgentPtyRegistry::new());
    let agent_id = registry
        .spawn_agent(SpawnOptions {
            command: Some("claude"),
            cwd: Some(&cwd),
            env: vec![
                ("PATH".to_string(), path_with_built_deck(&bin_dir)),
                ("TAG_RECORD".to_string(), record.display().to_string()),
                (
                    "DOT_AGENT_DECK_SOCKET".to_string(),
                    UNREACHABLE_HOOK_SOCKET.to_string(),
                ),
            ],
            ..SpawnOptions::default()
        })
        .expect("spawn unwrapped Claude stand-in through the in-process registry");

    let recorded = common::wait_until(Duration::from_secs(20), || record.is_file())
        .then(|| std::fs::read_to_string(&record).unwrap_or_default())
        .map(|s| s.trim().to_string());

    // Tear the registry down before asserting, so a failure cannot also leak the
    // very orphan this file is about.
    registry.shutdown_all();

    let recorded = recorded.unwrap_or_else(|| {
        panic!("the unwrapped Claude stand-in (agent {agent_id}) never recorded its environment")
    });
    assert!(
        recorded != UNSET_MARKER && !recorded.is_empty(),
        "an unwrapped agent spawned through the in-process registry carries \
         {LIFETIME_TAG_VAR}={recorded:?}, so the reaper armed for its pane has \
         no way to find a descendant that `setsid`s out of the pane's process \
         group — which is exactly the orphan issue #861 measured, on exactly \
         this agent. `wrap` cannot cover this: Claude Code is `NativeHooks` and \
         is spawned bare."
    );
}

/// Scenario: Feed `child_lifetime_bound::clamped` each shape of ambient value a
/// developer's shell can supply — absent, shorter, exactly at the ceiling, over
/// it, zero, and not a number — and assert which ones it lets stand.
///
/// Issue #668: the table that says the ceiling is enforced on ambient values
/// rather than merely defaulted. The end-to-end assertion above can only
/// observe whatever this process happens to have inherited, so it cannot cover
/// the over-ceiling case without an ambient value no test can portably
/// arrange; this can, and it costs microseconds. Lives here rather than in a
/// `#[cfg(test)] mod tests` beside the function because that file is
/// `#[path]`-included into ~88 test crates and would run these cases once per
/// crate.
#[test]
fn ambient_lifetime_caps_are_clamped_to_the_reapers_ceiling() {
    use common::child_lifetime_bound::clamped;

    let ceiling = MAX_LIFETIME_CEILING_SECS.to_string();
    // Absent: pin the default.
    assert_eq!(clamped(None).as_deref(), Some(ceiling.as_str()));
    // Shorter wins, and is left byte-identical — `wrap_io.rs` pins 120 and the
    // fd-table probe pins 10 precisely so nothing they mint outlives the case.
    assert_eq!(clamped(Some("120")), None, "a shorter ambient cap must win");
    assert_eq!(clamped(Some("1")), None, "the shortest legal cap must win");
    assert_eq!(
        clamped(Some(" 30 ")),
        None,
        "surrounding space must not matter"
    );
    // Exactly at the ceiling is in range, so it stands.
    assert_eq!(clamped(Some("300")), None, "the ceiling itself is in range");
    // Over it does not — this is the measured 3600 that made the reaper unsafe.
    assert_eq!(clamped(Some("3600")).as_deref(), Some(ceiling.as_str()));
    assert_eq!(clamped(Some("301")).as_deref(), Some(ceiling.as_str()));
    assert_eq!(
        clamped(Some(&u64::MAX.to_string())).as_deref(),
        Some(ceiling.as_str()),
        "a value that would overflow the consumers' deadline arithmetic must \
         not reach them"
    );
    // Zero and garbage both parse to "no cap" at every consumer
    // (`daemon::parse_max_lifetime_secs` returns `None`), which is the unbounded
    // case this mechanism exists to remove — so they are overwritten, not kept.
    assert_eq!(clamped(Some("0")).as_deref(), Some(ceiling.as_str()));
    assert_eq!(clamped(Some("")).as_deref(), Some(ceiling.as_str()));
    assert_eq!(clamped(Some("later")).as_deref(), Some(ceiling.as_str()));
    assert_eq!(clamped(Some("-1")).as_deref(), Some(ceiling.as_str()));
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
        // SAFETY: best-effort cleanup of a pid this test created. Same
        // check-then-act residual as the sites above: between
        // `process_running` and the signal the pid could be reaped and
        // reissued, so this is bounded same-UID exposure, not an impossibility.
        // A strict guarantee needs an OS-owned container, not a numeric pid.
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
    //
    // 5 s is the FLOOR, not a round number: it has to exceed the armed half's
    // own deadline below (1 s cap + one 250 ms backstop poll + the 1.5 s
    // `WRAP_TERMINATE_GRACE` ≈ 2.75 s), or a child that merely happened to die
    // on schedule would look like one nothing could end, and the assertion
    // underneath would pass for a reason other than the cap. Anything shorter
    // weakens the precondition; anything longer just buys idle seconds on every
    // fast-tier run, on all three CI platforms.
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
         can reach, with the lifetime cap armed — this is the orphan that runs \
         unkillable for days, holding a working directory that has already been \
         deleted (issues #657, #668). It does NOT hold its e2e temp root \
         against `clean-e2e-tmp`: that tool keys on the TEST process's pid in \
         the root's name, not on this child."
    );
}

/// What the wrapped child does after launching its escapee: keep running, or
/// exit at once and leave the escapee at `ppid = 1`.
///
/// Both are real and they take different paths through the reaper. `Persist`
/// leaves the child's process group alive at the deadline, so the reaper reaches
/// its `killpg` — and the escapee still has to be found some other way.
/// `ExitAtOnce` is the shape the #861 orphan was actually in: the group is gone
/// long before the deadline, which is precisely when a reaper that keys only on
/// that group has nothing left to watch and stops watching.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum OuterFate {
    Persist,
    ExitAtOnce,
}

/// Drive a probe whose wrapped child `setsid`s a grandchild into its **own
/// session**, then SIGKILL the wrapper and report whether that *escapee* was
/// still alive at the end of `budget`.
///
/// The shape is the one measured on the orphan in issue #861, and it is not
/// hypothetical: Claude Code's Bash tool detaches every shell it runs into a
/// fresh session — measured live on this box, `pgid == sid == its own pid`
/// against the `claude` process's own session — which `src/agent_pty.rs`
/// already names at its close path as "the `setsid`'d sub-shells Claude Code
/// creates internally". `setsid(1)` stands in for that here so the probe needs
/// no real agent and no credential.
///
/// `cap` is passed to the wrapper's environment verbatim; `None` removes the
/// variable, which is the control showing the cap is what does the work.
///
/// `None` is returned when the host cannot build the shape at all (no
/// `setsid(1)` on `PATH`), so the caller reports a skip rather than asserting on
/// a probe it never ran.
#[cfg(target_os = "linux")]
fn setsid_escapee_survives(cap: Option<&str>, fate: OuterFate, budget: Duration) -> Option<bool> {
    if !Command::new("setsid")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        return None;
    }

    let fixture = common::harness_tempdir().expect("create setsid-escapee fixture");
    let escapee_pid_path = fixture.path().join("escapee.pid");

    // `setsid` is `exec`'d rather than forked here — the wrapped shell is a
    // process-group leader but the `setsid` it runs is not, so `setsid(1)` has
    // no leader conflict to fork around and replaces its own image. That makes
    // the BACKGROUNDING load-bearing: run in the foreground the wrapped shell
    // would simply `wait` on it and never reach the rest of the script.
    // Descriptors go to `/dev/null` so the escapee holds no copy of the inner
    // PTY slave, and therefore cannot suppress the very hangup this file's
    // other probe relies on.
    let escapee = "setsid /bin/sh -c 'trap \"\" TERM; trap \"\" HUP; \
         printf \"%s\\n\" \"$$\" > \"$ESCAPEE_PID_FILE\"; \
         while :; do sleep 1; done' </dev/null >/dev/null 2>&1 &";
    let script = match fate {
        // Ignored dispositions survive `exec`, and this shell reads nothing, so
        // no hangup can reach it — only a signal can end it, which keeps the
        // child's group alive right up to the deadline.
        OuterFate::Persist => {
            format!("{escapee} trap '' TERM; trap '' HUP; while :; do sleep 1; done")
        }
        OuterFate::ExitAtOnce => format!("{escapee} exit 0"),
    };

    let mut command = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
    command
        .args(["wrap", "--agent", "codex", "--", "/bin/sh", "-c", &script])
        .env("ESCAPEE_PID_FILE", &escapee_pid_path)
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

    let read_pid = |path: &std::path::Path| -> Option<libc::pid_t> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|contents| contents.trim().parse().ok())
    };

    let mut wrapper = command.spawn().expect("spawn setsid-escapee probe");
    let recorded = common::wait_until(Duration::from_secs(10), || {
        read_pid(&escapee_pid_path).is_some()
    });
    if !recorded {
        let _ = wrapper.kill();
        let _ = wrapper.wait();
        panic!("the probe never recorded its escapee's pid");
    }
    let escapee_pid = read_pid(&escapee_pid_path).expect("escapee pid recorded");

    // A precondition on the SHAPE, so a probe that quietly failed to escape
    // cannot make the assertion below pass for the wrong reason: an escapee
    // still inside the wrapped child's process group is reached by the
    // pre-existing `killpg` and proves nothing about #861.
    assert_eq!(
        session_id_of(escapee_pid),
        Some(escapee_pid),
        "the escapee must lead a session of its own, or it never left the \
         wrapped child's process group and this probe is not the #861 shape"
    );

    // SIGKILL, not SIGTERM: the wrapper gets no chance to reap, so the only
    // thing that can still end the escapee is a bound that outlives it. Safe on
    // an `ExitAtOnce` run too — an unreaped child of this process is a zombie,
    // which `kill` still accepts.
    let wrapper_pid = wrapper.id() as libc::pid_t;
    // SAFETY: the wrapper pid came from this test's live `Child`; ending it
    // uncleanly is the behavior under test.
    assert_eq!(
        unsafe { libc::kill(wrapper_pid, libc::SIGKILL) },
        0,
        "deliver SIGKILL to wrapper pid {wrapper_pid}"
    );
    let _ = wrapper.wait();

    let gone = common::wait_until(budget, || !common::process_running(escapee_pid));

    // Never leak this test's own probe, whatever the outcome above — otherwise a
    // regression in the code under test would itself mint the orphan #861 is
    // about.
    //
    // Deliberately the single pid and NOT `kill(-pid, …)`, unlike the
    // group-resident probe above. A review finding on this PR pointed out that
    // the escapee is tracked by a bare number, so a check-then-act cleanup can
    // in principle signal a replacement that inherited the pid — and a *group*
    // send multiplies that from one stranger to a whole group of them. The
    // narrow form costs nothing here: this escapee's only children are the
    // transient `sleep 1`s of its own loop, which end within a second of it
    // whether they are signalled or not.
    //
    // The residual on the single send is the same bounded same-UID exposure
    // every pid-addressed site in this repository carries, and it is bounded
    // rather than closed for the reason `wrap`'s reaper gives at length: a
    // number carries no identity, and a strict guarantee needs an OS-owned
    // container rather than a pid.
    if common::process_running(escapee_pid) {
        // SAFETY: best-effort cleanup of a pid this test created.
        unsafe {
            libc::kill(escapee_pid, libc::SIGKILL);
        }
    }
    Some(!gone)
}

/// `getsid(2)` for another pid, or `None` when the pid is already gone.
#[cfg(target_os = "linux")]
fn session_id_of(pid: libc::pid_t) -> Option<libc::pid_t> {
    // SAFETY: `getsid` takes a pid and returns a pid or -1; it reads no memory
    // of ours.
    let sid = unsafe { libc::getsid(pid) };
    (sid > 0).then_some(sid)
}

/// Scenario: Run a wrapped child that `setsid`s a grandchild into its own
/// session — the shape Claude Code's Bash tool really has — then SIGKILL the
/// wrapper so no reap loop runs. With no lifetime cap in the environment the
/// escapee must still be alive after the budget; with a one-second cap it must
/// be gone inside it, whether the wrapped child kept running or exited the
/// moment it had launched the escapee.
///
/// Issue #861. The child-group reaper #661 forks holds
/// `killpg(child_pid, …)`, and a `killpg` cannot reach a process that has left
/// the group — so before this test the escapee outlived its cap without limit.
/// Measured in the wild: pid 2043710 carrying
/// `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS=300` in its own environment,
/// `PPID 1`, alive **4 days 01:23** — about 1170x its cap — with its owning
/// test process dead and its temp root already deleted from under it.
#[cfg(target_os = "linux")]
#[test]
fn a_setsid_escapee_is_still_bounded_by_the_cap() {
    // Control: nothing armed, so nothing can end it. 5 s for the same reason
    // the group-resident control uses it — it has to exceed the armed halves'
    // own deadline below (1 s cap + one 250 ms poll + the 1.5 s
    // `WRAP_TERMINATE_GRACE` ≈ 2.75 s), or an escapee that merely happened to
    // die on schedule would look like one nothing could end.
    let Some(survived) =
        setsid_escapee_survives(None, OuterFate::ExitAtOnce, Duration::from_secs(5))
    else {
        eprintln!(
            "SKIP: the #861 escapee probe needs `setsid(1)` on PATH to put a \
             grandchild in its own session"
        );
        return;
    };
    assert!(
        survived,
        "precondition failed: a TERM/HUP-ignoring `setsid` escapee that reads \
         nothing died with no lifetime cap armed, so the assertions below would \
         pass for a reason other than the cap"
    );
    // Armed: 1 s cap + one 250 ms backstop poll + WRAP_TERMINATE_GRACE (1.5 s).
    // 30 s is loose headroom for a loaded host, not an expected duration.
    //
    // Both fates are run before anything is asserted, deliberately: they fail
    // for *different* reasons — `ExitAtOnce` needs the reaper to stop treating
    // group death as "nothing left to bound", and `Persist` needs the sweep at
    // the deadline itself — so a loop that panicked on the first one would leave
    // the second's assertion never observed failing, which is no evidence at
    // all.
    let outcomes = [
        ("wrapped child exited at once", OuterFate::ExitAtOnce),
        ("wrapped child kept running", OuterFate::Persist),
    ]
    .map(|(label, fate)| {
        (
            label,
            setsid_escapee_survives(Some("1"), fate, Duration::from_secs(30)),
        )
    });
    let stranded: Vec<&str> = outcomes
        .iter()
        .filter(|(_, outcome)| *outcome != Some(false))
        .map(|(label, _)| *label)
        .collect();
    assert!(
        stranded.is_empty(),
        "a SIGKILL'd wrapper stranded a descendant that had `setsid`'d out of \
         the wrapped child's process group, with the lifetime cap armed, in \
         these cases: {stranded:?}. `killpg` cannot reach it and every ancestor \
         that could have is dead, so nothing enforces the cap it still carries \
         in its own environment — the orphan that ran for four days in issue \
         #861."
    );
}
