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
//! Two consequences of "job membership is inherited forward only" are load-bearing
//! and easy to get wrong, so both are pinned by tests and spelled out where they
//! live:
//!
//! - The job must be created and joined **at spawn**, not at teardown — see
//!   [`AgentProcessGroup::adopt`], which also documents the residual
//!   post-`CreateProcess` adoption race that `portable-pty`'s ConPTY spawn makes
//!   unavoidable in v1.
//! - The **job handle**, never the direct child's exit status, decides the final
//!   reap. A ConPTY child that exits while its own descendants keep running is
//!   ordinary (a shell wrapper whose tool process outlives it), so a teardown that
//!   short-circuits on "the direct child is gone" leaks exactly the tree the job
//!   exists to reap — see phase 3 of [`terminate_child_with_grace_and_wait`].
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
    ///
    /// # Known v1 limitation: the post-spawn adoption race
    ///
    /// Job membership is inherited *forward* only, and this call necessarily runs
    /// **after** `CreateProcessW` has already returned a running child. Any
    /// descendant the child manages to spawn in that window — between
    /// `CreateProcessW` returning inside `portable-pty` and
    /// `AssignProcessToJobObject` here — is **not** in the job, and neither are
    /// that descendant's own descendants. `TerminateJobObject` cannot reap what
    /// never joined, so such a process survives agent teardown. It is the one
    /// descendant-leak path the Job Object does not close.
    ///
    /// The window is closable in principle and **not** closable in v1, for a
    /// concrete reason rather than an ergonomic one. Both Win32 mechanisms need
    /// control over the `CreateProcessW` call itself, which
    /// `portable-pty`'s ConPTY backend does not delegate
    /// (`portable-pty` 0.8.1, `src/win/psuedocon.rs`):
    ///
    /// - **`CREATE_SUSPENDED` + assign + `ResumeThread`** — the creation flags are
    ///   hard-coded to `EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT`
    ///   with no way to add a flag, and the primary-thread handle needed to resume
    ///   is closed inside `spawn_command` before it returns.
    /// - **`PROC_THREAD_ATTRIBUTE_JOB_LIST`** (the clean fix: the child is *born*
    ///   in the job) — the `STARTUPINFOEX` attribute list is built internally with
    ///   room for exactly one attribute, the pseudoconsole handle, and is never
    ///   exposed.
    ///
    /// An *outer* job created before the spawn, which the child would inherit,
    /// does not help either: to be inherited it would have to contain the daemon
    /// itself, so terminating it would kill the daemon and every other agent —
    /// and nesting a per-agent job inside it still leaves the escaped descendant
    /// in the outer job only, i.e. outside the job we terminate.
    ///
    /// Closing this properly therefore means replacing `portable-pty`'s ConPTY
    /// spawn (vendoring it, or upstreaming a "spawn into these jobs" option),
    /// which PRD #163 scopes out of v1. Practically the exposure is small — the
    /// window is the microseconds between `spawn_command` returning and the next
    /// statement in [`crate::agent_pty::spawn`], while a real agent's first child
    /// process is milliseconds-to-seconds into its startup — and it degrades the
    /// same way the PRD's other Windows teardown gaps do: best-effort, logged, with
    /// `TerminateJobObject` still reaping everything that *did* join the job.
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
///
/// One deliberate structural difference from Unix, and the reason phase 3 is not
/// simply skipped when the child exits inside the grace window (PRD #163 review):
/// **the Job Object, not the direct child, decides when teardown is done.** On
/// Unix an exited child means the process group is addressable but usually empty,
/// and `killpg(SIGKILL)` on a group whose leader is gone is the same "reach the
/// survivors" call phase 3 makes here — so returning early is harmless there. On
/// Windows the direct child exiting says *nothing* about its job: a ConPTY child
/// that spawns a tool process and exits (a shell wrapper, a launcher, `cmd /c`)
/// leaves that tool process running *inside the job*, and nothing else will ever
/// reap it. So an exited child still gets a `TerminateJobObject`.
pub fn terminate_child_with_grace_and_wait(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    grace: Duration,
    group: &AgentProcessGroup,
) {
    // Phase 1: ask the agent's process group to stop.
    best_effort_ctrl_break(child.process_id(), "graceful-close-ctrl-break");

    // Phase 2: poll `try_wait` until the child exits or the grace elapses.
    let deadline = std::time::Instant::now() + grace;
    let mut child_exited = false;
    while std::time::Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => {
                child_exited = true;
                break;
            }
            Ok(None) => {}
            Err(_) => break,
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Phase 3: guaranteed backstop — reaches survivors *and* descendants
    // regardless of whether anything honoured the CTRL_BREAK.
    if child_exited {
        // `try_wait` already reaped the direct child, so there is no process for
        // the single-process fallback to kill and no status left to `wait` for —
        // but its descendants may still be in the job, so the job is still
        // terminated. `terminate_tree` is a no-op-safe call on a job whose
        // processes have all exited.
        group.terminate_tree("graceful-close-terminate-job-after-child-exit");
        return;
    }
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

/// A live claim on the *identity* of a pid: while this is held, Windows cannot
/// hand `pid` to a different process (PRD #163 review — Greptile P1).
///
/// The mechanism is the one Win32 guarantee that makes by-pid termination safe: a
/// process id stays reserved as long as any handle to the process **object** is
/// open, even after the process itself exits. So one handle held across the whole
/// `daemon stop` escalation — graceful ask, 5 s poll for the daemon to die, then
/// `TerminateProcess` — makes it impossible for that final call to reach anything
/// but the daemon we probed. Without it the recycling window is not a theoretical
/// sliver but the *expected* path: we deliberately wait for the target to exit and
/// then name its pid.
///
/// It carries no access rights worth having on purpose —
/// `PROCESS_QUERY_LIMITED_INFORMATION` is the cheapest right that keeps the object
/// alive, and the identity guarantee comes from holding *a* handle, not from which
/// rights it holds. [`terminate_pid`] / [`force_kill_pid`] still open their own
/// handles with the rights they need, and while a pin is held those opens
/// provably resolve to the same process.
///
/// This is the anchor the agent teardown path already has for free (its caller
/// holds a `std::process::Child`, whose handle pins the pid the same way) and that
/// `daemon stop`, which only ever had a number, was missing.
pub struct PinnedProcess(#[allow(dead_code)] OwnedHandle);

impl std::fmt::Debug for PinnedProcess {
    /// Deliberately opaque: the raw handle value is noise in a log line, and the
    /// only thing worth knowing about a pin is that one is held.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PinnedProcess")
    }
}

/// Pin `pid`'s identity for as long as the returned value is held — see
/// [`PinnedProcess`].
///
/// `Ok(None)` means the process was already gone (the `ESRCH` analogue), so there
/// is nothing to pin *and* nothing to terminate. `Err` means the pin genuinely
/// failed (e.g. `ERROR_ACCESS_DENIED`); callers must treat that as "do not
/// escalate by pid" rather than as "already gone", since an unpinned pid is
/// exactly what makes escalation unsafe.
pub fn pin_process(pid: u32) -> std::io::Result<Option<PinnedProcess>> {
    Ok(open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?.map(PinnedProcess))
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
// Foreground process-group query (PRD #370 M1).
// ---------------------------------------------------------------------------

/// Always `None` on Windows: `portable_pty::MasterPty::process_group_leader`
/// is `#[cfg(unix)]`-only on the trait itself (there is no Windows console
/// analogue of a pty foreground process group — Windows pipes/conpty have no
/// equivalent of `tcgetpgrp`), so the method does not exist to call here at
/// all. See `unix.rs` for the real implementation. Callers must treat `None`
/// as "no signal available" and fall back to whatever status they already
/// have, never as "not busy".
pub fn foreground_pgid(_master: &dyn portable_pty::MasterPty) -> Option<i32> {
    None
}

// ---------------------------------------------------------------------------
// Process-table sample (PRD #386 M1).
// ---------------------------------------------------------------------------

/// Always `None` on Windows: PRD #386 scopes native Windows process walking
/// out, exactly as [`foreground_pgid`] does today. There is also nothing for
/// the discriminator to compare — the whole signal rests on POSIX sessions
/// (`getsid(2)`), which Windows has no analogue of, so a `Toolhelp32Snapshot`
/// walk would enumerate a tree it could not classify.
///
/// Callers must treat `None` as "no signal available" and leave whatever status
/// they already have alone, never read it as "not busy". The cross-platform
/// half — [`super::ProcessInfo`], [`super::descendants`] and
/// [`super::descendant_shell_activity`] — still compiles and is still tested
/// here, so a future Windows backend only has to supply this one function.
pub fn process_table() -> Option<Vec<super::ProcessInfo>> {
    None
}

/// The async twin of [`process_table`], so the daemon's poll loop needs no
/// `cfg` branch of its own (issue #429). Unconditionally `None` for the same
/// reason, and it never awaits anything: there is no subprocess to bound, so the
/// caller's timeout simply never fires here.
pub async fn process_table_async() -> Option<Vec<super::ProcessInfo>> {
    None
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
    /// the guard turns it into `InvalidInput` on every by-PID entry point.
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
        assert_eq!(
            pin_process(0).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    /// PRD #163 review (Greptile P1) — the property that makes `daemon stop`'s
    /// by-pid escalation safe: a held [`PinnedProcess`] keeps the *process object*
    /// (and therefore the pid) reserved after the process itself has exited **and**
    /// after every other handle to it is closed. That is the whole window the old
    /// code left open — it waits for the daemon to die, then names its pid.
    ///
    /// The helper's own `Child` handle is dropped before the assertion on purpose:
    /// with it alive the pid would stay resolvable regardless, and the test would
    /// pass without the pin doing anything.
    #[test]
    fn a_pin_reserves_the_pid_after_the_process_exits() {
        let mut child = spawn_helper_tree();
        let pid = child.id();

        let pin = pin_process(pid)
            .expect("pin a live process")
            .expect("a live process must be pinnable");

        force_kill_pid(pid).expect("TerminateProcess on the live helper");
        child.wait().expect("wait for the terminated helper");
        drop(child);

        assert!(
            open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)
                .expect("opening a pinned pid must not error")
                .is_some(),
            "the pin must keep the pid resolvable, so nothing else can be handed it"
        );
        assert_eq!(
            terminate_pid(pid).unwrap(),
            TerminateSignal::AlreadyGone,
            "and the pinned object must still report the real exit, not STILL_ACTIVE"
        );

        drop(pin);
    }

    /// A pid nothing holds cannot be pinned, and that is `Ok(None)` — the `ESRCH`
    /// analogue — never an error. `daemon stop` reads it as "already gone" and
    /// declines to escalate, which is the safe answer.
    #[test]
    fn pinning_a_nonexistent_pid_reports_already_gone() {
        assert!(pin_process(u32::MAX).unwrap().is_none());
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

    /// Assign an **additional** live process to `group`'s job. Test-only, and the
    /// point of it: it reproduces — deterministically, with a pid we know — the
    /// state a real agent reaches by inheritance, where the job holds the direct
    /// child *and* the tool processes it spawned. Building that with a genuine
    /// grandchild (`cmd /C start /B …`) would leave us without the survivor's pid
    /// to assert on, and job membership, not parentage, is what teardown acts on.
    fn assign_extra_process_to_job(group: &AgentProcessGroup, pid: u32) -> bool {
        let job = group.job.as_ref().expect("the group must own a job");
        // SAFETY: no pointer arguments; `binherithandle: 0`; the pid names a live
        // process the caller still holds a `std::process::Child` handle for, so it
        // cannot have been recycled. A null return is failure, checked below.
        let handle = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let handle = OwnedHandle(handle);
        // SAFETY: both handles are live and owned here (the job by `group`, the
        // process by `handle`); the call only records membership in the kernel and
        // retains neither handle.
        unsafe { AssignProcessToJobObject(job.0, handle.0) != 0 }
    }

    /// PRD #163 review — the descendant leak this fix closes: when the agent's
    /// **direct child exits inside the CTRL_BREAK grace window** but processes it
    /// spawned are still running, teardown must still `TerminateJobObject`. The job
    /// handle, not the direct child's exit, decides when the tree is gone.
    ///
    /// Before the fix, phase 2's `try_wait` returning `Some` short-circuited the
    /// whole backstop, so the survivor below lived on with nothing left to reap it
    /// — the common shape on Windows, where a launcher/`cmd` wrapper exits the
    /// moment it has started the real tool.
    ///
    /// The survivor is a 30-second `ping`, so the 10-second bounded wait is a real
    /// assertion: it can only exit that early because the job terminated it.
    #[test]
    fn teardown_reaps_the_job_when_the_direct_child_exits_during_grace() {
        // Stands in for a tool process the agent spawned and left behind. A bare
        // `ping` rather than [`spawn_helper_tree`]'s `cmd`-wrapped one: only the pid
        // we hand to the job below is guaranteed to be terminated, so a single
        // process leaves nothing running past the test either way.
        let mut survivor = std::process::Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the stand-in descendant");
        let survivor_pid = survivor.id();

        // The direct child: exits immediately, like a launcher that has handed off.
        let doomed = std::process::Command::new("cmd")
            .args(["/C", "exit", "0"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the short-lived direct child");

        let group = AgentProcessGroup::adopt(Some(doomed.id()));
        assert!(
            group.job.is_some(),
            "a freshly spawned child must be adoptable into a job object"
        );
        assert!(
            assign_extra_process_to_job(&group, survivor_pid),
            "the stand-in descendant must join the agent's job"
        );

        let mut child: Box<dyn portable_pty::Child + Send + Sync> =
            Box::new(crate::platform::proc::test_child::StdChild(doomed));
        terminate_child_with_grace_and_wait(&mut child, Duration::from_secs(3), &group);

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut reaped = false;
        while std::time::Instant::now() < deadline {
            if survivor
                .try_wait()
                .expect("try_wait on the stand-in descendant")
                .is_some()
            {
                reaped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !reaped {
            // Don't leave a 30s `ping` behind when the assertion below fails.
            let _ = survivor.kill();
        }
        // Unconditional: reaps the handle either way (a no-op returning the cached
        // status when the poll above already saw the exit).
        let _ = survivor.wait();
        assert!(
            reaped,
            "TerminateJobObject must still run when the direct child exits during the grace \
             window, otherwise every descendant it spawned leaks"
        );
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
