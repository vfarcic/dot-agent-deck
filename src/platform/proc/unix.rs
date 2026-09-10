//! Unix process lifecycle: `killpg`/`kill` signal teardown + `getppid`.
//! Behavior-preserving lift of the signal helpers from `agent_pty.rs`, the
//! daemon-stop kill from `build_version_handshake.rs`, and `current_ppid` from
//! `daemon.rs`.

use std::time::Duration;

// ---------------------------------------------------------------------------
// Agent process-group teardown (lifted from agent_pty.rs).
// ---------------------------------------------------------------------------

/// PRD #163 M3 — Unix has nothing to hold here.
///
/// An agent's descendant tree is already addressable on Unix: `portable-pty`
/// `setsid`s the child, which makes it a process-group leader, so `killpg(pid,
/// …)` reaches the agent *and* everything it spawned with no bookkeeping at all.
/// This zero-sized type exists only so the spawn/teardown seam has the same
/// shape on both platforms — Windows has no implicit grouping and must own a Job
/// Object handle for the agent's whole lifetime (see the windows backend), which
/// is why the handle has to be created at spawn and carried on
/// [`crate::agent_pty::RunningAgent`].
///
/// Being a ZST, it is `Send`/`Sync`/`Default` for free and costs nothing in
/// `AgentPty`/`RunningAgent`; the Unix teardown paths ignore it entirely and
/// keep using `killpg`.
#[derive(Debug, Default, Clone, Copy)]
pub struct AgentProcessGroup;

impl AgentProcessGroup {
    /// No-op on Unix: the process group the kill paths use is the one
    /// `portable-pty`'s `setsid` already established, so there is nothing to
    /// create and nothing that can fail.
    pub fn adopt(_pid: Option<u32>) -> Self {
        Self
    }
}

/// PRD #92 F1 followup (defensive): convert a portable-pty `process_id()`
/// (a `u32`) into a positive `libc::pid_t` suitable for `killpg`, or `None` if
/// the raw value can't legally name a process group.
///
/// `killpg(pgid, sig)` has two dangerous degenerate cases for non-positive
/// `pgid`:
///   - `pgid == 0` is documented as "signal every process in *the caller's*
///     process group" — which for the daemon would mean signalling the daemon
///     itself plus every connected attach-client.
///   - `pgid < 0` is undefined behavior in POSIX and a likely overflow
///     indicator (a `u32` PID that didn't fit in `i32`).
///
/// Both should be impossible from a well-behaved `portable-pty` spawn (Linux
/// PIDs are positive `i32` values up to `i32::MAX`), but defensively checking
/// is one `if` and one unit test, which is much cheaper than the unbounded
/// blast radius of getting it wrong. On `None` the caller falls back to
/// `child.kill()` (single-PID).
pub(crate) fn pid_to_pgid(pid: u32) -> Option<libc::pid_t> {
    let signed = pid as i64;
    if signed > 0 && signed <= libc::pid_t::MAX as i64 {
        Some(signed as libc::pid_t)
    } else {
        None
    }
}

/// Low-level shared helper. Send `signal` to the child's process group,
/// falling back to `portable_pty::Child::kill` when `pid_to_pgid` rejects the
/// raw pid (F1-followup defensive boundary check). `phase` is included in
/// `tracing::warn!` payloads so a wedged child can be traced back to whichever
/// phase issued the kill. Returns `true` if the `killpg` syscall actually fired
/// (or the `child.kill` fallback was used), `false` if the syscall reported an
/// error other than ESRCH.
fn signal_child_pgroup_or_fallback(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    signal: libc::c_int,
    phase: &'static str,
) -> bool {
    let raw_pid = child.process_id();
    let pgid = raw_pid.and_then(pid_to_pgid);
    let Some(pgid) = pgid else {
        // PRD #92 F8 followup (auditor #2 — option b documented):
        // pid_to_pgid rejected the raw pid (either `process_id()` returned
        // `None` or the pid was outside the safe `(0, i32::MAX]` range). The
        // portable-pty `Child` trait allows `None` here, but the Unix backend
        // used by this codebase always returns `Some` in practice. The
        // `(0, i32::MAX]` boundary check is defense-in-depth against a future
        // portable-pty bug; on real Linux/macOS PIDs it never fails. The
        // fallback below uses `portable_pty::Child::kill`, which sends SIGHUP
        // — strictly weaker than the requested `signal` (typically SIGTERM or
        // SIGKILL) and limited to the direct child (no process-group
        // semantics, so descendants leak). The caller's subsequent
        // `child.wait()` is unbounded — that's acceptable for the same "this
        // branch is practically unreachable" reason.
        //
        // Auditor #5: emit a warn-level event so a descendant leak surfaced
        // via this fallback is at least observable.
        tracing::warn!(
            ?raw_pid,
            signal,
            phase = %phase,
            reason = if raw_pid.is_none() { "process_id-returned-none" } else { "pid_to_pgid-rejected" },
            "signal_child_pgroup_or_fallback: pgid unavailable — falling back to portable_pty::Child::kill (SIGHUP, single-PID; descendants will leak)"
        );
        let _ = child.kill();
        return true;
    };
    // SAFETY: `killpg(2)` is async-signal-safe; the pgid we just validated via
    // `pid_to_pgid` is the child's own PID (portable-pty `setsid`'d it, making
    // it the group leader), so this cannot affect any other agent's group.
    let rc = unsafe { libc::killpg(pgid, signal) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        let benign = err.raw_os_error() == Some(libc::ESRCH);
        if !benign {
            tracing::warn!(pgid, signal, phase = %phase, error = %err, "killpg failed");
        }
        return benign;
    }
    true
}

/// Forcefully terminate the child *and every descendant in its process group*
/// with SIGKILL and reap it. SIGKILL is preferred over
/// `portable_pty::Child::kill()` (which sends SIGHUP) because a shell can
/// ignore SIGHUP — leaving the subsequent `wait()` to block forever. SIGKILL
/// cannot be caught or ignored, so the kernel tears the process down and
/// `wait()` returns promptly. Callers should drop the master/writer/reader
/// handles before invoking this so any I/O blocked on the PTY unblocks first.
///
/// `_group` is the cross-platform teardown handle (PRD #163 M3) and is unused on
/// Unix — see [`AgentProcessGroup`]; the process group `killpg` addresses is
/// implicit in the child's own pid.
pub fn force_kill_child_and_wait(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    group: &AgentProcessGroup,
) {
    force_kill_child_group(child, group);
    let _ = child.wait();
}

/// The SIGNAL half of [`force_kill_child_and_wait`]: `killpg(SIGKILL)` the
/// child's whole process group and return **without reaping**.
///
/// Issue #581 — the two halves are separable because they starve each other
/// when a caller holds several agents. The `wait()` above is unbounded (see the
/// note in [`signal_child_pgroup_or_fallback`]): a child wedged in
/// uninterruptible kernel I/O does not die on SIGKILL until that I/O completes,
/// so a loop that signals-then-waits per agent parks in the first wedged
/// agent's `wait()` and never signals the agents behind it. Splitting the
/// signal out lets such a caller deliver every kill first and reap afterwards —
/// see `AgentPtyRegistry::force_kill_and_reap_all`, the only caller.
///
/// `_group` is unused on Unix for the same reason as in
/// [`force_kill_child_and_wait`].
pub fn force_kill_child_group(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    _group: &AgentProcessGroup,
) {
    signal_child_pgroup_or_fallback(child, libc::SIGKILL, "force-kill");
}

/// SIGTERM-then-SIGKILL escalation used by the single-pane Ctrl+W path. Sends
/// `SIGTERM` to the child's process group, polls `try_wait` until the child
/// exits or `grace` elapses, then sends `SIGKILL` as the backstop and reaps the
/// child.
///
/// `_group` is unused on Unix (see [`force_kill_child_and_wait`]).
pub fn terminate_child_with_grace_and_wait(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    grace: Duration,
    _group: &AgentProcessGroup,
) {
    // Phase 1: SIGTERM the process group.
    signal_child_pgroup_or_fallback(child, libc::SIGTERM, "graceful-close-sigterm");

    // Phase 2: poll `try_wait` until the child exits or the grace elapses.
    // Polling avoids the obvious "sleep for grace then SIGKILL" alternative —
    // a child that exits promptly after SIGTERM doesn't have to wait around
    // for the deadline. 50 ms cadence is small enough to feel responsive and
    // large enough to keep CPU cost negligible (~60 polls over 3 s).
    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(_) => break,
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Phase 3: SIGKILL backstop. Reaches survivors regardless of
    // SIGTERM-trapping state.
    signal_child_pgroup_or_fallback(child, libc::SIGKILL, "graceful-close-sigkill");
    let _ = child.wait();
}

/// SIGTERM the child's process group without waiting (the daemon-wide
/// `shutdown_all_graceful` SIGTERM phase issues this to every agent in
/// parallel and polls them together). `phase` tags the `tracing` payload.
pub fn send_sigterm_to_child_group(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    phase: &'static str,
) {
    signal_child_pgroup_or_fallback(child, libc::SIGTERM, phase);
}

// ---------------------------------------------------------------------------
// Foreground process-group query (PRD #370 M1).
// ---------------------------------------------------------------------------

/// The pty's current foreground process-group id (`tcgetpgrp` under the
/// hood, via `portable_pty::MasterPty::process_group_leader`), or `None` if
/// the backend can't report one (e.g. the master fd is already closed).
///
/// This is the Unix half of the [`foreground_pgid`] seam; see `windows.rs`
/// for why the Windows half is an unconditional `None` rather than a
/// best-effort implementation.
pub fn foreground_pgid(master: &dyn portable_pty::MasterPty) -> Option<i32> {
    master.process_group_leader()
}

// ---------------------------------------------------------------------------
// Process-table sample (PRD #386 M1).
// ---------------------------------------------------------------------------

/// A process's POSIX session id, or a negative value if it could not be read
/// (the usual cause being that the process exited between the `ps` sample and
/// this call, giving `ESRCH`).
///
/// **This is deliberately not `ps -o sess=`**, which prints `0` for a non-root
/// caller on macOS and is useless for the discriminator. `getsid(2)` is POSIX,
/// behaves identically on macOS and Linux, works on any pid rather than only on
/// children, and needs no `/proc` parsing on Linux either.
fn getsid_or_negative(pid: i32) -> i32 {
    // SAFETY: `getsid(2)` is async-signal-safe and has no side effects; it
    // either reports the target's session id or returns -1 with `errno` set.
    unsafe { libc::getsid(pid) }
}

/// The **bulk phase**'s `ps` invocation, shared by both samplers so the sync and
/// async paths can never drift into asking `ps` for different columns.
///
/// The trailing `=` on every `-o` field suppresses the header line entirely.
/// `-w -w` is kept even though the argv column is gone: BSD `ps` truncates its
/// output to the window width (79 columns when stdout is not a terminal), and
/// while three short columns are nowhere near that, keeping the flag makes
/// `args=` the *only* thing this invocation lost and removes truncation from the
/// set of things a macOS reader has to reason about.
///
/// **Issue #862 removed `args=`, and that removal is the whole point of the
/// change.** Measured with `strace` on procps-ng 4.0.4, per invocation on a
/// 382-process box:
///
/// | column set | `stat` | `status` | `cmdline` | `environ` |
/// | --- | --- | --- | --- | --- |
/// | `pid=,ppid=,tty=` | 397 | 396 | **0** | **0** |
/// | `pid=,ppid=,tty=,args=` | 374 | 373 | **372** | **372** |
///
/// So `args` cost *two* extra per-process reads, not one. Both
/// `/proc/<pid>/cmdline` and `/proc/<pid>/environ` are served through the
/// kernel's `access_remote_vm()`, which takes the **target's** `mmap_lock`;
/// `stat` and `status`, which is where `pid`/`ppid`/`tty` come from, are not.
/// That made the sample's wall time the sum of every unrelated process's
/// `mmap_lock` wait, which is the mechanism behind the 19-20 s field sample
/// recorded in issue #862. The command lines the cross-check actually needs are
/// read in the second phase — see [`PS_COMMAND_LINE_ARGS`].
const PS_TABLE_ARGS: [&str; 5] = ["-A", "-w", "-w", "-o", "pid=,ppid=,tty="];

/// The **argv phase**'s `ps` invocation prefix, to which the caller appends a
/// comma-separated pid list (issue #862).
///
/// `-w -w` disables `ps`'s width truncation on both macOS and Linux, so the argv
/// survives whole — the [`super::ShellToolShape`] cross-check substring-matches
/// inside it, and a truncated command line would silently stop matching.
///
/// This phase runs **only when the bulk table shows at least one session-boundary
/// descendant** of one of the sample's roots
/// (`super::shell_tool_candidates` — read its doc comment, the narrowing there is
/// what keeps this phase from re-reading a whole build tree). So it costs nothing
/// on an idle deck, and on a busy one it reads one command line per `setsid`-ed
/// shell-tool call — always a process the deck itself spawned, never an unrelated
/// one, and never the `cargo`/`rustc`/`ld` subtree below that call.
///
/// Kept as one portable `ps` call rather than split into a Linux
/// `/proc/<pid>/cmdline` read and a macOS `sysctl(KERN_PROCARGS2)`: those would
/// remove this phase's fork/exec, but they are two platform-specific
/// implementations to maintain for a fork that only happens while a pane is
/// genuinely running a shell command, and Route A's whole premise is one
/// identical invocation on both platforms.
const PS_COMMAND_LINE_ARGS: [&str; 4] = ["-w", "-w", "-o", "pid=,args="];

/// Turn a finished bulk-phase `ps` run into a table, or `None` when it said
/// nothing usable.
///
/// Shared by [`process_table`] and [`process_table_async`]. The `getsid(2)` per
/// row happens here: it is a pure kernel lookup with no I/O wait, which is why
/// it is safe to leave on the caller's thread even in the async path (measured
/// at ~1.4 ms for ~620 rows in a release build).
///
/// Every row comes back [`super::CommandLine::NotSampled`]; the argv phase fills
/// in the few that need one.
fn table_from_ps_output(success: bool, stdout: &[u8]) -> Option<Vec<super::ProcessInfo>> {
    if !success {
        return None;
    }
    let stdout = String::from_utf8_lossy(stdout);
    let rows = super::scan::parse_ps_table(&stdout, &getsid_or_negative);
    if rows.is_empty() { None } else { Some(rows) }
}

/// The pid list argument for the argv phase: `-p 123,456`.
///
/// Split out so both samplers build it identically and so the formatting is
/// unit-testable without spawning anything.
fn ps_pid_list(pids: &[i32]) -> String {
    pids.iter()
        .map(|pid| pid.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Turn a finished argv-phase `ps` run into `pid → command line`.
///
/// **The exit status is deliberately not consulted, unlike
/// [`table_from_ps_output`]'s.** `ps -p <list>` reports failure when *no* pid in
/// the list matched, and on Linux/procps returns 0 while printing the survivors
/// when only some did (measured) — but that partial-match status is not
/// something this project has verified on BSD `ps`, and a platform that returned
/// non-zero there would silently cost every busy pane its cross-check because
/// one *other* pane's candidate happened to exit during the sample. Parsing
/// whatever came out is safe in either case: a `ps` that genuinely failed writes
/// its complaint to stderr and leaves stdout empty, so the map comes back empty
/// on its own.
///
/// An empty map is not an error: the bulk table is still perfectly good, and
/// every wanted pid then reads [`super::CommandLine::Unavailable`], which the
/// classifier treats as "not a match" — the same reading a process that exited
/// between the phases gets. A failed argv phase therefore costs a pane one poll
/// of the cross-check, not the whole sample.
fn command_lines_from_ps_output(stdout: &[u8]) -> std::collections::HashMap<i32, String> {
    super::scan::parse_ps_command_lines(&String::from_utf8_lossy(stdout))
}

/// Sample every process on the machine into a [`super::ProcessInfo`] table
/// (PRD #386 M1, Route A), or `None` if `ps` could not be run or produced
/// nothing parseable.
///
/// `roots` are the pids the sample is being taken on behalf of — each pane's PTY
/// child. They select **whose command line gets read** (issue #862): the bulk
/// `ps -A` asks for no argv column at all, and a second `ps` then reads the
/// command line of exactly the detached descendants of these roots, which is
/// what [`super::command_line_targets`] computes. Pass an empty slice to skip
/// the argv phase entirely and get a table whose every row is
/// [`super::CommandLine::NotSampled`] — useful when the caller only wants the
/// structural test, which reads no command line.
///
/// One `ps -A` per call, parsed once and reusable for *every* pane in that poll,
/// so the cost is one fork/exec per poll cycle rather than per pane; the argv
/// phase adds a second fork/exec only when some root actually has a detached
/// descendant, and it is batched across every root. The session id of each row
/// comes from [`getsid_or_negative`], not from `ps`.
///
/// **Synchronous and unbounded — never call this from an async task** (issue
/// #429). It blocks the calling thread for the whole `ps` run, and forever if
/// `ps` wedges in D-state on a stuck filesystem. [`process_table_async`] is the
/// variant for a Tokio context. This one remains for synchronous callers and
/// tests.
///
/// **Route B (native enumeration) was declined on the issue #862 measurement**,
/// which is what PRD #386's M5 asked for. Measured on this box (Linux 7.0.0,
/// 16 cores, 382 processes, warm): the bulk phase costs 12 ms against a 12.5 ms
/// `ps -p 1` fork/exec floor, i.e. enumerating the whole machine's `pid`/`ppid`
/// is below measurement resolution and the fork *is* the cost. So Route B would
/// remove ~12 ms of fork/exec per tick and nothing else — while reading the same
/// `/proc/<pid>/cmdline` files that made the sample stall in the first place, and
/// costing two platform-specific implementations. It optimizes the half that was
/// never the problem. See `prds/386-descendant-scan-shell-activity-signal.md`
/// (M5) for the full numbers.
pub fn process_table(roots: &[i32]) -> Option<Vec<super::ProcessInfo>> {
    let output = std::process::Command::new("ps")
        .args(PS_TABLE_ARGS)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let mut table = table_from_ps_output(output.status.success(), &output.stdout)?;
    let wanted = super::command_line_targets(&table, roots);
    if wanted.is_empty() {
        return Some(table);
    }
    let argv = std::process::Command::new("ps")
        .args(PS_COMMAND_LINE_ARGS)
        .arg("-p")
        .arg(ps_pid_list(&wanted))
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .map(|out| command_lines_from_ps_output(&out.stdout))
        .unwrap_or_default();
    super::fill_command_lines(&mut table, &wanted, &argv);
    Some(table)
}

/// [`process_table`] for an async caller: the same sample, but awaited instead
/// of blocked on (issue #429).
///
/// Two properties the synchronous version cannot offer, both load-bearing for
/// the daemon's 2 Hz shell-activity poll:
///
/// - **It does not occupy a Tokio worker thread.** The wait for `ps` to exit is
///   a real `await` on the runtime's child reaper, so the worker goes back to
///   the queue for the time the sample takes instead of sitting in `waitpid`.
///   That wait every 500 ms — *even with zero panes open*, before issue #493 —
///   is what previously stalled hook ingestion, client requests and daemon
///   shutdown behind this signal. `spawn_blocking` would only relocate that
///   stall to the blocking pool; worse, `tokio::time::timeout` around a
///   `spawn_blocking` handle does not cancel the thread, so a permanently-wedged
///   `ps` at 2 Hz would leak one pool thread per tick until the 512-thread cap.
///   Awaiting an async child is what actually fixes it.
/// - **It is cancel-safe, so a timeout can genuinely bound it.** `kill_on_drop`
///   means dropping this future — which is exactly what
///   [`tokio::time::timeout`] does on expiry — kills the `ps` child and leaves
///   it to the runtime's orphan reaper rather than abandoning it. Both phases
///   are inside this one future, so the caller's single deadline bounds the
///   whole sample and the argv phase needs no deadline of its own.
///
/// **Callers MUST wrap this in a timeout**; it has no internal deadline, and a
/// `ps` wedged in D-state never returns. The deadline lives at the call site
/// (see `run_shell_activity_monitor`) because the *interpretation* of a blown
/// deadline is the caller's: a timed-out sample means "no opinion", never "not
/// busy".
pub async fn process_table_async(roots: &[i32]) -> Option<Vec<super::ProcessInfo>> {
    // `output()` forces `stdout`/`stderr` to pipes (tokio, unlike `std`, leaves
    // `stdin` alone — hence the explicit null), and `wait_with_output` drains
    // both concurrently, so the captured-and-discarded stderr cannot deadlock.
    let output = tokio::process::Command::new("ps")
        .args(PS_TABLE_ARGS)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    let mut table = table_from_ps_output(output.status.success(), &output.stdout)?;
    let wanted = super::command_line_targets(&table, roots);
    if wanted.is_empty() {
        return Some(table);
    }
    let argv = match tokio::process::Command::new("ps")
        .args(PS_COMMAND_LINE_ARGS)
        .arg("-p")
        .arg(ps_pid_list(&wanted))
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
    {
        Ok(out) => command_lines_from_ps_output(&out.stdout),
        Err(_) => std::collections::HashMap::new(),
    };
    super::fill_command_lines(&mut table, &wanted, &argv);
    Some(table)
}

// ---------------------------------------------------------------------------
// Daemon-stop termination by PID (lifted from build_version_handshake.rs).
// ---------------------------------------------------------------------------

/// Convert a `u32` PID (as returned by `peer_pid()`) into the `pid_t` (`i32`)
/// shape `libc::kill` wants, refusing values that would dangerously change the
/// syscall's meaning:
/// - `pid == 0`: `kill(0, sig)` broadcasts to every process in the calling
///   process group — would take down the parent shell.
/// - `pid > i32::MAX`: the `as i32` cast would wrap to a negative value.
///   `kill(-pgid, sig)` means "signal every process in process group `pgid`" —
///   a wildcard kill. Refuse rather than send.
/// - resulting `i32 <= 0` after the cast: defense-in-depth for any path that
///   bypasses the explicit checks above.
fn checked_signal_pid(pid: u32) -> std::io::Result<libc::pid_t> {
    if pid == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "peer pid is 0; refusing to kill(0, SIGTERM) (would broadcast to process group)",
        ));
    }
    if pid > i32::MAX as u32 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "peer pid {pid} does not fit in pid_t; refusing kill() (negative i32 would target a process group)"
            ),
        ));
    }
    let signed = pid as libc::pid_t;
    if signed <= 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("peer pid {pid} resolves to non-positive pid_t {signed}; refusing kill()"),
        ));
    }
    Ok(signed)
}

/// Send `SIGTERM` to `pid` (the daemon-stop graceful signal). Guards against
/// pid 0 / overflow that would turn the signal into a process-group broadcast.
///
/// `ESRCH` (no such process) is **not** an error: it means the daemon already
/// exited, which is a clean already-gone success for the caller (the
/// `daemon stop` path racing a self-exiting daemon, and the re-resolve fallback
/// in `build_version_handshake` that documents "SIGTERM lands as ESRCH").
/// Rather than collapsing that case into an indistinguishable `Ok(())` — which
/// would force `terminate_daemon_graceful` into the same poll/escalate loop it
/// runs for a signal that *was* delivered — ESRCH surfaces as the distinct
/// [`TerminateSignal::AlreadyGone`] so the caller can short-circuit straight to
/// `Stopped`, exactly matching the pre-refactor `terminate_daemon_graceful` on
/// `main` (which special-cased ESRCH to an immediate `Ok(Stopped)`). Any other
/// errno is a genuine failure and is **not** swallowed.
pub fn terminate_pid(pid: u32) -> std::io::Result<super::TerminateSignal> {
    let signal_pid = checked_signal_pid(pid)?;
    // SAFETY: `libc::kill` is async-signal-safe and has no in-process side
    // effects beyond delivering the signal to the target PID.
    let rc = unsafe { libc::kill(signal_pid, libc::SIGTERM) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(super::TerminateSignal::AlreadyGone);
        }
        return Err(err);
    }
    Ok(super::TerminateSignal::Delivered)
}

/// Unix has nothing to pin, so this is an empty token — see [`pin_process`].
#[derive(Debug)]
pub struct PinnedProcess;

impl Drop for PinnedProcess {
    /// Nothing to release. Declared anyway so "the pin is held until here" is
    /// expressible in the shared `daemon stop` flow on both platforms — an
    /// explicit `drop(pinned)` there is a real release on Windows and must still
    /// compile (and mean the same thing) here.
    fn drop(&mut self) {}
}

/// Pin `pid`'s identity — a **justified no-op on Unix**, kept as a seam so the
/// shared `daemon stop` flow does not grow a `cfg` branch (PRD #163 review).
///
/// POSIX offers no portable way to reserve a pid: the kernel frees it at reap
/// time regardless of what the reaper holds, and the one mechanism that would
/// (Linux `pidfd_open`) has no macOS counterpart. So Unix keeps exactly the
/// behaviour it always had — zero syscalls here, and the residual TOCTOU window
/// stays documented where it is accepted, at the top of
/// [`crate::build_version_handshake::terminate_daemon_graceful`].
///
/// Always `Ok(Some(…))`: reporting "already gone" would change the Unix control
/// flow, and reporting an error would refuse a stop that works today. The pid
/// guards inside [`terminate_pid`] / [`force_kill_pid`] remain the only gate.
pub fn pin_process(_pid: u32) -> std::io::Result<Option<PinnedProcess>> {
    Ok(Some(PinnedProcess))
}

/// Send `SIGKILL` to `pid` (the daemon-stop `--force` escalation). Same guards
/// as [`terminate_pid`].
pub fn force_kill_pid(pid: u32) -> std::io::Result<()> {
    let signal_pid = checked_signal_pid(pid)?;
    // SAFETY: same as `terminate_pid`; SIGKILL is uncatchable but the syscall
    // itself is async-signal-safe.
    let rc = unsafe { libc::kill(signal_pid, libc::SIGKILL) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Orphan watchdog (lifted from daemon.rs; test-gated, OFF in production).
// ---------------------------------------------------------------------------

/// The calling process's parent pid. Wraps `getppid(2)` (async-signal-safe,
/// infallible) so the single `unsafe` lives in one place.
pub fn current_ppid() -> i32 {
    // SAFETY: `getppid(2)` has no failure mode and no side effects.
    unsafe { libc::getppid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    // PRD #92 F1 followup (auditor #3) — defensive boundary check on the
    // `u32` PID → `libc::pid_t` PGID conversion used by the `killpg` call
    // sites. The pre-followup code did `pid as i32` directly, which silently
    // wrapped overflowing `u32` values into negative `i32`s (undefined
    // behavior for `killpg`) and never guarded against `pgid == 0` (which
    // `killpg(2)` documents as "signal every process in the *caller's* process
    // group" — for the daemon that would signal itself plus every attach
    // client). Real-world Linux PIDs are positive `i32` values, so this is
    // defense-in-depth; the unit test pins the boundary semantics.

    #[test]
    fn pid_to_pgid_accepts_positive_normal_pid() {
        assert_eq!(pid_to_pgid(1), Some(1));
        assert_eq!(pid_to_pgid(12345), Some(12345));
    }

    #[test]
    fn pid_to_pgid_rejects_zero_pid() {
        // `killpg(0, ...)` would signal the caller's own group — for the
        // daemon that's a fatal self-target. Must be filtered out.
        assert_eq!(pid_to_pgid(0), None);
    }

    #[test]
    fn pid_to_pgid_accepts_max_i32_pid() {
        let max = i32::MAX as u32;
        assert_eq!(pid_to_pgid(max), Some(i32::MAX));
    }

    #[test]
    fn pid_to_pgid_rejects_overflowing_u32_pid() {
        // Anything above i32::MAX would overflow the `as i32` cast in the
        // pre-followup code into a negative pgid. The guard converts those to
        // `None` so the kill path falls back to the single-PID `child.kill()`
        // path.
        assert_eq!(pid_to_pgid(i32::MAX as u32 + 1), None);
        assert_eq!(pid_to_pgid(u32::MAX), None);
    }

    // PRD #42 review N1 — boundary check on the daemon-stop `kill()` PID guard
    // (`checked_signal_pid`, lifted here from `build_version_handshake.rs`). It
    // is security-sensitive: a `peer_pid()` of 0 would make `kill(0, SIGTERM)`
    // broadcast to the caller's whole process group (taking down the parent
    // shell), and a `u32` PID above `i32::MAX` would wrap the `as i32` cast to a
    // negative value, turning `kill()` into a process-group wildcard. These
    // tests pin the guard semantics without signalling any real process.

    #[test]
    fn checked_signal_pid_accepts_positive_normal_pid() {
        assert_eq!(checked_signal_pid(1).unwrap(), 1);
        assert_eq!(checked_signal_pid(12345).unwrap(), 12345);
        assert_eq!(checked_signal_pid(i32::MAX as u32).unwrap(), i32::MAX);
    }

    #[test]
    fn checked_signal_pid_rejects_zero_pid() {
        // `kill(0, ...)` broadcasts to the caller's process group — must be
        // refused with `InvalidInput`, never signalled.
        let err = checked_signal_pid(0).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    /// PRD #163 review — the Unix half of the pid-pin seam must stay a pure no-op,
    /// including for the pids the by-signal guards reject. Anything else (an error,
    /// or an `Ok(None)` read as "already gone") would change what `daemon stop`
    /// does on Unix, and the whole point of the seam is that it does not.
    #[test]
    fn pinning_is_a_no_op_that_never_changes_the_unix_flow() {
        assert!(pin_process(std::process::id()).unwrap().is_some());
        assert!(pin_process(0).unwrap().is_some());
        assert!(pin_process(u32::MAX).unwrap().is_some());
    }

    #[test]
    fn checked_signal_pid_rejects_overflowing_u32_pid() {
        // Above i32::MAX the `as i32` cast would wrap negative → a `kill(-pgid)`
        // process-group wildcard. The guard must reject with `InvalidInput`.
        assert_eq!(
            checked_signal_pid(i32::MAX as u32 + 1).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            checked_signal_pid(u32::MAX).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
    /// Issue #862 — the regression guard for the whole fix. The bulk phase must
    /// NOT ask `ps` for the `args` column: that one column is what made it read
    /// `/proc/<pid>/cmdline` and `/proc/<pid>/environ` for every process on the
    /// machine, both of which take the target's `mmap_lock`, and it is why a
    /// sample went from ~49 ms to 19-20 s under a build storm. Re-adding it
    /// would reopen that silently — nothing else in the suite would notice,
    /// because the classification stays correct and only gets slow.
    ///
    /// Stated as "no `args`" rather than as an exact string so that adding a
    /// genuinely cheap column later does not fail this for the wrong reason.
    #[test]
    fn the_bulk_process_table_sample_never_asks_ps_for_the_argv_column() {
        let joined = PS_TABLE_ARGS.join(" ");
        assert!(
            !joined.contains("args"),
            "the bulk phase must not request the argv column (issue #862): {joined:?}"
        );
        assert!(
            !joined.contains("command") && !joined.contains("comm"),
            "nor any other column `ps` serves out of /proc/<pid>/cmdline: {joined:?}"
        );
        // And the argv phase, which is where a command line is allowed to come
        // from, must still ask for one — otherwise the cross-check would be
        // vetoing every candidate on an empty string.
        let argv_phase = PS_COMMAND_LINE_ARGS.join(" ");
        assert!(
            argv_phase.contains("args="),
            "the argv phase must request the argv column: {argv_phase:?}"
        );
        assert!(
            argv_phase.contains("-w -w"),
            "and must disable ps's width truncation, or a long command line is \
             silently cut and stops matching its measured shape: {argv_phase:?}"
        );
    }

    /// The argv phase's pid list is what `ps -p` is handed, so its formatting is
    /// worth pinning without spawning anything (issue #862).
    #[test]
    fn ps_pid_list_is_comma_separated_with_no_spaces() {
        assert_eq!(ps_pid_list(&[42]), "42");
        assert_eq!(ps_pid_list(&[42, 7, 1234]), "42,7,1234");
        assert_eq!(
            ps_pid_list(&[]),
            "",
            "an empty list never reaches `ps` — the sampler returns early — but the \
             formatting must not invent a stray separator"
        );
    }
}
