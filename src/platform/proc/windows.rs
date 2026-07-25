//! Windows process lifecycle (PRD #163 M3): Job-Object agent teardown +
//! `TerminateProcess` daemon-stop escalation.
//!
//! ## Agent teardown — Job Object, not `killpg`
//!
//! The Unix backend reaches an agent *and everything it spawned* with
//! `killpg(pid, …)`, which works because `portable-pty` `setsid`s the child into
//! its own process group. Windows has no such implicit grouping: a ConPTY child
//! is just a process, and terminating it leaves its descendants (the sub-shells
//! and tool processes an agent like Claude Code spawns) orphaned and running.
//!
//! The faithful analogue is a **Job Object**: [`AgentProcessGroup::adopt`]
//! creates one per agent at spawn and assigns the child to it, so every process
//! the child later creates is in the job too, and one `TerminateJobObject` call
//! reaps the whole tree atomically. That is the unconditional backstop —
//! the `killpg(SIGKILL)` equivalent — and it is what makes
//! [`force_kill_child_and_wait`] and the last phase of
//! [`terminate_child_with_grace_and_wait`] leak-free.
//!
//! ## The graceful window is explicitly best-effort (v1 documented difference)
//!
//! PRD #163 (and #42 before it) locks this in: *"Graceful agent shutdown is
//! best-effort on Windows (`CTRL_BREAK_EVENT` then hard `TerminateJobObject`) —
//! a faithful SIGTERM-trap grace window is not reproducible for console apps.
//! Documented difference."* Concretely, `GenerateConsoleCtrlEvent` is weaker than
//! `killpg(SIGTERM)` in three independent ways, and all three are expected to
//! bite in the daemon deployment:
//!
//! 1. **Console apps honour `CTRL_BREAK_EVENT` inconsistently** — many treat it
//!    as an immediate abort, some ignore it, and there is no equivalent of a
//!    SIGTERM handler contract that lets an agent flush state.
//! 2. **The caller must share a console with the target.** The daemon is spawned
//!    `DETACHED_PROCESS` (PRD #163 M2 — it must not die with the launching
//!    console), so it has *no* console, and its agents live in ConPTY consoles of
//!    their own. `GenerateConsoleCtrlEvent` from the daemon therefore usually
//!    fails outright rather than reaching the agent.
//! 3. **The target must be a process-group leader.** `portable-pty`'s ConPTY
//!    backend spawns with `EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT`
//!    and does *not* pass `CREATE_NEW_PROCESS_GROUP`, so the agent's pid is not a
//!    process-group id and the call is rejected as such.
//!
//! So the grace window in [`terminate_child_with_grace_and_wait`] is honest about
//! what it is: we *ask* (cheap, occasionally effective — e.g. a console-hosted
//! registry whose child did get its own group), then we poll `try_wait` for the
//! same grace budget the Unix path uses so a self-exiting agent is never
//! penalised, and then `TerminateJobObject` guarantees the teardown. A failed ask
//! is logged at debug, not warn: in the detached-daemon deployment it is the
//! *expected* outcome, and warning on every pane close would be pure noise.
//!
//! The stronger graceful path — writing `ETX` (Ctrl-C) into the ConPTY *input*
//! stream, which the pseudoconsole turns into a real console control event for
//! its attached processes — is deliberately out of scope here: it needs the
//! agent's PTY writer, which this teardown seam does not own, and PRD #163 lists
//! "faithful SIGTERM-trap graceful-shutdown parity" as out of scope for v1.
//!
//! ## Daemon stop — protocol first, `TerminateProcess` second
//!
//! There is no `SIGTERM` to send a detached daemon, and `CTRL_BREAK` cannot reach
//! it (point 2 above). So the graceful half is the existing, cross-platform
//! `KIND_SHUTDOWN`/ACK frame — declared via
//! [`super::GRACEFUL_STOP_DELIVERY`] and sent by the shared caller — while
//! [`terminate_pid`] only classifies liveness and [`force_kill_pid`] escalates
//! with `TerminateProcess`. No wire format branches on the platform.

use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, HANDLE};
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
    PROCESS_TERMINATE, TerminateProcess,
};

use super::{TerminateSignal, checked_target_pid};

/// Exit code stamped on a process/tree we tore down ourselves. Matches the PRD's
/// `TerminateProcess(OpenProcess(pid), 1)` and reads as "killed, did not exit on
/// its own" — the closest thing to the Unix `SIGKILL` wait-status.
const TEARDOWN_EXIT_CODE: u32 = 1;

/// `GetExitCodeProcess`'s "still running" sentinel (`STILL_ACTIVE` = 259 =
/// `STATUS_PENDING`). Spelled out locally rather than imported so the constant's
/// meaning is documented at the one place that compares against it.
///
/// Carries the well-known Win32 ambiguity: a process that *exits* with code 259
/// is indistinguishable from a running one. Harmless here — the only process this
/// is asked about is our own daemon, which never exits 259, and the caller's
/// authoritative liveness signal is the connect-poll, not this classification.
const STILL_ACTIVE: u32 = 259;

/// Owns a Win32 handle and closes it exactly once, on drop.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a successful `CreateJobObjectW` /
        // `OpenProcess` (constructors only ever wrap a non-null handle) and is
        // closed exactly once, here; nothing reads it afterwards.
        unsafe { CloseHandle(self.0) };
    }
}

/// The Windows counterpart of an agent's Unix process group: the **Job Object**
/// the agent and every process it spawns belong to.
///
/// Created and populated at spawn ([`Self::adopt`]) because job membership is
/// only inherited *forward* — assigning a process to a job later would leave the
/// descendants it had already spawned outside, so a retroactive
/// `TerminateJobObject` would miss exactly the processes it exists to reap.
/// Carried on [`crate::agent_pty::AgentPty`] / [`crate::agent_pty::RunningAgent`]
/// for the agent's lifetime and handed to the teardown helpers.
///
/// Deliberately **no** `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: dropping this guard
/// closes the handle without killing anything, matching Unix, where dropping a
/// `Child` leaves the agent running and only an explicit `killpg` tears it down.
/// (It also means a daemon that is itself hard-killed leaves its agents alive —
/// the same outcome as on Unix, where agents are in their own sessions.)
///
/// [`Default`] is the *unassigned* group — no job, so teardown degrades to a
/// single-process kill. It exists for one purpose: to leave behind as a
/// placeholder when the real group is moved out of a container (the spawn path's
/// `ChildGuard`). Never adopt a child into a defaulted group.
#[derive(Default)]
pub struct AgentProcessGroup {
    /// The job every process in the agent's tree belongs to. `None` when the
    /// grouping could not be established (see [`Self::adopt`]) — teardown then
    /// degrades to a single-process kill and says so.
    job: Option<OwnedHandle>,
}

// SAFETY: unlike a mutex — whose ownership is thread-affine (see
// `platform::lock::windows`) — a job-object handle has no thread affinity:
// `AssignProcessToJobObject`, `TerminateJobObject` and `CloseHandle` may be
// called from any thread and the kernel serializes access internally. The handle
// is owned exclusively by this struct (never duplicated, never handed out) and
// closed exactly once in `OwnedHandle::drop`, so moving the struct between
// threads — which the registry does, e.g. into `spawn_blocking` on the respawn
// path — and sharing it behind the registry's mutex are both sound.
unsafe impl Send for AgentProcessGroup {}
unsafe impl Sync for AgentProcessGroup {}

impl AgentProcessGroup {
    /// Adopt a freshly spawned PTY child into a new Job Object so its whole
    /// descendant tree can later be reaped in one call.
    ///
    /// Infallible by design: every failure below degrades to "no job", which the
    /// teardown helpers handle by killing the direct child only (and logging that
    /// descendants may leak). A Windows-specific job quirk must not be able to
    /// fail an agent spawn that is otherwise fine.
    ///
    /// Failure modes, all warned about:
    /// - `pid` is `None`/0 — `portable-pty`'s `Child::process_id()` is allowed to
    ///   return `None`; the [`checked_target_pid`] guard also refuses 0 (never a
    ///   real child, and a wildcard in the sibling Win32 calls).
    /// - `CreateJobObjectW` fails — an unnamed job with the default security
    ///   descriptor needs no privileges, so this is exceptional.
    /// - `OpenProcess` fails — the child died between spawn and here.
    /// - `AssignProcessToJobObject` fails — most plausibly `ERROR_ACCESS_DENIED`
    ///   from a parent job that forbids nesting. (Nested jobs have been supported
    ///   since Windows 8, i.e. on every host this project targets, but a
    ///   supervisor's job can still be configured to refuse.)
    ///
    /// PID-reuse safety: we resolve the pid through `OpenProcess` rather than
    /// taking the child's handle (`portable-pty`'s `Child` does not expose it),
    /// which is normally a TOCTOU window. It is closed here by the caller's
    /// ownership: the `Child` this pid came from is alive and holds an open
    /// handle to the process, and Windows does not recycle a PID while any handle
    /// to the process object remains — so the pid cannot name a different process
    /// than the one we just spawned.
    pub fn adopt(pid: Option<u32>) -> Self {
        let unassigned = Self { job: None };

        let Some(pid) = pid.and_then(|pid| checked_target_pid(pid).ok()) else {
            tracing::warn!(
                ?pid,
                "spawned agent has no usable pid; it will not be adopted into a job object, so \
                 its descendants may survive teardown"
            );
            return unassigned;
        };

        // SAFETY: NULL attributes = default security descriptor (the job is
        // unnamed, so it is reachable only through this handle); NULL name = an
        // anonymous job. Returns a null handle on failure, checked below.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            tracing::warn!(
                pid,
                error = %std::io::Error::last_os_error(),
                "CreateJobObjectW failed; agent descendants may survive teardown"
            );
            return unassigned;
        }
        let job = OwnedHandle(job);

        // PROCESS_SET_QUOTA + PROCESS_TERMINATE are exactly the rights
        // `AssignProcessToJobObject` documents as required.
        // SAFETY: `binherithandle: 0` (nothing inherits this handle); the pid is
        // guarded non-zero and pinned live by the caller's `Child` handle. A null
        // return means failure, checked below.
        let child = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if child.is_null() {
            tracing::warn!(
                pid,
                error = %std::io::Error::last_os_error(),
                "OpenProcess for job assignment failed; agent descendants may survive teardown"
            );
            return unassigned;
        }
        let child = OwnedHandle(child);

        // SAFETY: both handles are live and owned here; the call only records
        // membership in the kernel and retains neither handle — closing the
        // process handle right after (via `child`'s Drop) does not undo it.
        if unsafe { AssignProcessToJobObject(job.0, child.0) } == 0 {
            tracing::warn!(
                pid,
                error = %std::io::Error::last_os_error(),
                "AssignProcessToJobObject failed (a nesting-hostile parent job?); agent \
                 descendants may survive teardown"
            );
            return unassigned;
        }

        Self { job: Some(job) }
    }

    /// Reap the agent and every descendant in its job. Returns `false` when
    /// there is no job to terminate (so the caller falls back to a single-process
    /// kill) or the call failed.
    fn terminate_tree(&self, phase: &'static str) -> bool {
        let Some(job) = self.job.as_ref() else {
            return false;
        };
        // SAFETY: `job.0` is the live job handle this struct owns; the call is
        // idempotent (terminating an already-empty/terminated job succeeds) and
        // affects only processes assigned to this job.
        if unsafe { TerminateJobObject(job.0, TEARDOWN_EXIT_CODE) } == 0 {
            tracing::warn!(
                phase = %phase,
                error = %std::io::Error::last_os_error(),
                "TerminateJobObject failed; falling back to a single-process kill"
            );
            return false;
        }
        true
    }
}

/// Tear the agent's whole tree down, falling back to `portable-pty`'s
/// single-process kill when there is no job (see [`AgentProcessGroup::adopt`]).
fn reap_tree_or_fallback(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    group: &AgentProcessGroup,
    phase: &'static str,
) {
    if group.terminate_tree(phase) {
        return;
    }
    tracing::warn!(
        pid = ?child.process_id(),
        phase = %phase,
        "no job object for this agent — killing the direct child only; any descendants it \
         spawned will leak"
    );
    let _ = child.kill();
}

/// Best-effort `CTRL_BREAK_EVENT` to the child's process group — the closest
/// Windows has to `killpg(SIGTERM)`, and, per the module docs, expected to fail
/// in the detached-daemon deployment. Never fatal: the caller always follows up
/// with the `TerminateJobObject` backstop.
fn best_effort_ctrl_break(child_pid: Option<u32>, phase: &'static str) {
    let Some(pid) = child_pid.and_then(|pid| checked_target_pid(pid).ok()) else {
        // A missing/zero pid means we cannot name a process group at all;
        // `GenerateConsoleCtrlEvent(_, 0)` would broadcast to the caller's whole
        // console, so it must never be attempted (see `checked_target_pid`).
        tracing::debug!(
            ?child_pid,
            phase = %phase,
            "skipping the CTRL_BREAK grace signal: no usable process-group id"
        );
        return;
    };
    // SAFETY: no pointers involved; `pid` is guarded non-zero so this can only
    // ever address the child's own process group, never the caller's console.
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) } == 0 {
        // Debug, not warn: with a detached daemon and a ConPTY child that is not
        // a process-group leader, failing here is the documented normal case.
        tracing::debug!(
            pid,
            phase = %phase,
            error = %std::io::Error::last_os_error(),
            "CTRL_BREAK grace signal was not delivered (expected when the daemon shares no \
             console with the agent); relying on the TerminateJobObject backstop"
        );
    }
}

/// Forcefully terminate the child *and every descendant in its Job Object* and
/// reap it — the `killpg(SIGKILL)` equivalent. Callers should drop the
/// master/writer/reader handles before invoking this so any I/O blocked on the
/// PTY unblocks first.
pub fn force_kill_child_and_wait(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    group: &AgentProcessGroup,
) {
    reap_tree_or_fallback(child, group, "force-kill");
    let _ = child.wait();
}

/// Best-effort-graceful-then-force teardown used by the single-pane Ctrl+W path:
/// ask with `CTRL_BREAK_EVENT`, poll `try_wait` until the child exits or `grace`
/// elapses, then `TerminateJobObject` as the unconditional backstop and reap.
///
/// Structurally identical to the Unix `SIGTERM` → poll → `SIGKILL` sequence
/// (same 50 ms poll cadence, same "an early exiter is not penalised by the
/// deadline" property) — only the two signals differ, and the first one is
/// best-effort. See the module docs for exactly how it is weaker.
pub fn terminate_child_with_grace_and_wait(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    grace: Duration,
    group: &AgentProcessGroup,
) {
    // Phase 1: ask the agent's process group to stop.
    best_effort_ctrl_break(child.process_id(), "graceful-close-ctrl-break");

    // Phase 2: poll `try_wait` until the child exits or the grace elapses.
    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(_) => break,
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Phase 3: guaranteed backstop — reaches survivors *and* descendants
    // regardless of whether anything honoured the CTRL_BREAK.
    reap_tree_or_fallback(child, group, "graceful-close-terminate-job");
    let _ = child.wait();
}

/// The daemon-wide `shutdown_all_graceful` "SIGTERM phase" for one agent: ask
/// with `CTRL_BREAK_EVENT` and return without waiting (the caller polls every
/// agent together, then force-kills survivors via
/// [`force_kill_child_and_wait`], which is where the Job-Object backstop runs).
/// `phase` tags the tracing payload, as on Unix.
pub fn send_sigterm_to_child_group(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    phase: &'static str,
) {
    best_effort_ctrl_break(child.process_id(), phase);
}

// ---------------------------------------------------------------------------
// Daemon-stop termination by PID.
// ---------------------------------------------------------------------------

/// Open `pid` with the given access rights, mapping "there is no such process"
/// onto `Ok(None)` — the Windows `ESRCH`.
///
/// `OpenProcess` reports a dead/never-existed pid as `ERROR_INVALID_PARAMETER`
/// (there is no distinct "no such process" code); anything else — notably
/// `ERROR_ACCESS_DENIED` for a process owned by another user — is a genuine
/// failure and is surfaced, never swallowed into "already gone".
fn open_process(pid: u32, access: u32) -> std::io::Result<Option<OwnedHandle>> {
    let pid = checked_target_pid(pid)?;
    // SAFETY: no pointer arguments; `binherithandle: 0`. Returns a null handle on
    // failure, which is checked before the handle is wrapped/used.
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            return Ok(None);
        }
        return Err(err);
    }
    Ok(Some(OwnedHandle(handle)))
}

/// Classify the daemon PID for the *graceful* half of `daemon stop`.
///
/// Windows has no `SIGTERM`, so — unlike the Unix backend — this call delivers
/// nothing. Per [`super::GRACEFUL_STOP_DELIVERY`] the graceful request on this
/// platform is the shared `KIND_SHUTDOWN`/ACK frame, which
/// [`crate::build_version_handshake::terminate_daemon_graceful`] has already sent
/// by the time it gets here; what the shared escalation state machine still needs
/// from the platform is the Unix `ESRCH` distinction:
///
/// - [`TerminateSignal::AlreadyGone`] — the daemon is already gone
///   (`OpenProcess` → `ERROR_INVALID_PARAMETER`, or an open handle whose exit
///   code is no longer `STILL_ACTIVE`), so there is nothing to wait for and the
///   caller reports `Stopped` immediately.
/// - [`TerminateSignal::Delivered`] — the daemon is still alive, so the caller
///   polls for it to disappear and escalates to [`force_kill_pid`] if it does
///   not. This is also the answer when the shutdown frame was never acknowledged,
///   which is deliberate: a wedged daemon must reach the force escalation, and it
///   is the same thing Unix does with a `SIGTERM` the daemon ignores.
///
/// A pid of 0 is refused up front (`InvalidInput`) rather than opened — see
/// [`checked_target_pid`].
pub fn terminate_pid(pid: u32) -> std::io::Result<TerminateSignal> {
    let Some(handle) = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)? else {
        return Ok(TerminateSignal::AlreadyGone);
    };

    let mut code: u32 = 0;
    // SAFETY: `handle.0` is a live process handle opened with
    // PROCESS_QUERY_LIMITED_INFORMATION (which is sufficient for
    // `GetExitCodeProcess`); `code` is a stack `u32` that outlives the call and is
    // the only thing written.
    if unsafe { GetExitCodeProcess(handle.0, &mut code) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if code == STILL_ACTIVE {
        Ok(TerminateSignal::Delivered)
    } else {
        // A process object outlives the process itself while any handle to it is
        // open, so "openable" is not "alive". Its exit code says which.
        Ok(TerminateSignal::AlreadyGone)
    }
}

/// Daemon-stop force escalation by PID: `TerminateProcess`, the Windows
/// `SIGKILL`. Same pid guard as [`terminate_pid`].
///
/// Mirrors the Unix contract exactly, including its rough edge: a target that
/// vanished between the grace window and this call surfaces as an error
/// (`ERROR_INVALID_PARAMETER`, the `ESRCH` analogue) rather than a silent
/// success, because that is what `force_kill_pid` does on Unix and the shared
/// caller maps both onto the same `TerminateFailed`.
pub fn force_kill_pid(pid: u32) -> std::io::Result<()> {
    let Some(handle) = open_process(pid, PROCESS_TERMINATE)? else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no process with pid {pid}; it exited before the force escalation"),
        ));
    };
    // SAFETY: `handle.0` is a live process handle opened with PROCESS_TERMINATE;
    // the call takes no pointers and cannot affect any other process.
    if unsafe { TerminateProcess(handle.0, TEARDOWN_EXIT_CODE) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Orphan watchdog (test-gated, OFF in production).
// ---------------------------------------------------------------------------

/// The orphan watchdog has no Windows analogue (`getppid`/pid-1-reparent is
/// POSIX) and is test-only / OFF in production. Returns a sentinel so
/// `should_exit_orphaned` never triggers.
pub fn current_ppid() -> i32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    // These run on the `windows-latest` CI job (`cargo nextest run`), which is
    // where they earn their keep: they exercise the real Win32 calls against
    // real processes, which a Linux `cargo check --target` cannot reach. The
    // pure-data pid guard they lean on is tested on every platform in
    // `super::super::tests`.

    /// A long-running helper: `cmd.exe` running `ping`, i.e. a child *and* a
    /// grandchild. Arguments are passed individually rather than as one quoted
    /// string (with `/C`, `cmd` keeps the quotes when the string contains
    /// redirection characters, and would then look for a program with that whole
    /// name), and stdio is discarded so the wait is not output-dependent.
    fn spawn_helper_tree() -> std::process::Child {
        std::process::Command::new("cmd")
            .args(["/C", "ping", "-n", "30", "127.0.0.1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the helper process tree")
    }

    /// The escalation state machine's platform input: a live process reads as
    /// `Delivered` (so the caller polls and escalates) and a process that has
    /// exited reads as `AlreadyGone` (so the caller short-circuits to `Stopped`).
    ///
    /// The second assertion covers the arm a naive "`OpenProcess` succeeded ⇒
    /// alive" implementation gets wrong: `child` is still in scope, so its open
    /// handle keeps the *process object* — and its pid — alive after the process
    /// itself is gone. Only the exit code distinguishes the two, and keeping the
    /// handle is also what makes the assertion deterministic (no pid can be
    /// recycled underneath it).
    #[test]
    fn terminate_pid_reads_live_as_delivered_and_exited_as_already_gone() {
        let mut child = spawn_helper_tree();
        let pid = child.id();

        assert_eq!(terminate_pid(pid).unwrap(), TerminateSignal::Delivered);

        // Take it down through the very call `daemon stop --force` escalates to.
        force_kill_pid(pid).expect("TerminateProcess on a live child");
        child.wait().expect("wait for the terminated helper");

        assert_eq!(terminate_pid(pid).unwrap(), TerminateSignal::AlreadyGone);
    }

    /// The other `AlreadyGone` arm: `OpenProcess` answering
    /// `ERROR_INVALID_PARAMETER` for a pid nothing holds — the `ESRCH` analogue —
    /// must not surface as an error. `u32::MAX` is a safe stand-in: Windows hands
    /// out PIDs as small multiples of four, so nothing can be holding it.
    #[test]
    fn terminate_pid_reads_a_nonexistent_pid_as_already_gone() {
        assert_eq!(
            terminate_pid(u32::MAX).unwrap(),
            TerminateSignal::AlreadyGone
        );
    }

    /// pid 0 must never reach `OpenProcess` (it names the System Idle Process) —
    /// the guard turns it into `InvalidInput` on both by-PID entry points.
    #[test]
    fn by_pid_helpers_refuse_pid_zero() {
        assert_eq!(
            terminate_pid(0).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            force_kill_pid(0).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    /// A pid that cannot be adopted degrades to "no job" instead of panicking or
    /// failing the spawn — the property that keeps a Windows job quirk from
    /// breaking an otherwise-healthy agent spawn.
    #[test]
    fn adopt_degrades_to_no_job_for_an_unusable_pid() {
        assert!(AgentProcessGroup::adopt(None).job.is_none());
        assert!(AgentProcessGroup::adopt(Some(0)).job.is_none());
    }

    /// The real thing, and the reason a Job Object replaced the single-process
    /// kill: adopting the child puts *it and its descendants* (here `cmd.exe` plus
    /// the `ping.exe` it spawns — exactly what `killpg` reaches on Unix and
    /// `TerminateProcess` on the direct child does not) into one job, and one
    /// `TerminateJobObject` tears the whole tree down.
    #[test]
    fn adopted_child_and_its_descendants_are_reaped_by_the_job() {
        let mut child = spawn_helper_tree();
        let pid = child.id();

        let group = AgentProcessGroup::adopt(Some(pid));
        assert!(
            group.job.is_some(),
            "a freshly spawned child must be adoptable into a job object"
        );

        assert!(group.terminate_tree("test"));
        // The child (and with it the grandchild in the same job) is gone; the
        // still-open handle in `child` keeps the assertion pid-stable.
        child.wait().expect("wait for the reaped helper");
        assert_eq!(terminate_pid(pid).unwrap(), TerminateSignal::AlreadyGone);
    }

    /// The grace signal never throws: with no console to signal into (the daemon
    /// is `DETACHED_PROCESS`) the call simply fails and is logged, and a
    /// missing/zero pid is skipped rather than broadcast to the caller's console.
    /// Callers rely on that — the `TerminateJobObject` backstop runs afterwards
    /// either way.
    #[test]
    fn ctrl_break_grace_is_never_fatal() {
        let mut child = spawn_helper_tree();
        best_effort_ctrl_break(Some(child.id()), "test");
        best_effort_ctrl_break(None, "test");
        best_effort_ctrl_break(Some(0), "test");
        force_kill_pid(child.id()).expect("clean up the helper");
        child.wait().expect("wait for the terminated helper");
    }
}
