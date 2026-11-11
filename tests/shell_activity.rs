//! PRD #386 M1/M2 — the process-table primitive and the structural
//! discriminator the descendant-scan shell-activity signal is built on:
//! `process_table()` enumerates every process on the machine (Unix; `None`
//! on Windows) including each process's POSIX session id (`ProcessInfo`),
//! `descendants()` walks that table from a root pid cycle-safely, and
//! `descendant_shell_activity()` classifies a table as busy/idle by
//! `getsid(descendant) != getsid(root_pid)` — the structural test that
//! replaced this PRD's original argv-match condition 3 (see the PRD's Work
//! Log, 2026-08-06).
//!
//! Per the PRD's Test Plan, M1 proves nothing about the feature working in a
//! real pane — that is the explicit lesson of PRD #370, whose one
//! end-to-end test was a correct mechanism test wired to nothing real. M2
//! proves the classification is correct against the measured process shapes,
//! including the CI trap where the agent itself has no controlling
//! terminal. M3/M4/M6 (later milestones) carry the burden of proving the
//! signal actually fires in a real pane.
//!
//! RED until M1/M2 land: `dot_agent_deck::platform::proc::{ProcessInfo,
//! process_table, descendants, descendant_shell_activity}` do not exist yet
//! (or, for `ProcessInfo`, exist without a `session_id` field), so this test
//! binary fails to compile. That compile-level RED is the point — coder
//! implements to this file's intended API.

// Issue #668: the wrapped-child lifetime bound for the one test below that
// spawns through a bare `AgentPtyRegistry`, in a file small enough to include
// on its own instead of pulling in the ~420 KB harness this file deliberately
// does not link. `common::init_test_env` calls the same `arm()`. Unix-gated
// like its only caller, so the module is not dead code on Windows.
#[cfg(unix)]
#[path = "common/child_lifetime_bound.rs"]
mod child_lifetime_bound;

#[cfg(unix)]
#[path = "../src/test_temp.rs"]
mod test_temp;

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::io::Read as _;
#[cfg(unix)]
use std::os::unix::io::FromRawFd as _;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
#[cfg(unix)]
use std::process::{Child, Command, Stdio};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use dot_agent_deck::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions};
#[cfg(unix)]
use dot_agent_deck::platform::proc::CLAUDE_BASH_TOOL_SHAPE;
use dot_agent_deck::platform::proc::{
    CommandLine, ProcessInfo, descendant_shell_activity, descendants, process_table,
};

use spec::spec;

// ---------------------------------------------------------------------------
// Real-process half (Unix only — spawns and setsid()'s a genuine grandchild).
// ---------------------------------------------------------------------------

/// Owns the two real processes `spawn_detached_grandchild` creates and kills
/// both on drop, so a panic mid-assertion still cleans up rather than
/// leaking a process into the runner's tree. `target_pid` is learned from
/// `mid`'s own `pre_exec` fork over a pipe — a `fork()` return value is only
/// visible inside the two forked branches, never to this (the original)
/// process.
#[cfg(unix)]
struct SpawnedGrandchild {
    mid: Child,
    target_pid: i32,
}

#[cfg(unix)]
impl Drop for SpawnedGrandchild {
    fn drop(&mut self) {
        // SAFETY: `target_pid` is a real pid this process learned over the
        // pipe; SIGKILL on an already-exited pid just returns ESRCH, which is
        // fine to ignore for cleanup purposes.
        unsafe {
            libc::kill(self.target_pid, libc::SIGKILL);
        }
        let _ = self.mid.kill();
        let _ = self.mid.wait();
    }
}

/// Spawns a real `mid` process directly from the test (so `mid`'s ppid is
/// the test binary's own pid — the "test-owned ancestor"), and has `mid`
/// itself `fork()` a second real process inside its own `pre_exec` — before
/// `mid` execs anything — so the second process (`target`) is a genuine
/// grandchild of the test process with a real `ppid` chain, not a synthetic
/// relationship. `target` immediately `setsid()`s (detaching from any
/// controlling terminal and becoming its own session leader) and then
/// `execv`s into a marker-tagged `/bin/sleep 20` — the same shape a Claude
/// Bash-tool child has in a real pane (on pipes, via setsid, argv carries an
/// identifiable marker). `target`'s real pid is handed back to this process
/// over a pipe written inside `mid`'s `pre_exec`, synchronously, before
/// `mid` itself execs its own long-lived placeholder (`sleep 20`) so the
/// `ppid` chain stays valid for the whole assertion window. 20s is long
/// enough for the assertion but short enough that a test process that dies
/// mid-run does not leave orphans behind indefinitely.
#[cfg(unix)]
fn spawn_detached_grandchild(marker: &str) -> SpawnedGrandchild {
    let program = CString::new("/bin/sleep").expect("no NUL in \"/bin/sleep\"");
    let argv_strings: Vec<CString> = vec![
        CString::new(marker.to_string()).expect("marker must not contain an interior NUL"),
        CString::new("20").expect("static literal has no interior NUL"),
    ];

    let mut pipe_fds = [0i32; 2];
    // SAFETY: `pipe(2)` fills both fds on success. Used purely to hand
    // `target`'s real pid back to this process, since a `fork()` return
    // value is invisible outside the two forked branches.
    let rc = unsafe { libc::pipe(pipe_fds.as_mut_ptr()) };
    assert_eq!(rc, 0, "pipe() failed: {}", std::io::Error::last_os_error());
    let [read_fd, write_fd] = pipe_fds;

    let mut mid = Command::new("/bin/sleep");
    mid.arg("20");
    mid.stdin(Stdio::null());
    mid.stdout(Stdio::null());
    mid.stderr(Stdio::null());

    // SAFETY: runs inside the forked `mid` child, strictly before `mid`
    // execs `sleep 20`. Only async-signal-safe libc calls are used in the
    // `target` (grandchild) branch (`fork`, `setsid`, `close`, `execv`,
    // `_exit`) — the same discipline `child_pre_exec` in `src/wrap.rs` uses
    // for its single-fork case. Building `argv_ptrs` (a `Vec`) in the
    // grandchild branch is a pragmatic, widely-used exception to strict
    // async-signal-safety (the branch is single-threaded at fork time, so
    // there is no other thread that could be holding the allocator lock).
    unsafe {
        mid.pre_exec(move || {
            match libc::fork() {
                -1 => Err(std::io::Error::last_os_error()),
                0 => {
                    // `target` branch: never returns to Rust's own
                    // post-`pre_exec` exec logic — either `execv` replaces
                    // this process image, or `_exit` on failure.
                    libc::close(read_fd);
                    libc::close(write_fd);
                    libc::setsid();
                    let mut argv_ptrs: Vec<*const libc::c_char> =
                        argv_strings.iter().map(|s| s.as_ptr()).collect();
                    argv_ptrs.push(std::ptr::null());
                    libc::execv(program.as_ptr(), argv_ptrs.as_ptr());
                    libc::_exit(127);
                }
                child_pid => {
                    // `mid` branch: hand `target`'s real pid back over the
                    // pipe, then fall through (`Ok(())`) so `mid` execs its
                    // own placeholder and stays alive as `target`'s real
                    // parent for the rest of the test.
                    libc::close(read_fd);
                    let bytes = child_pid.to_ne_bytes();
                    libc::write(write_fd, bytes.as_ptr() as *const libc::c_void, bytes.len());
                    libc::close(write_fd);
                    Ok(())
                }
            }
        });
    }

    let mid_child = mid.spawn().expect("spawn mid (the test-owned ancestor)");
    // SAFETY: this process's own copy of the pipe — the write end is only
    // ever used from inside the forked child above.
    unsafe {
        libc::close(write_fd);
    }
    // SAFETY: `read_fd` was just filled by `pipe(2)` above and is owned
    // exclusively by this process from this point on.
    let mut reader = unsafe { std::fs::File::from_raw_fd(read_fd) };
    let mut buf = [0u8; 4];
    reader
        .read_exact(&mut buf)
        .expect("read target's real pid back from the pipe");
    let target_pid = i32::from_ne_bytes(buf);

    SpawnedGrandchild {
        mid: mid_child,
        target_pid,
    }
}

/// Scenario: the test spawns `mid` directly (a real child of the test
/// process), and `mid` itself forks a second real process (`target`),
/// `setsid()`s it, and execs it as a marker-tagged `/bin/sleep 20` — so
/// `target` is a genuine grandchild of the test process, on pipes, detached
/// from any controlling terminal, and its own session leader: the same
/// process shape a real Claude Bash-tool child has in a real pane. Calls the
/// not-yet-written `process_table()` + `descendants()` primitives and
/// asserts `target` is found as a descendant of the test's own pid, with no
/// controlling terminal, session-leader true, its full argv containing the
/// marker, and — the amended assertion — a `session_id` that differs from
/// the test process's own (read independently via `libc::getsid(0)`), the
/// real-process proof that `getsid` reads what condition 3 of the
/// discriminator assumes it reads.
#[cfg(unix)]
#[spec("status/shell-activity/001")]
#[test]
fn shell_activity_001_finds_a_real_detached_grandchild_as_a_descendant() {
    let marker = format!("shell-activity-001-target-{}", std::process::id());
    let spawned = spawn_detached_grandchild(&marker);
    let root_pid = std::process::id() as i32;

    // Poll: `mid`'s pre_exec fork/setsid/exec races this thread, so give the
    // OS a moment to make `target` observable in the process table.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut found: Option<ProcessInfo> = None;
    while found.is_none() && Instant::now() < deadline {
        // Issue #862: the sample reads a command line only for the detached
        // descendants of the roots it is given, so this test — which asserts on
        // the target's argv — must name its own pid as the root. That IS the
        // property under test here: pass no root and the argv comes back
        // `NotSampled`, which the second half of this test pins.
        let table = process_table(&[root_pid]).expect("process_table() must enumerate on unix");
        found = descendants(&table, root_pid)
            .into_iter()
            .find(|p| p.pid == spawned.target_pid)
            .cloned();
        if found.is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    let target = found.unwrap_or_else(|| {
        panic!(
            "target pid {} never appeared as a descendant of test pid {root_pid} within 2s",
            spawned.target_pid
        )
    });

    assert!(
        !target.has_controlling_tty,
        "a setsid()'d grandchild must report no controlling terminal: {target:?}"
    );
    assert!(
        target.session_leader,
        "a process that just called setsid() is its own session leader: {target:?}"
    );
    let target_argv = target
        .command_line
        .read()
        .unwrap_or_else(|| {
            panic!(
                "a detached descendant of the sample's root must have its command line read \
                 (issue #862's two-phase sample): {target:?}"
            )
        })
        .to_string();
    assert!(
        target_argv.contains(&marker),
        "descendant argv must be the full command line — marker {marker:?} not found in \
         {target_argv:?}"
    );

    // SAFETY: `getsid(2)` with pid 0 means "the caller's own session id"; it
    // is async-signal-safe and has no side effects. This is an independent
    // oracle (not routed through `process_table()`) proving `target.session_id`
    // — the field the discriminator's condition 3 is built on — actually
    // reflects what `getsid` reports, rather than merely differing from some
    // other field by coincidence.
    let own_sid = unsafe { libc::getsid(0) };
    assert_ne!(
        target.session_id, own_sid,
        "a setsid()'d grandchild must be its own session leader: its session id must differ \
         from the test process's own (target={target:?}, own_sid={own_sid})"
    );
}

/// Scenario: spawns the same real detached grandchild as
/// `status/shell-activity/001` (a `setsid`'d, marker-tagged `/bin/sleep`, on
/// pipes, off any terminal — the shape a Claude Bash-tool child has), then
/// samples the process table twice: once naming the test's own pid as a root and
/// once naming no root at all. With the root, the detached grandchild's command
/// line must be `Read` and carry the marker; the test's own non-detached child
/// (`mid`, in the test's own session) and every unrelated process on the machine
/// must be `NotSampled`. With no root, even the grandchild must be `NotSampled`.
/// That is the whole of issue #862's fix expressed as a property: a poll reads
/// the command line of the processes at its own panes' session boundaries and of
/// nothing else on the machine.
#[cfg(unix)]
#[spec("status/shell-activity/008")]
#[test]
fn shell_activity_008_the_sample_reads_command_lines_only_at_session_boundaries() {
    let marker = format!("shell-activity-008-target-{}", std::process::id());
    let spawned = spawn_detached_grandchild(&marker);
    let root_pid = std::process::id() as i32;
    let mid_pid = spawned.mid.id() as i32;

    // `mid`'s `pre_exec` fork/setsid/exec races this thread, exactly as in
    // `status/shell-activity/001` — poll until the grandchild is observable.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut table = Vec::new();
    while Instant::now() < deadline {
        table = process_table(&[root_pid]).expect("process_table() must enumerate on unix");
        if table.iter().any(|p| p.pid == spawned.target_pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let row = |table: &[ProcessInfo], pid: i32| {
        table
            .iter()
            .find(|p| p.pid == pid)
            .cloned()
            .unwrap_or_else(|| panic!("pid {pid} must appear in the process-table sample"))
    };

    // 1. The detached grandchild: its command line IS read, because it is a
    //    detached descendant of the root the sample was taken for.
    let target = row(&table, spawned.target_pid);
    let target_argv = target.command_line.read().unwrap_or_else(|| {
        panic!("the detached grandchild's command line must be read: {target:?}")
    });
    assert!(
        target_argv.contains(&marker),
        "and it must be the full command line — marker {marker:?} not in {target_argv:?}"
    );

    // 2. `mid`, a real child of this test in this test's OWN session, is not a
    //    detached descendant, so nothing reads its command line — the scoping is
    //    by session, not merely by descendancy.
    assert_eq!(
        row(&table, mid_pid).command_line,
        CommandLine::NotSampled,
        "a descendant that shares the root's POSIX session can never be a cross-check \
         candidate, so its command line must not be read"
    );

    // 2b. And the scoping stops AT the session boundary rather than descending
    //     past it: the grandchild is the only candidate, so a process below it
    //     would inherit its session and still not be read. `shell_tool_candidates`
    //     is asserted directly because that narrowing is what keeps a Bash-tool
    //     call's whole build tree out of the argv phase (issue #862); the
    //     synthetic-tree proof is `shell_tool_candidates_stop_at_the_session_boundary`
    //     in `src/platform/proc/scan.rs`.
    assert_eq!(
        dot_agent_deck::platform::proc::shell_tool_candidates(&table, root_pid),
        Some(vec![spawned.target_pid]),
        "the detached grandchild must be the one and only session-boundary candidate \
         under this test's own pid"
    );

    // 3. And nothing outside this test's tree is read at all. This is the claim
    //    the old `ps -A … args=` sample could not make: it read
    //    `/proc/<pid>/cmdline` AND `/proc/<pid>/environ` for every process on
    //    the machine, each of which takes that process's `mmap_lock`.
    let unrelated: Vec<i32> = table
        .iter()
        .filter(|p| p.command_line != CommandLine::NotSampled)
        .map(|p| p.pid)
        .filter(|pid| *pid != spawned.target_pid)
        .collect();
    assert!(
        unrelated.is_empty(),
        "only the detached descendants of the sample's roots may have a command line \
         read; these other pids did too: {unrelated:?}"
    );

    // 4. With no roots, the argv phase does not run at all — so even the
    //    grandchild comes back `NotSampled`. This is what makes claim 3 a
    //    property of the roots rather than an accident of this process tree.
    let rootless = process_table(&[]).expect("process_table() must enumerate on unix");
    assert_eq!(
        row(&rootless, spawned.target_pid).command_line,
        CommandLine::NotSampled,
        "a sample taken for no root must read no command line whatsoever"
    );
    assert!(
        rootless
            .iter()
            .all(|p| p.command_line == CommandLine::NotSampled),
        "and that must hold for every row, not just the grandchild's"
    );
}

// ---------------------------------------------------------------------------
// Cycle-safety half (any platform — pure data, no real processes, no `ps`).
// ---------------------------------------------------------------------------

/// Scenario: constructs a synthetic table by hand where pid 2 is a normal
/// child of root pid 1, pid 3 is a normal child of pid 2, but the table ALSO
/// claims pid 1's `ppid` is 2 — a `ppid` cycle looping straight back into
/// the branch the walk just descended, exactly the shape the PRD says a
/// non-atomically sampled `ps` table can produce after PID reuse. Calls
/// `descendants()` on a background thread with a bounded join timeout and
/// asserts it returns (rather than looping forever) with exactly `{2, 3}` —
/// each reachable descendant reported once, and the root itself never
/// re-reported despite the cycle linking back to it.
#[spec("status/shell-activity/002")]
#[test]
fn shell_activity_002_descendant_walk_terminates_on_a_ppid_cycle() {
    // `session_id` is irrelevant to cycle-termination — every row shares one
    // arbitrary value so the walk's own logic (ppid-following, cycle
    // detection) is what's under test, not the discriminator.
    const IRRELEVANT_SID: i32 = 1000;
    let table = vec![
        ProcessInfo {
            pid: 2,
            ppid: 1,
            session_id: IRRELEVANT_SID,
            has_controlling_tty: false,
            session_leader: false,
            command_line: CommandLine::Read("cycle-entry".to_string()),
        },
        ProcessInfo {
            pid: 1,
            ppid: 2,
            session_id: IRRELEVANT_SID,
            has_controlling_tty: false,
            session_leader: false,
            command_line: CommandLine::Read("cycle-back-to-root".to_string()),
        },
        ProcessInfo {
            pid: 3,
            ppid: 2,
            session_id: IRRELEVANT_SID,
            has_controlling_tty: false,
            session_leader: false,
            command_line: CommandLine::Read("past-the-cycle".to_string()),
        },
    ];

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let pids: Vec<i32> = descendants(&table, 1).into_iter().map(|p| p.pid).collect();
        let _ = tx.send(pids);
    });

    // A hang detector, not a performance bound: a walk that terminates returns
    // in microseconds, so the ceiling is paid only by a looping regression. It
    // was 2 s, which a starved box overran without any loop (`io full` 82% in
    // PRD #1631's broad `cargo test-e2e` run); this file does not link the
    // harness's `load_scaled`, so the ceiling is a flat 30 s instead.
    let mut pids = rx.recv_timeout(std::time::Duration::from_secs(30)).expect(
        "descendants() must terminate on a synthetic ppid cycle instead of looping forever",
    );
    pids.sort_unstable();
    assert_eq!(
        pids,
        vec![2, 3],
        "the walk must report each reachable descendant exactly once and must not \
         re-report the root (pid 1) even though the cycle links back to it"
    );
}

// ---------------------------------------------------------------------------
// M2 — the structural discriminator (any platform — pure data, no real
// processes, no `ps`). Fixture rows are the measured tables from
// `.dot-agent-deck/386-argv-notes.md` §4a / the PRD's own reproduction of it:
// a live agent pane's `getsid(2)` table and its `ps -Ao pid,ppid,pgid,tty,stat,args`
// table, both captured 2026-08-06 against Claude Code 2.1.220. The two
// captures were taken at different instants and so carry different pids for
// the Bash-tool subtree (61296 in the `ps` capture, 63120 in the `getsid`
// capture); this fixture uses the `getsid` capture's pid (63120) as the row
// identity and attributes to it the no-controlling-tty / session-leader
// shape the PRD states is true of "every Bash-tool child measured" (not
// specific to either single capture).
//
// Session id caveat, reported rather than invented: the `getsid` table
// names an explicit session id for only three of the five confounders
// (context7, engram, caffeinate — all `sid=51698`, the agent's own). It does
// not list `task-master` or `pysemgrep`. Their session id here (also
// `51698`) is derived, not separately `getsid`-measured: the `ps` table
// shows `pgid=51698` for both, on the same `ttys014` as the three confirmed
// rows, and none of the five ever appears with a `pgid` that isn't also a
// confirmed session id elsewhere in the same tables — so `pgid` and `sid`
// coincide throughout this tree (true for the pane leader and for the three
// confirmed confounders alike). Flagged here, and in the work-done report,
// rather than silently presented as a direct `getsid` reading.
// ---------------------------------------------------------------------------

const AGENT_PID: i32 = 51757;
const AGENT_SID: i32 = 51698;
const BASH_TOOL_SID: i32 = 63118;

/// The agent's own row plus its five measured long-lived children
/// (`context7`, `task-master`, `engram`, `pysemgrep`, `caffeinate`), all in
/// the agent's own session on the pane's tty. `flip_ctty_to_none` simulates
/// the CI trap: every row (the agent included) reporting no controlling
/// terminal, exactly the container shape `.dot-agent-deck/386-argv-notes.md`
/// §5 measured (`tty_nr = 0` for PID 1 under `docker run` without `-t`).
fn backbone_rows(flip_ctty_to_none: bool) -> Vec<ProcessInfo> {
    let has_ctty = !flip_ctty_to_none;
    let mut rows = vec![ProcessInfo {
        pid: AGENT_PID,
        // Parent is the pane leader (devbox), not itself a descendant of
        // AGENT_PID and so never reachable by the walk — included only so
        // `descendant_shell_activity` can look up the agent's own session id
        // from the same table it classifies against.
        ppid: 51698,
        session_id: AGENT_SID,
        has_controlling_tty: has_ctty,
        session_leader: false,
        command_line: CommandLine::Read("claude --model opus".to_string()),
    }];
    for (pid, argv) in [
        (51787, "npm exec @upstash/context7-mcp"),
        (51788, "npm exec task-master-ai"),
        (51789, "engram mcp --tools=agent"),
        (51807, "pysemgrep mcp"),
        (60798, "caffeinate -i -t 300"),
    ] {
        rows.push(ProcessInfo {
            pid,
            ppid: AGENT_PID,
            session_id: AGENT_SID,
            has_controlling_tty: has_ctty,
            session_leader: false,
            command_line: CommandLine::Read(argv.to_string()),
        });
    }
    rows
}

/// The measured Bash-tool descendant: `setsid`-detached into its own
/// session (`BASH_TOOL_SID`, differing from `AGENT_SID`), no controlling
/// terminal, its own session leader — already true in the CI-trap case, so
/// unlike [`backbone_rows`] this has no `flip_ctty_to_none` parameter.
fn bash_tool_descendant_row() -> ProcessInfo {
    ProcessInfo {
        pid: 63120,
        ppid: AGENT_PID,
        session_id: BASH_TOOL_SID,
        has_controlling_tty: false,
        session_leader: true,
        command_line: CommandLine::Read(
            "/bin/zsh -c source …/shell-snapshots/snapshot-zsh-… && eval …".to_string(),
        ),
    }
}

/// Scenario: Classify fixture process tables with one command session containing many children, then two command sessions and an unmatched detached service. Distinct qualifying sessions are counted once each, nested terminal agents re-root the walk, and unknown root/session samples stay unknown rather than clearing work.
#[spec("status/background/005")]
#[test]
fn background_005_scan_deduplicates_qualifying_sessions_and_reroots() {
    use dot_agent_deck::platform::proc::qualifying_command_sessions;
    use std::collections::HashSet;

    let shape = dot_agent_deck::platform::proc::CLAUDE_BASH_TOOL_SHAPE;
    let mut table = backbone_rows(false);
    table.push(bash_tool_descendant_row());
    for pid in [63121, 63122] {
        table.push(ProcessInfo {
            pid,
            ppid: 63120,
            session_id: BASH_TOOL_SID,
            has_controlling_tty: false,
            session_leader: false,
            command_line: CommandLine::NotSampled,
        });
    }
    assert_eq!(
        qualifying_command_sessions(&table, AGENT_PID, &[shape]),
        Some(HashSet::from([BASH_TOOL_SID])),
        "one session with many processes counts once"
    );
    let mut second = bash_tool_descendant_row();
    second.pid = 64120;
    second.session_id = 64118;
    table.push(second);
    assert_eq!(
        qualifying_command_sessions(&table, AGENT_PID, &[shape]),
        Some(HashSet::from([BASH_TOOL_SID, 64118])),
        "the argv cross-check must visit every qualifying candidate"
    );
    table.push(ProcessInfo {
        pid: 65120,
        ppid: AGENT_PID,
        session_id: 65118,
        has_controlling_tty: false,
        session_leader: true,
        command_line: CommandLine::Read("unmatched-detached-service".into()),
    });
    assert_eq!(
        qualifying_command_sessions(&table, AGENT_PID, &[shape]),
        Some(HashSet::from([BASH_TOOL_SID, 64118])),
        "a match must not admit another session whose argv fails"
    );
    // Removing one matched session leaves exactly one despite an unmatched
    // candidate beside it; structural-only agents count all three sessions.
    table.retain(|row| row.session_id != 64118);
    assert_eq!(
        qualifying_command_sessions(&table, AGENT_PID, &[shape]),
        Some(HashSet::from([BASH_TOOL_SID]))
    );
    assert_eq!(
        qualifying_command_sessions(&table, AGENT_PID, &[]),
        Some(HashSet::from([BASH_TOOL_SID, 65118]))
    );
    let nested = vec![
        ProcessInfo {
            pid: 1,
            ppid: 0,
            session_id: 1,
            has_controlling_tty: true,
            session_leader: true,
            command_line: CommandLine::NotSampled,
        },
        ProcessInfo {
            pid: 2,
            ppid: 1,
            session_id: 2,
            has_controlling_tty: true,
            session_leader: true,
            command_line: CommandLine::NotSampled,
        },
        ProcessInfo {
            pid: 3,
            ppid: 2,
            session_id: 2,
            has_controlling_tty: true,
            session_leader: false,
            command_line: CommandLine::NotSampled,
        },
        ProcessInfo {
            pid: 4,
            ppid: 3,
            session_id: 4,
            has_controlling_tty: false,
            session_leader: true,
            command_line: CommandLine::NotSampled,
        },
    ];
    assert_eq!(
        qualifying_command_sessions(&nested[..3], 1, &[]),
        Some(HashSet::new()),
        "inner PTY agent is not itself a command"
    );
    assert_eq!(
        qualifying_command_sessions(&nested, 1, &[]),
        Some(HashSet::from([4])),
        "inner agent's detached tool is a command"
    );
    assert_eq!(qualifying_command_sessions(&[], AGENT_PID, &[]), None);
    assert_eq!(qualifying_command_sessions(&table, -1, &[]), None);
    table
        .iter_mut()
        .find(|row| row.pid == AGENT_PID)
        .unwrap()
        .session_id = 0;
    assert_eq!(
        qualifying_command_sessions(&table, AGENT_PID, &[]),
        None,
        "unknown observation must not become zero"
    );
}

/// Scenario: Classify a Claude pane's process table that holds the deck's own inbox waiter — `sh -c '<deck>' inbox wait --agent claude-code` and the deck process under it — in a session of its own, and an OpenCode-style watcher left in the agent's session. Neither is a background command, so neither can make a card read Waiting; a real Bash-tool command beside the waiter still counts.
#[spec("status/waiting/006")]
#[test]
fn status_waiting_006_inbox_waiter_is_not_a_background_command() {
    use dot_agent_deck::platform::proc::qualifying_command_sessions;
    use std::collections::HashSet;

    const WAITER_SID: i32 = 70118;
    let shape = dot_agent_deck::platform::proc::CLAUDE_BASH_TOOL_SHAPE;
    let waiter = |sid: i32| {
        vec![
            // Claude Code runs a hook as `sh -c <command>`.
            ProcessInfo {
                pid: 70120,
                ppid: AGENT_PID,
                session_id: sid,
                has_controlling_tty: false,
                session_leader: sid != AGENT_SID,
                command_line: CommandLine::Read(
                    "/bin/sh -c '/home/user/.local/bin/dot-agent-deck' inbox wait --agent claude-code"
                        .to_string(),
                ),
            },
            ProcessInfo {
                pid: 70121,
                ppid: 70120,
                session_id: sid,
                has_controlling_tty: false,
                session_leader: false,
                command_line: CommandLine::Read(
                    "/home/user/.local/bin/dot-agent-deck inbox wait --agent claude-code"
                        .to_string(),
                ),
            },
        ]
    };

    // A Claude pane, the waiter in a session of its own: the argv cross-check
    // refuses it.
    let mut table = backbone_rows(false);
    table.extend(waiter(WAITER_SID));
    assert_eq!(
        qualifying_command_sessions(&table, AGENT_PID, &[shape]),
        Some(HashSet::new()),
        "the deck's own waiter is not a background command"
    );
    // Control: a real Bash-tool command beside it still counts, alone.
    table.push(bash_tool_descendant_row());
    assert_eq!(
        qualifying_command_sessions(&table, AGENT_PID, &[shape]),
        Some(HashSet::from([BASH_TOOL_SID])),
        "the waiter must not hide or join a real command"
    );

    // An agent with no measured shape (OpenCode, Pi): its watcher stays in the
    // agent's own session, so the structural test alone refuses it.
    let mut table = backbone_rows(false);
    table.extend(waiter(AGENT_SID));
    assert_eq!(
        qualifying_command_sessions(&table, AGENT_PID, &[]),
        Some(HashSet::new()),
        "a watcher in the agent's own session is not a background command"
    );
}

/// Scenario: Classify an OpenCode 2 pane's process table, shaped as measured on OpenCode 2.2.5 (the client on the pane, its private server `<path>/opencode serve --stdio --port 0` as its direct child in a POSIX session of its own, the deck's inbox watcher in the server's session), given the client/server pair the daemon's OpenCode watch identified. That server and its session are not a background command; every other process counts as before: a command below the server, a `serve --stdio` a tool starts below or beside it, one a wrapper shell starts, a server whose command line was not read or no longer reads as a server, and anything on a pane with no identified server (OpenCode 1.x, an unknown version), even when its command text quotes `opencode serve --stdio`.
#[spec("status/waiting/016")]
#[test]
fn status_waiting_016_opencode_own_server_is_not_a_background_command() {
    use dot_agent_deck::platform::proc::{
        AgentServer, is_opencode_private_server_command, qualifying_command_sessions,
        qualifying_command_sessions_with_server,
    };
    use std::collections::HashSet;

    const WRAPPER: i32 = 80090;
    const CLIENT: i32 = 80100;
    const SERVER: i32 = 80110;
    const COMMAND_SID: i32 = 80120;
    const TOOL_SERVER_SID: i32 = 80130;
    const SIBLING_SID: i32 = 80150;
    const FORGED_SID: i32 = 80160;
    const SERVER_ARGV: &str = "/home/user/.opencode/bin/opencode serve --stdio --port 0";
    let identified = Some(AgentServer {
        client_pid: CLIENT,
        server_pid: SERVER,
    });
    let row = |pid: i32, ppid: i32, sid: i32, argv: Option<&str>| ProcessInfo {
        pid,
        ppid,
        session_id: sid,
        has_controlling_tty: sid == CLIENT,
        session_leader: pid == sid,
        command_line: argv.map_or(CommandLine::NotSampled, |argv| {
            CommandLine::Read(argv.to_string())
        }),
    };
    // What the sampler reads: a command line for each session-boundary row.
    let standalone = || {
        vec![
            row(CLIENT, 1, CLIENT, None),
            row(SERVER, CLIENT, SERVER, Some(SERVER_ARGV)),
            // The server plugin's inbox watcher and another server child, in
            // the server's own session.
            row(80111, SERVER, SERVER, None),
            row(80112, SERVER, SERVER, None),
        ]
    };
    let count = |table: &[ProcessInfo], root: i32, server: Option<AgentServer>| {
        qualifying_command_sessions_with_server(table, root, &[], server)
    };

    let table = standalone();
    assert_eq!(
        qualifying_command_sessions(&table, CLIENT, &[]),
        Some(HashSet::from([SERVER])),
        "premise: without the identified server the structural test counts it"
    );
    assert_eq!(count(&table, CLIENT, None), Some(HashSet::from([SERVER])));
    assert_eq!(
        count(&table, CLIENT, identified),
        Some(HashSet::new()),
        "OpenCode's own server, and its session, are not a background command"
    );

    // A real backgrounded command: the shell tool's child of the server, in a
    // session of its own, with its own child.
    let mut table = standalone();
    table.push(row(
        80120,
        SERVER,
        COMMAND_SID,
        Some("/bin/bash -c ping -c 60 127.0.0.1 && touch sentinel"),
    ));
    table.push(row(80121, 80120, COMMAND_SID, None));
    assert_eq!(
        count(&table, CLIENT, identified),
        Some(HashSet::from([COMMAND_SID])),
        "the command below the server counts, once"
    );

    // A server a tool started below the agent's own is a command like any
    // other: only the identified pid is the agent's.
    table.push(row(80130, SERVER, TOOL_SERVER_SID, Some(SERVER_ARGV)));
    assert_eq!(
        count(&table, CLIENT, identified),
        Some(HashSet::from([COMMAND_SID, TOOL_SERVER_SID])),
        "a tool's own `serve --stdio` is not the agent's server"
    );

    // A sibling of the real server — a second `serve --stdio` child of the
    // client, in a session of its own — counts.
    let mut table = standalone();
    table.push(row(80150, CLIENT, SIBLING_SID, Some(SERVER_ARGV)));
    assert_eq!(
        count(&table, CLIENT, identified),
        Some(HashSet::from([SIBLING_SID])),
        "a sibling of the identified server is a command"
    );

    // A wrapper shell runs the client; a process that forged a server's argv,
    // whose parent is that shell, counts — and it would even if the identity
    // named its pid, since its parent is not the client.
    let wrapped = || {
        let mut table = vec![row(WRAPPER, 1, CLIENT, None)];
        table.extend(standalone().into_iter().map(|mut r| {
            if r.pid == CLIENT {
                r.ppid = WRAPPER;
            }
            r
        }));
        table.push(row(
            80160,
            WRAPPER,
            FORGED_SID,
            Some("opencode serve --stdio"),
        ));
        table
    };
    assert_eq!(
        count(&wrapped(), WRAPPER, identified),
        Some(HashSet::from([FORGED_SID])),
        "a forged server under a shell is a command; the real one is not"
    );
    assert_eq!(
        count(
            &wrapped(),
            WRAPPER,
            Some(AgentServer {
                client_pid: CLIENT,
                server_pid: 80160,
            })
        ),
        Some(HashSet::from([SERVER, FORGED_SID])),
        "an identity whose pid's parent is not the client exempts nothing"
    );

    // A server whose command line was not read is not recognised, so it
    // counts as it did before rather than hiding what runs below it. A
    // `serve --stdio` a tool starts below it is in a session nested inside
    // that counted one, so it is part of that count rather than a second one
    // (`status/waiting/025`: a session nested in a detached session is the
    // same command).
    let mut table = standalone();
    table[1].command_line = CommandLine::Unavailable;
    table.push(row(80130, SERVER, TOOL_SERVER_SID, Some(SERVER_ARGV)));
    assert_eq!(
        count(&table, CLIENT, identified),
        Some(HashSet::from([SERVER])),
        "an unread server counts, and the tool server below it is inside that count"
    );

    // The identified pid, reused since the watch looked by a process whose
    // command line is not a server's, counts.
    let mut table = standalone();
    table[1].command_line = CommandLine::Read("/bin/bash -c sleep 60".to_string());
    assert_eq!(
        count(&table, CLIENT, identified),
        Some(HashSet::from([SERVER])),
        "a reused pid is not the server"
    );

    // OpenCode 1.x, and a pane whose OpenCode version the deck does not know:
    // the watch identifies no server, so nothing is exempted — not a shell
    // command quoting the server's words, not a `serve --stdio` a tool
    // started as the client's own child.
    let opencode_1 = vec![
        row(CLIENT, 1, CLIENT, None),
        row(80111, CLIENT, CLIENT, None),
        row(
            80140,
            CLIENT,
            80140,
            Some("/bin/bash -c sleep 600; opencode serve --stdio --port 0"),
        ),
        row(80141, 80140, 80140, None),
        row(
            80145,
            CLIENT,
            80145,
            Some("opencode serve --stdio --port 0"),
        ),
    ];
    assert_eq!(
        count(&opencode_1, CLIENT, None),
        Some(HashSet::from([80140, 80145])),
        "OpenCode 1.x counts exactly as before"
    );
    assert_eq!(
        count(&opencode_1, CLIENT, None),
        qualifying_command_sessions(&opencode_1, CLIENT, &[]),
    );

    // The command-line re-check reads words in place, not substrings.
    for server in [
        SERVER_ARGV,
        "opencode serve --stdio --port 0",
        "/opt/bin/opencode-1.18.35 serve --stdio",
    ] {
        assert!(is_opencode_private_server_command(server), "{server}");
    }
    for not_server in [
        "/bin/bash -c opencode serve --stdio --port 0",
        "/bin/bash -c : 'opencode serve --stdio'; sleep 60; :",
        "python3 x.py opencode serve --stdio --port 0",
        "/usr/bin/node /x/opencode serve --stdio",
        "opencode serve --service",
        "opencode --stdio serve",
        "notopencode serve --stdio",
        "opencode",
        "",
    ] {
        assert!(
            !is_opencode_private_server_command(not_server),
            "{not_server}"
        );
    }
}

/// Scenario: The daemon's OpenCode watch looks at a pane's process tree (executables and arguments, as it samples them) and names the private server the shell-activity scan may exempt. It names one only for a client whose version is known to be 2.x, and only that client's first direct child whose executable is OpenCode and whose arguments are `serve --stdio`: not on OpenCode 1.x (where a tool can start that shape as the client's child), not for an unknown version, not a process that only forged a server's arguments, not one a wrapper shell started, and not a second server. Fed into the scan, the named server is exempted and a forged one beside it still counts.
#[spec("status/waiting/017")]
#[test]
fn status_waiting_017_the_watch_names_only_a_known_opencode_2s_own_server() {
    use dot_agent_deck::opencode_mode::{ProcRow, Watch, Watched, evaluate};
    use dot_agent_deck::platform::proc::{AgentServer, qualifying_command_sessions_with_server};
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};

    const OC: &str = "/home/user/.opencode/bin/opencode";
    let proc_row = |pid: i32, ppid: i32, exe: &str, argv: &[&str]| ProcRow {
        pid,
        ppid,
        exe: Some(PathBuf::from(exe)),
        argv: (!argv.is_empty()).then(|| argv.iter().map(|a| a.to_string()).collect()),
    };
    let server_argv: &[&str] = &[OC, "serve", "--stdio", "--port", "0"];
    // The pane's shell (100) runs the client (101); the client's private
    // server (102); a forged `serve --stdio` the pane's shell started (103,
    // a Python whose argv claims to be OpenCode).
    let tree = || {
        vec![
            proc_row(100, 1, "/usr/bin/bash", &[]),
            proc_row(101, 100, OC, &["opencode", "--standalone"]),
            proc_row(102, 101, OC, server_argv),
            proc_row(
                103,
                100,
                "/usr/bin/python3",
                &["opencode", "serve", "--stdio", "--port", "0"],
            ),
        ]
    };
    let watched = |spawn_version| Watched {
        pane_id: "p".to_string(),
        agent_id: "agent-p".to_string(),
        root_pid: 100,
        spawn_version,
    };
    let unprobed = |_: &Path| None;
    let identify = |rows: &[ProcRow], spawn_version| {
        let mut watch = Watch::default();
        evaluate(
            &[watched(spawn_version)],
            rows,
            &mut watch,
            std::time::Instant::now(),
            &unprobed,
            &HashMap::new(),
        );
        watch.private_server("agent-p")
    };

    let identified = identify(&tree(), Some((2, 2, 5)));
    assert_eq!(
        identified,
        Some(AgentServer {
            client_pid: 101,
            server_pid: 102,
        }),
        "a known 2.x client's private server"
    );
    assert_eq!(
        identify(&tree(), Some((1, 19, 5))),
        None,
        "OpenCode 1.x: a `serve --stdio` child is a tool's"
    );
    assert_eq!(identify(&tree(), None), None, "an unknown version");

    // A second server child of the client is not the one named.
    let mut two = tree();
    two.push(proc_row(104, 101, OC, server_argv));
    assert_eq!(
        identify(&two, Some((2, 2, 5))),
        identified,
        "the first only"
    );

    // A client whose only `serve --stdio` child is not an OpenCode executable,
    // or that only the wrapper shell has, has no private server.
    let forged_only: Vec<ProcRow> = tree()
        .into_iter()
        .filter(|r| r.pid != 102)
        .chain([proc_row(105, 101, "/usr/bin/python3", server_argv)])
        .collect();
    assert_eq!(identify(&forged_only, Some((2, 2, 5))), None);

    // The client gone: the identity is forgotten on the next look.
    let mut watch = Watch::default();
    let now = std::time::Instant::now();
    evaluate(
        &[watched(Some((2, 2, 5)))],
        &tree(),
        &mut watch,
        now,
        &unprobed,
        &HashMap::new(),
    );
    assert!(watch.private_server("agent-p").is_some());
    let gone: Vec<ProcRow> = tree().into_iter().filter(|r| r.pid == 100).collect();
    evaluate(
        &[watched(Some((2, 2, 5)))],
        &gone,
        &mut watch,
        now,
        &unprobed,
        &HashMap::new(),
    );
    assert_eq!(watch.private_server("agent-p"), None);

    // Fed into the scan: the same tree as the shell-activity sampler sees it.
    let info = |pid: i32, ppid: i32, sid: i32, argv: Option<&str>| ProcessInfo {
        pid,
        ppid,
        session_id: sid,
        has_controlling_tty: sid == 100,
        session_leader: pid == sid,
        command_line: argv.map_or(CommandLine::NotSampled, |a| CommandLine::Read(a.into())),
    };
    let table = vec![
        info(100, 1, 100, None),
        info(101, 100, 100, None),
        info(102, 101, 102, Some(&server_argv.join(" "))),
        info(103, 100, 103, Some("opencode serve --stdio --port 0")),
    ];
    assert_eq!(
        qualifying_command_sessions_with_server(&table, 100, &[], identified),
        Some(HashSet::from([103])),
        "the identified server is exempt; the forged one beside it counts"
    );
    assert_eq!(
        qualifying_command_sessions_with_server(&table, 100, &[], None),
        Some(HashSet::from([102, 103])),
        "with no identity, both count"
    );
}

/// Scenario: A pane started directly on OpenCode 2.x has its client and private server identified by the daemon's OpenCode watch; then, without a respawn, another client takes the pane — a new process running a 1.x binary, a new process whose version the deck never recorded, or the same process `exec`ing a 1.x binary — and that client starts a tool command `opencode serve --stdio` as its direct child in a session of its own. The spawn's 2.x version no longer vouches for the pane's client, so the watch names no private server and the shell-activity scan counts that tool command (and what runs in its session), while the original client, still the one the deck started, keeps its own server exempt.
#[spec("status/waiting/018")]
#[test]
fn status_waiting_018_a_replacement_client_does_not_inherit_the_spawns_version() {
    use dot_agent_deck::opencode_mode::{ProcRow, Watch, Watched, evaluate};
    use dot_agent_deck::platform::proc::{AgentServer, qualifying_command_sessions_with_server};
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};

    const V2: &str = "/opt/v2/opencode";
    const V1: &str = "/opt/old/opencode-1.19.5";
    const UNPROBED: &str = "/opt/elsewhere/opencode";
    let proc_row = |pid: i32, ppid: i32, exe: &str, argv: &[&str]| ProcRow {
        pid,
        ppid,
        exe: Some(PathBuf::from(exe)),
        argv: (!argv.is_empty()).then(|| argv.iter().map(|a| a.to_string()).collect()),
    };
    // The pane runs `/opt/v2/opencode --standalone; <another OpenCode>; :`,
    // a direct command the spawn probed as 2.2.5.
    let watched = [Watched {
        pane_id: "p".to_string(),
        agent_id: "agent-p".to_string(),
        root_pid: 100,
        spawn_version: Some((2, 2, 5)),
    }];
    // Only the 1.x binary has a recorded version of its own; the 2.x client's
    // is known from the spawn, and `UNPROBED` from nothing.
    let version_of = |exe: &Path| (exe == Path::new(V1)).then_some((1, 19, 5));
    let first = vec![
        proc_row(100, 1, "/usr/bin/bash", &[]),
        proc_row(101, 100, V2, &["opencode", "--standalone"]),
        proc_row(102, 101, V2, &[V2, "serve", "--stdio", "--port", "0"]),
    ];
    // A client (`client_pid`, running `exe`) whose tool started a
    // `serve --stdio` (202) as the client's direct child.
    let replaced = |client_pid: i32, exe: &str| {
        vec![
            proc_row(100, 1, "/usr/bin/bash", &[]),
            proc_row(client_pid, 100, exe, &["opencode"]),
            proc_row(
                202,
                client_pid,
                V2,
                &[V2, "serve", "--stdio", "--port", "0"],
            ),
        ]
    };
    let t0 = std::time::Instant::now();
    let look = |watch: &mut Watch, rows: &[ProcRow], n: u32| {
        evaluate(
            &watched,
            rows,
            watch,
            t0 + std::time::Duration::from_secs(5 * u64::from(n)),
            &version_of,
            &HashMap::new(),
        );
        watch.private_server("agent-p")
    };

    // The scan's view of a replacement tree: the client on the pane's
    // session, the tool's server detached with a child of its own.
    let info = |pid: i32, ppid: i32, sid: i32, argv: Option<&str>| ProcessInfo {
        pid,
        ppid,
        session_id: sid,
        has_controlling_tty: sid == 100,
        session_leader: pid == sid,
        command_line: argv.map_or(CommandLine::NotSampled, |a| CommandLine::Read(a.into())),
    };
    let tool_server = format!("{V2} serve --stdio --port 0");
    let scan_of = |client_pid: i32| {
        vec![
            info(100, 1, 100, None),
            info(client_pid, 100, 100, None),
            info(202, client_pid, 202, Some(&tool_server)),
            info(203, 202, 202, None),
        ]
    };

    for (name, client_pid, exe) in [
        ("a new 1.x client", 201, V1),
        ("a new client of unknown version", 201, UNPROBED),
        ("the 2.x client exec'ing a 1.x binary", 101, V1),
    ] {
        let mut watch = Watch::default();
        assert_eq!(
            look(&mut watch, &first, 0),
            Some(AgentServer {
                client_pid: 101,
                server_pid: 102,
            }),
            "{name}: premise, the spawn's own 2.x client and its server"
        );
        // The pane runs no OpenCode for a look, then the replacement: a gap
        // does not make the replacement the spawn's client.
        let shell_only: Vec<ProcRow> = first.iter().filter(|r| r.pid == 100).cloned().collect();
        assert_eq!(look(&mut watch, &shell_only, 1), None, "{name}: no client");
        let identity = look(&mut watch, &replaced(client_pid, exe), 2);
        assert_eq!(identity, None, "{name}: no private server is named");
        assert_eq!(
            watch.private_servers(),
            vec![],
            "{name}: nothing to publish"
        );
        assert_eq!(
            qualifying_command_sessions_with_server(&scan_of(client_pid), 100, &[], identity),
            Some(HashSet::from([202])),
            "{name}: the tool's `serve --stdio` and its session count"
        );
    }

    // Without the gap, back to back, the same.
    let mut watch = Watch::default();
    assert!(look(&mut watch, &first, 0).is_some());
    assert_eq!(look(&mut watch, &replaced(201, V1), 1), None);

    // The spawn's own client, looked at again, keeps its server exempt.
    let mut watch = Watch::default();
    assert!(look(&mut watch, &first, 0).is_some());
    assert_eq!(
        look(&mut watch, &first, 1),
        Some(AgentServer {
            client_pid: 101,
            server_pid: 102,
        })
    );
    assert_eq!(
        watch.private_servers(),
        vec![(
            "agent-p".to_string(),
            AgentServer {
                client_pid: 101,
                server_pid: 102,
            }
        )]
    );
}

/// Scenario: Classify a Codex pane's process table as measured on Linux, where one command started through Codex's exec tool runs under `codex-linux-sandbox` in a session of its own and the bubblewrap inside it starts a second session for the command. The two nested sessions count as one command; a second command beside it counts as a second, and the agent's own children in its session count as none.
#[spec("status/waiting/025")]
#[test]
fn status_waiting_025_a_session_nested_in_a_detached_one_is_the_same_command() {
    use dot_agent_deck::platform::proc::{descendant_shell_activity, qualifying_command_sessions};
    use std::collections::HashSet;

    let row = |pid: i32, ppid: i32, session_id: i32, argv: &str| ProcessInfo {
        pid,
        ppid,
        session_id,
        has_controlling_tty: false,
        session_leader: pid == session_id,
        command_line: CommandLine::Read(argv.to_string()),
    };
    // The tree `status/waiting/024` printed (Codex 0.164.0, workspace-write
    // sandbox, 2026-11-11), pids kept: codex (in the agent's session) →
    // `codex-linux-sandbox` (setsid: session 538581) → `bwrap` (same session)
    // → `codex-linux-sandbox --apply-seccomp-then-exec` (bwrap's
    // `--new-session`: session 538586) → `bash -c` → `sleep`.
    const CODEX: i32 = 538240;
    const CODEX_SID: i32 = 537924;
    let mut table = vec![
        row(CODEX, 538226, CODEX_SID, "codex --model gpt-5.6-luna"),
        row(538558, CODEX, CODEX_SID, "codex-code-mode-host"),
        row(
            538581,
            CODEX,
            538581,
            "codex-linux-sandbox -- /bin/bash -c …",
        ),
        row(538585, 538581, 538581, "bwrap --new-session …"),
        row(
            538586,
            538585,
            538586,
            "codex-linux-sandbox --apply-seccomp-then-exec -- /bin/bash -c …",
        ),
        row(
            538587,
            538586,
            538586,
            "/bin/bash -c sleep 60 && touch done",
        ),
        row(538588, 538587, 538586, "sleep 60"),
    ];
    assert_eq!(
        qualifying_command_sessions(&table, CODEX, &[]),
        Some(HashSet::from([538581])),
        "the sandbox's inner session is part of the command its outer session runs — one \
         command, not two"
    );
    assert_eq!(descendant_shell_activity(&table, CODEX, &[]), Some(true));
    // A second command through the same tool is a second outer session.
    table.extend([
        row(
            538681,
            CODEX,
            538681,
            "codex-linux-sandbox -- /bin/bash -c …",
        ),
        row(538685, 538681, 538681, "bwrap --new-session …"),
        row(
            538686,
            538685,
            538686,
            "codex-linux-sandbox --apply-seccomp-then-exec",
        ),
    ]);
    assert_eq!(
        qualifying_command_sessions(&table, CODEX, &[]),
        Some(HashSet::from([538581, 538681]))
    );
    // A parent whose session could not be read keeps its child counted: that
    // is the direction that cannot lose a command.
    table
        .iter_mut()
        .find(|r| r.pid == 538685)
        .unwrap()
        .session_id = 0;
    assert_eq!(
        qualifying_command_sessions(&table, CODEX, &[]),
        Some(HashSet::from([538581, 538681, 538686]))
    );
    // With both commands gone only the agent's own children remain: none.
    table.retain(|r| r.session_id == CODEX_SID);
    assert_eq!(
        qualifying_command_sessions(&table, CODEX, &[]),
        Some(HashSet::new())
    );
}

/// Scenario: builds three pairs of fixture tables from the measured
/// `getsid`/`ps` captures in `.dot-agent-deck/386-argv-notes.md` and the
/// PRD — Table A (backbone plus the Bash-tool descendant), Table B (backbone
/// only), and a CI-trap variant of both where every row, the agent included,
/// reports no controlling terminal. Calls the not-yet-written
/// `descendant_shell_activity()` with the argv cross-check disabled
/// (`shapes: &[]`) throughout, and asserts Table A classifies busy, Table B
/// classifies idle, and the CI-trap variant of each classifies identically
/// to its non-trap counterpart — pinning both that the session-id test alone
/// excludes every measured confounder and that the exclusion survives the
/// container shape where a bare no-controlling-terminal test would collapse.
#[spec("status/shell-activity/003")]
#[test]
fn shell_activity_003_session_id_discriminator_classifies_the_measured_confounders() {
    // Table A: backbone + the measured Bash-tool descendant -> busy.
    let mut table_a = backbone_rows(false);
    table_a.push(bash_tool_descendant_row());
    assert_eq!(
        descendant_shell_activity(&table_a, AGENT_PID, &[]),
        Some(true),
        "a descendant in a different POSIX session than the agent must classify the pane as busy"
    );

    // Table B: backbone only, argv cross-check disabled -> idle. This is
    // what pins the claim the whole design rests on: the session-id test
    // alone, with no argv help, already excludes every measured confounder.
    let table_b = backbone_rows(false);
    assert_eq!(
        descendant_shell_activity(&table_b, AGENT_PID, &[]),
        Some(false),
        "every measured confounder (context7, task-master, engram, pysemgrep, caffeinate) shares \
         the agent's own session id; the session-id test alone, with the argv cross-check \
         disabled, must exclude all five"
    );

    // CI trap: same two tables, every row (agent included) now reports no
    // controlling terminal. Classification must be unchanged — the
    // discriminator compares session ids, and must never fall back to a
    // bare no-ctty test that would collapse here.
    let mut table_a_ci_trap = backbone_rows(true);
    table_a_ci_trap.push(bash_tool_descendant_row());
    assert_eq!(
        descendant_shell_activity(&table_a_ci_trap, AGENT_PID, &[]),
        Some(true),
        "classification of the busy table must not change when every process, including the \
         agent, has no controlling terminal (the CI/container shape)"
    );

    let table_b_ci_trap = backbone_rows(true);
    assert_eq!(
        descendant_shell_activity(&table_b_ci_trap, AGENT_PID, &[]),
        Some(false),
        "classification of the confounder-only table must not change under the same CI-trap \
         condition — this is the direct regression test for a bare no-controlling-terminal \
         fallback"
    );
}

// ---------------------------------------------------------------------------
// M3 — `AgentPtyRegistry::shell_foreground_busy_snapshot` (the public seam
// over `RunningAgent::shell_foreground_busy`) wired onto the descendant scan.
// Unix only — spawns a real PTY pane and a real `setsid()`'d, `ps`-visible
// child; there is no process table to scan on Windows.
// ---------------------------------------------------------------------------

/// RAII guard for the detached target's real pid, so a panic between
/// discovering it and the explicit kill below still reaps it rather than
/// leaking a 120s `sleep` into the runner's process tree — the same pattern
/// `SpawnedGrandchild` uses in `status/shell-activity/001`. A second SIGKILL
/// after the process has already exited just returns ESRCH, which this
/// ignores.
#[cfg(unix)]
struct KillOnDrop(Option<i32>);

#[cfg(unix)]
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // SAFETY: `pid` is a real pid this process learned from its own
            // `process_table()` sample; SIGKILL on an already-exited pid is a
            // no-op ESRCH.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

/// Scenario: spawns a real PTY shell tagged as a Claude pane and confirms it
/// reads idle before releasing a gated, Bash-tool-shaped detached child whose
/// startup deliberately exceeds the old four-second busy poll.
/// Waits independently for that child's exec and verifies it has no controlling
/// terminal, leads its own session, and differs from the shell's session;
/// Asserts the pane reads busy while the child lives and idle after killing it.
#[cfg(unix)]
#[spec("status/shell-activity/004")]
#[test]
fn shell_activity_004_shell_foreground_busy_flips_for_a_real_detached_pipe_child() {
    // Issue #668: armed before anything is spawned, so the pane shell below —
    // and every descendant it mints — carries the wrapped-child lifetime bound
    // that lets `wrap` reap a stranded child.
    child_lifetime_bound::arm();

    const PANE_ID: &str = "shell-activity-004-pane";
    // A real process-table sample has taken 19–20s under build contention
    // (issue #862). Allow 30s for setup and each signal edge between samples,
    // while the child outlives those waits; a synchronous sample can overrun
    // the budget, so this is a polling bound rather than a timeout on ps.
    const SIGNAL_BUDGET: Duration = Duration::from_secs(30);
    let marker = format!("shell-activity-004-target-{}", std::process::id());
    // A fixed `sleep 0.3` before the detached child launched used to give the
    // pre-edge assertion below an "observable idle window" to sample inside
    // — but that window is a race, not a guarantee: once `spawn_agent` +
    // registry setup is delayed past ~300ms by machine load, the first
    // successful sample already sees the detached child, and the pre-edge
    // assertion fails even though the production code is correct (confirmed
    // by the maintainer's PR #390 review: 5/5 passes in isolation, fails only
    // under full-tier contention). Gate the child behind a ready-marker file
    // this process controls instead: the script busy-waits on the marker's
    // existence, so the detached child structurally cannot exist until this
    // test has already sampled and asserted the idle state below — the
    // pre-edge read no longer depends on winning a timing race at all. The
    // python3 one-liner setsid()'s itself (detaching from the pane's
    // controlling terminal and becoming its own session leader, exactly as
    // Claude Code's Bash-tool child does) and execv's into `/bin/sleep 120`
    // with an argv crafted to carry the measured Bash-tool shape
    // (`shell-snapshots/snapshot-` and `&& eval `) so the argv cross-check is
    // exercised against a real process, not just a fixture string. 120s is a
    // backstop bound in case the test panics before the explicit
    // kill below runs; `KillOnDrop` and the explicit kill both aim to end it
    // long before that. The trailing `sleep 120` keeps the pane's own shell
    // alive (and so its registry entry) past the detached child's death —
    // without it the shell has nothing left to run and exits the instant the
    // killed child is reaped, so the pane disappears from the snapshot
    // instead of reading idle.
    let fixture = test_temp::tempdir().expect("shell-activity fixture directory");
    let ready_marker = fixture.path().join("ready");
    let target_argv = format!("shell-snapshots/snapshot- && eval {marker}");
    // The ready path is interpolated inside shell double quotes below.
    assert!(
        !ready_marker
            .to_string_lossy()
            .contains(['"', '$', '`', '\\', '\n', '\r']),
        "fixture path cannot be interpolated into the shell command: {ready_marker:?}"
    );
    let command = format!(
        "while [ ! -e \"{ready}\" ]; do sleep 0.05; done; python3 -c \"import os, time; time.sleep(5); os.setsid(); \
         os.execv('/bin/sleep', ['{target_argv}', '120'])\"; sleep 120",
        ready = ready_marker.display(),
    );
    // Keep the five-second startup delay as a regression stimulus: the old
    // four-second busy poll deterministically said idle before setsid ran.
    // Readiness must be established independently of how quickly the host
    // gets around to launching the detached child.

    let registry = Arc::new(AgentPtyRegistry::new());
    let id = registry
        .spawn_agent(SpawnOptions {
            command: Some(&command),
            env: vec![
                (DOT_AGENT_DECK_PANE_ID.to_string(), PANE_ID.to_string()),
                // Pins the `-c` wrap shell so the script's syntax is
                // predictable across developer machines regardless of login
                // shell; consumed by the wrap decision only, never exported
                // into the child's own environment.
                ("SHELL".to_string(), "/bin/sh".to_string()),
            ],
            // Load-bearing for the argv cross-check claim below:
            // `shell_tool_shape_key` selects `CLAUDE_BASH_TOOL_SHAPE` only
            // for `AgentType::ClaudeCode`, and `&[]` for `None` — so without
            // this the `&[CLAUDE_BASH_TOOL_SHAPE]` passed to
            // `shell_foreground_busy_snapshot` below is filtered out before
            // the scan ever sees it, and the crafted argv is never actually
            // checked.
            agent_type: Some(dot_agent_deck::event::AgentType::ClaudeCode),
            ..SpawnOptions::default()
        })
        .expect("spawn should succeed");
    let shell_pid = registry
        .child_pid(&id)
        .expect("spawned agent must expose a pid") as i32;

    let mut target_guard = KillOnDrop(None);

    let busy_for_pane = |registry: &AgentPtyRegistry| -> Option<bool> {
        registry
            .shell_foreground_busy_snapshot(&[CLAUDE_BASH_TOOL_SHAPE])
            .unwrap_or_default()
            .into_iter()
            .find(|(pane_id, _)| pane_id == PANE_ID)
            .map(|(_, busy)| busy)
    };

    // Falling edge before the rising edge: the script is blocked on the
    // ready marker and structurally cannot have launched the detached child
    // yet, so the pane must read idle. No longer a race against a fixed
    // sleep — the only thing this loop can observe before the marker is
    // written below is `None` (pane not registered yet) or `Some(false)`.
    let deadline = Instant::now() + SIGNAL_BUDGET;
    let mut state = busy_for_pane(&registry);
    while state != Some(false) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        state = busy_for_pane(&registry);
    }
    assert_eq!(
        state,
        Some(false),
        "an idle shell with no detached descendant yet must not read busy"
    );

    // Release the gated shell now that the idle read above is confirmed —
    // only past this point can the detached child come into existence.
    std::fs::write(&ready_marker, b"go")
        .expect("write ready marker to release the gated shell script");

    // Establish the stimulus BEFORE asking the registry to classify it.
    // The old 4s busy poll also paid for shell/python startup, and timed out
    // with Some(false) when no detached child existed yet. Wait on the real
    // process topology instead. The argv prefix proves execv has completed:
    // a detached python still running its -c script also contains the marker,
    // but is not yet the Bash-tool-shaped sleep this test needs.
    // Issue #862: sample with the shell as a root to obtain candidate argv.
    let startup = Instant::now();
    let deadline = startup + SIGNAL_BUDGET;
    let (own_row, target) = loop {
        let table = process_table(&[shell_pid]).expect("process_table() must enumerate on unix");
        let own_row = table.iter().find(|p| p.pid == shell_pid);
        let children = descendants(&table, shell_pid);
        let target = children.iter().find(|p| {
            p.command_line.read().is_some_and(|argv| {
                argv.strip_prefix(&target_argv)
                    .is_some_and(|tail| tail.starts_with(' '))
            })
        });
        if let (Some(own_row), Some(target)) = (own_row, target) {
            break (own_row.clone(), (*target).clone());
        }
        assert!(
            Instant::now() < deadline,
            "detached child never reached its exec-ready topology; shell={own_row:?}, \
             descendants={children:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    target_guard.0 = Some(target.pid);
    eprintln!(
        "detached child exec-ready after {:?}: {target:?}",
        startup.elapsed()
    );
    assert!(
        !target.has_controlling_tty,
        "the detached child must have no controlling terminal — it never touches the pane's \
         PTY: {target:?}"
    );
    assert!(
        target.session_leader,
        "a setsid()'d child must be its own session leader: {target:?}"
    );
    assert_ne!(
        target.session_id, own_row.session_id,
        "the detached child must be in a different POSIX session than the pane's own shell — \
         the load-bearing condition the whole discriminator rests on: {target:?}"
    );

    // Rising edge: the independently verified child lives throughout this
    // wait, so a classifier miss is distinct from delayed fixture startup.
    let deadline = Instant::now() + SIGNAL_BUDGET;
    let mut state = busy_for_pane(&registry);
    while state != Some(true) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        state = busy_for_pane(&registry);
    }
    assert_eq!(
        state,
        Some(true),
        "a verified detached-on-pipes Bash-tool-shaped child must read busy: {target:?}"
    );

    // Falling edge: kill the detached child and confirm the signal clears. A
    // test that only asserted the rising edge would pass against an
    // implementation that never clears.
    // SAFETY: `target.pid` was just read from this process's own
    // `process_table()` sample.
    unsafe {
        libc::kill(target.pid, libc::SIGKILL);
    }
    let deadline = Instant::now() + SIGNAL_BUDGET;
    let mut state = busy_for_pane(&registry);
    while state != Some(false) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        state = busy_for_pane(&registry);
    }
    assert_eq!(
        state,
        Some(false),
        "once the detached descendant exits the pane must read idle again — a test that only \
         asserts the rising edge would pass against an implementation that never clears"
    );

    registry.close_agent(&id).unwrap();
}

// ---------------------------------------------------------------------------
// Windows contract half — `windows-latest` runs `cargo nextest run` (the
// fast tier, no `--features e2e`) in CI, so this genuinely executes there
// even though it cannot be run from this macOS worktree.
// ---------------------------------------------------------------------------

/// Scenario: on Windows there is no process-enumeration backend (PRD #386
/// scopes native Windows process walking out, matching the existing
/// `foreground_pgid`'s `None` contract). Calls the not-yet-written
/// `process_table()` and asserts it reports `None` rather than a table, so
/// an accidental future Windows implementation is caught here rather than
/// shipping unverified.
#[cfg(windows)]
#[spec("status/shell-activity/001")]
#[test]
fn shell_activity_001_process_table_is_none_on_windows() {
    assert_eq!(
        process_table(&[std::process::id() as i32]),
        None,
        "process_table() must be None on Windows, matching foreground_pgid's existing contract"
    );
    assert_eq!(
        process_table(&[]),
        None,
        "and with no roots too — issue #862's argv phase changes nothing here, since there \
         is no table for it to run a second phase against"
    );
}
