//! Process lifecycle: agent teardown + daemon-stop termination + orphan
//! watchdog (PRD #42 M1, lifted from `agent_pty.rs`, `build_version_handshake.rs`,
//! and `daemon.rs`).
//!
//! Unix uses POSIX signals: `killpg(SIGTERM/SIGKILL)` to tear down an agent's
//! whole process group, `kill(SIGTERM/SIGKILL)` to stop the daemon by PID, and
//! `getppid` for the (test-only) orphan watchdog. Windows (PRD #163 M3) has no
//! signal analogue and splits the two jobs apart:
//!
//! - **Agent teardown** — each agent is adopted into a **Job Object** at spawn
//!   ([`AgentProcessGroup`]) and the whole descendant tree is reaped with
//!   `TerminateJobObject`, the unconditional backstop that stands in for
//!   `killpg(SIGKILL)`. The SIGTERM grace window maps to a best-effort
//!   `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, …)` — **explicitly best-effort**
//!   per the PRD, see the windows backend for why.
//! - **Daemon stop** — the graceful half is not a signal at all but the existing,
//!   cross-platform `KIND_SHUTDOWN`/ACK protocol frame; only the force escalation
//!   is platform code (`TerminateProcess`). Which of the two a platform uses is
//!   declared by [`GRACEFUL_STOP_DELIVERY`] so the shared caller
//!   ([`crate::build_version_handshake::terminate_daemon_graceful`]) never
//!   branches on `cfg` and the wire format stays identical everywhere. Because
//!   that escalation names a *pid* long after the target was probed,
//!   [`pin_process`] lets the caller hold the target's identity across the whole
//!   sequence — a real handle on Windows, a documented no-op on Unix.
//!
//! Note: peer-credential PID *discovery* (`SO_PEERCRED`) lives in
//! [`crate::platform::peercred`]; this module only owns kill/teardown.
//!
//! PRD #386 M1/M2 adds a second, read-only concern alongside teardown: the
//! **descendant-process scan** the shell-activity signal is built on. Its
//! cross-platform half — [`ProcessInfo`], [`descendants`] and the structural
//! [`descendant_shell_activity`] discriminator — lives in `scan.rs` and
//! compiles everywhere; only the act of sampling the machine is platform code
//! (`ps` on Unix, an unconditional `None` on Windows, matching
//! [`foreground_pgid`]'s existing contract). It comes in two flavours:
//! [`process_table`], which blocks the calling thread, and
//! [`process_table_async`], which the daemon's poll loop uses so a wedged `ps`
//! cannot stall a Tokio worker and so a [`tokio::time::timeout`] can genuinely
//! kill it (issue #429).

mod scan;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

pub use scan::{
    AgentServer, CLAUDE_BASH_TOOL_SHAPE, CommandLine, CommandSessions, CommandWrapper,
    MEASURED_SHELL_TOOL_SHAPES, OPENCODE_PROCESS_DAEMON, ProcessInfo, ShellToolShape,
    command_line_targets, command_sessions_in_pane, descendant_shell_activity, descendants,
    detached_descendants, fill_command_lines, is_opencode_private_server_command,
    is_opencode_process_daemon_command, qualifying_command_sessions,
    qualifying_command_sessions_in_pane, qualifying_command_sessions_with_server,
    shell_tool_candidates,
};

/// What a single-agent teardown learned about the direct child's exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildTermination {
    /// `try_wait` collected an exit status.
    Reaped,
    /// The child still had not reported an exit when the reap deadline expired.
    /// The kill flag says whether the forced-termination mechanism reported an
    /// error; `true` is not proof that the child has exited or will exit.
    Unreaped { kill_reported_ok: bool },
}

/// Poll a direct child's exit without ever entering its potentially unbounded
/// blocking `wait`. Errors are not evidence of an exit, so they keep polling
/// until the same deadline as a child that reports `None`.
fn reap_child_with_deadline(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    deadline: std::time::Duration,
) -> bool {
    let deadline = std::time::Instant::now() + deadline;
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Result of delivering the daemon-stop graceful signal to a PID via
/// [`terminate_pid`].
///
/// The distinction matters to
/// [`crate::build_version_handshake::terminate_daemon_graceful`]: `Delivered`
/// means the signal reached a live process that may still be shutting down, so
/// the caller must poll for it to disappear (and possibly escalate to
/// [`force_kill_pid`]); `AlreadyGone` means the target PID no longer existed
/// (`ESRCH` on Unix, `ERROR_INVALID_PARAMETER` from `OpenProcess` — or an
/// already-signalled exit code — on Windows), so there is nothing to wait for
/// and the caller can report `Stopped` immediately. This mirrors `main`, where an
/// `ESRCH` from the `SIGTERM` `kill(2)` short-circuited straight to
/// `Ok(TerminateOutcome::Stopped)` rather than entering the poll/escalate loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminateSignal {
    /// The signal was delivered to a live process; it may still be dying, so
    /// the caller must poll for the process to disappear.
    Delivered,
    /// The target process was already gone when signalled (`ESRCH`) — an
    /// already-gone success that short-circuits the poll/escalate loop.
    AlreadyGone,
}

/// PRD #163 M3 — how the **graceful** half of `daemon stop` reaches the daemon
/// on this platform. The force half is always platform code
/// ([`force_kill_pid`]: `SIGKILL` / `TerminateProcess`); this enum is only about
/// the polite first ask.
///
/// It exists so [`crate::build_version_handshake::terminate_daemon_graceful`] —
/// which owns the shared *graceful → poll → force* escalation state machine, and
/// is the single path both `daemon stop` and the build-mismatch prompt go
/// through — can pick the right first step without a `cfg` branch of its own and
/// without any per-platform wire format. The platform decision lives here, at
/// the platform seam; the protocol stays identical on every platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GracefulStopDelivery {
    /// Unix: an out-of-band `SIGTERM` delivered by [`terminate_pid`].
    ///
    /// Load-bearing property: **zero protocol bytes are exchanged**, so
    /// `daemon stop` works against *any* daemon version — including the
    /// v0.24.x daemon that predates `KIND_SHUTDOWN` and motivated PRD #103.
    Signal,
    /// Windows: there is no `SIGTERM`. The graceful request is the existing
    /// `KIND_SHUTDOWN`/ACK frame (identical wire on every platform), sent by
    /// the shared caller *before* [`terminate_pid`], which then only classifies
    /// the target as still-alive vs already-gone. `TerminateProcess` remains the
    /// escalation when the daemon does not go away within the grace window.
    ///
    /// The trade-off this makes explicit: unlike [`Signal`](Self::Signal) the
    /// graceful step now needs a daemon that speaks `PROTOCOL_VERSION` ≥ 2.
    /// That is free on Windows — the Windows daemon is unblocked *by* #163, so
    /// no older Windows daemon exists — and a daemon that does not answer just
    /// falls through to the force escalation, exactly like a Unix daemon that
    /// ignores `SIGTERM`.
    ShutdownProtocol,
}

/// Unix delivers the graceful stop as a signal — see
/// [`GracefulStopDelivery::Signal`] for the zero-protocol-bytes property this
/// pins.
#[cfg(unix)]
pub const GRACEFUL_STOP_DELIVERY: GracefulStopDelivery = GracefulStopDelivery::Signal;

/// Windows has no `SIGTERM`, so the graceful stop rides the shared
/// `KIND_SHUTDOWN`/ACK protocol — see [`GracefulStopDelivery::ShutdownProtocol`].
#[cfg(windows)]
pub const GRACEFUL_STOP_DELIVERY: GracefulStopDelivery = GracefulStopDelivery::ShutdownProtocol;

/// Guard a `u32` PID before naming it in a Win32 lifecycle call
/// (`OpenProcess` for the daemon-stop path, `GenerateConsoleCtrlEvent`'s
/// process-group id for the agent grace window). The Windows counterpart of the
/// Unix `checked_signal_pid` / `pid_to_pgid` guards, and it exists for the same
/// reason: **0 is not "no process", it is a wildcard.**
///
/// - `GenerateConsoleCtrlEvent(_, 0)` is documented as "signal every process
///   that shares the caller's console" — the exact `killpg(0, …)` hazard, which
///   for a console-hosted registry would mean signalling the TUI itself.
/// - `OpenProcess(_, _, 0)` names the System Idle Process, never a daemon.
///
/// Unlike the Unix guard there is no overflow arm: a Windows PID *is* a `u32`
/// (`DWORD`), so no value other than 0 changes the call's meaning.
///
/// Compiled on every platform — it is pure data, so the rule stays unit-testable
/// on Linux CI where the `#[cfg(windows)]` callers are absent (the same shape
/// [`crate::platform::lock::spawn_mutex_name`] uses).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn checked_target_pid(pid: u32) -> std::io::Result<u32> {
    if pid == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "pid is 0; refusing the Win32 call (0 is a wildcard: it broadcasts to every process \
             sharing the console / names the System Idle Process)",
        ));
    }
    Ok(pid)
}

#[cfg(target_os = "linux")]
pub use unix::LinuxProcView;
#[cfg(unix)]
pub use unix::{
    AgentProcessGroup, PinnedProcess, current_ppid, executable_of, force_kill_child_and_wait,
    force_kill_child_group, force_kill_pid, foreground_pgid, parent_pid_of, pin_process,
    process_start_of, process_table, process_table_async, send_sigterm_to_child_group,
    terminate_child_with_grace_and_wait, terminate_pid,
};
#[cfg(windows)]
pub use windows::{
    AgentProcessGroup, PinnedProcess, current_ppid, executable_of, force_kill_child_and_wait,
    force_kill_child_group, force_kill_pid, foreground_pgid, parent_pid_of, pin_process,
    process_start_of, process_table, process_table_async, send_sigterm_to_child_group,
    terminate_child_with_grace_and_wait, terminate_pid,
};

/// PRD #1666 part 3, round 5-wake (plan §1.3): the session process `pid`
/// belongs to (`getsid(2)`), or `None` when it cannot be read (no such
/// process, or a platform without sessions).
#[cfg(unix)]
pub fn session_of(pid: u32) -> Option<u32> {
    let pid = i32::try_from(pid).ok()?;
    // SAFETY: `getsid(2)` has no side effects and only reads the process table.
    let sid = unsafe { libc::getsid(pid) };
    u32::try_from(sid).ok()
}
#[cfg(windows)]
pub fn session_of(_pid: u32) -> Option<u32> {
    None
}

/// PRD #1790 stage 3 (review R1 / audit B1): what the OS says of process
/// `pid`, keeping "gone" apart from "could not look". [`process_start_of`]
/// answers `None` for both, so a caller deciding that a process is gone must
/// ask this instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessProbe {
    /// The kernel says no process holds the pid (`ESRCH`).
    Absent,
    /// Audit F4b/S3: the process holding the pid has exited and only waits
    /// to be reaped (Linux: state `Z` or `X`). It runs nothing and writes
    /// nothing, so a stop proof and a later "is it gone" check both read it
    /// as gone ([`can_run`]).
    Exited,
    /// A process holds the pid, and started at this time
    /// ([`process_start_of`]'s unit).
    Started(u64),
    /// Neither could be established: the start time could not be read and
    /// the kernel did not say the pid is free (a process another user owns,
    /// a restricted or missing `/proc`, a refused `proc_pidinfo`, a platform
    /// without start times).
    Unknown,
}

/// Probe process `pid` ([`ProcessProbe`]). The start time first; when it
/// cannot be read, `kill(pid, 0)`, whose `ESRCH` is the one answer taken as
/// absence (`EPERM` means a process holds the pid).
pub fn probe_process(pid: u32) -> ProcessProbe {
    match probe_process_with(pid, process_start_of, pid_is_free) {
        ProcessProbe::Started(_) if is_zombie(pid) => ProcessProbe::Exited,
        probe => probe,
    }
}

/// [`probe_process`] with its two observations injected, for a test.
pub fn probe_process_with(
    pid: u32,
    start_of: impl Fn(u32) -> Option<u64>,
    is_free: impl Fn(u32) -> Option<bool>,
) -> ProcessProbe {
    if let Some(started) = start_of(pid) {
        return ProcessProbe::Started(started);
    }
    match is_free(pid) {
        Some(true) => ProcessProbe::Absent,
        Some(false) | None => ProcessProbe::Unknown,
    }
}

/// Whether no process holds `pid`: `Some(true)` on `ESRCH` from
/// `kill(pid, 0)`, `Some(false)` when the signal could be sent or was refused
/// for permission, `None` otherwise (a pid `pid_t` cannot hold, a platform
/// without the call).
#[cfg(unix)]
fn pid_is_free(pid: u32) -> Option<bool> {
    let pid = libc::pid_t::try_from(pid).ok().filter(|p| *p > 0)?;
    // SAFETY: signal 0 delivers nothing; the call only checks the pid.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Some(false);
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Some(true),
        Some(libc::EPERM) => Some(false),
        _ => None,
    }
}

#[cfg(windows)]
fn pid_is_free(_pid: u32) -> Option<bool> {
    None
}

/// PRD #1790 stage 3 (audit "later slices" 2): whether any process is in
/// the session `sid` names, `getsid(2)` asked of every process the OS lists
/// (Linux: `/proc`; macOS: `proc_listallpids`). `None` when that cannot be
/// established: the list could not be read, or a listed process's session
/// could not be read for any reason but the process having exited. Elsewhere
/// always `None`.
pub fn session_has_members(sid: u32) -> Option<bool> {
    session_has_members_in(sid, all_pids()?, running_session_of)
}

/// Audit F4b/S3: the one definition of "a process that can still run" that
/// a stop proof ([`session_members`]) and every later check of a session
/// ([`session_has_members`]) share: a process that exists and has not exited
/// waiting to be reaped. `Some(Some(sid))` for such a process in `sid`,
/// `Some(None)` for one that is gone or a zombie, `None` when its session
/// could not be read. A state that cannot be read is not taken for a zombie,
/// so it stays unknown or running, never gone.
fn running_session_of(pid: u32) -> Option<Option<u32>> {
    if is_zombie(pid) {
        Some(None)
    } else {
        session_of_checked(pid)
    }
}

/// [`session_has_members`] over `pids`, with `session_of` answering
/// `Some(Some(sid))`, `Some(None)` for a process that exited meanwhile, and
/// `None` for one whose session could not be read.
pub fn session_has_members_in(
    sid: u32,
    pids: impl IntoIterator<Item = u32>,
    session_of: impl Fn(u32) -> Option<Option<u32>>,
) -> Option<bool> {
    let mut unknown = false;
    for pid in pids {
        match session_of(pid) {
            Some(Some(found)) if found == sid => return Some(true),
            Some(_) => {}
            None => unknown = true,
        }
    }
    (!unknown).then_some(false)
}

/// PRD #1790 stage 3 (audit F4): the processes in the session `sid` names
/// that can still run, as [`session_has_members`] finds them, each with the
/// start time it had when listed ([`process_start_of`]); `None` when that
/// cannot be established. A member is named by both, so a pid reissued to
/// another process after the listing does not answer to it
/// ([`SessionMember`]).
pub fn session_members(sid: u32) -> Option<Vec<SessionMember>> {
    let pids = session_members_in(sid, all_pids()?, running_session_of)?;
    let mut members = Vec::with_capacity(pids.len());
    for pid in pids {
        match process_start_of(pid) {
            Some(start) => members.push(SessionMember { pid, start }),
            // Gone since it was listed: nothing to stop.
            None if pid_is_free(pid) == Some(true) => {}
            None => return None,
        }
    }
    Some(members)
}

/// One process of a session, as [`session_members`] listed it: its pid and
/// its start time, which together name one process and not whichever holds
/// the pid later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionMember {
    pub pid: u32,
    pub start: u64,
}

/// Audit F4a/S2: a handle on one process that a signal sent through it
/// reaches only that process, never one that holds its pid after it. Linux
/// holds a pidfd (`pidfd_open(2)`); a kernel without the call (`ENOSYS`,
/// before 5.3), and every other Unix, holds the bare pid, so there a pid
/// reissued between the last check and the signal is the documented
/// residual window (`docs/develop/agent-restore.md`). On a kernel that has
/// the call, any other failure to open one leaves the process unopened
/// ([`Opened::Unusable`], round-4 F5): it is not signalled by its bare pid.
#[cfg(unix)]
pub struct ProcessHandle {
    pid: u32,
    #[cfg(target_os = "linux")]
    fd: Option<std::os::fd::OwnedFd>,
}

/// What opening a handle on a process found ([`ProcessHandle::open`]).
#[derive(Debug, PartialEq, Eq)]
pub enum Opened<H> {
    /// A handle on the process that held the pid when it was opened.
    Handle(H),
    /// No process holds the pid, or it is not one a process can hold.
    Absent,
    /// A process may hold it, but no handle that pins it could be had (a
    /// full descriptor table, a refused call): it must not be signalled.
    Unusable,
}

/// Round-4 F5: what a failed `pidfd_open(2)`, its `errno` `errno`, leaves
/// [`ProcessHandle::open`] with: `ESRCH` names no process; `ENOSYS`, a
/// kernel without the call, falls back to the bare pid (`Handle(())`, the
/// documented window); anything else is [`Opened::Unusable`].
#[cfg(unix)]
pub fn pidfd_open_failure(errno: Option<i32>) -> Opened<()> {
    match errno {
        Some(libc::ESRCH) => Opened::Absent,
        Some(libc::ENOSYS) => Opened::Handle(()),
        _ => Opened::Unusable,
    }
}

#[cfg(unix)]
impl ProcessHandle {
    /// Open a handle on the process holding `pid` now ([`Opened`]).
    pub fn open(pid: u32) -> Opened<Self> {
        let Some(raw) = libc::pid_t::try_from(pid).ok().filter(|p| *p > 0) else {
            return Opened::Absent;
        };
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::FromRawFd as _;
            // SAFETY: `pidfd_open(2)` takes a pid and flags and returns a
            // new descriptor or -1; it touches no memory of ours.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, raw, 0) };
            if fd >= 0 {
                // SAFETY: a descriptor the kernel just returned to us, owned
                // by nothing else.
                let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) };
                return Opened::Handle(ProcessHandle { pid, fd: Some(fd) });
            }
            match pidfd_open_failure(std::io::Error::last_os_error().raw_os_error()) {
                Opened::Handle(()) => Opened::Handle(ProcessHandle { pid, fd: None }),
                Opened::Absent => Opened::Absent,
                Opened::Unusable => Opened::Unusable,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = raw;
            Opened::Handle(ProcessHandle { pid })
        }
    }

    /// The pid the handle was opened for.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether the handle pins its process: a signal through it cannot
    /// reach a later holder of the pid.
    pub fn is_pinned(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.fd.is_some()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    /// Send `SIGKILL` through the handle. `true` when it was delivered.
    pub fn kill(&self) -> bool {
        self.signal(libc::SIGKILL)
    }

    /// Send `SIGSTOP` through the handle. `true` when it was delivered.
    pub fn stop(&self) -> bool {
        self.signal(libc::SIGSTOP)
    }

    fn signal(&self, signal: libc::c_int) -> bool {
        #[cfg(target_os = "linux")]
        if let Some(fd) = &self.fd {
            use std::os::fd::AsRawFd as _;
            // SAFETY: a pidfd we own, a valid signal number, no siginfo,
            // no flags.
            let sent = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            return sent == 0;
        }
        let Ok(pid) = libc::pid_t::try_from(self.pid) else {
            return false;
        };
        // SAFETY: a positive pid the caller checked was the listed process
        // just now; see the type's docs for the window that remains.
        unsafe { libc::kill(pid, signal) == 0 }
    }
}

/// Round-4 F5: [`probe_process`] without its reading of a zombie as
/// [`ProcessProbe::Exited`]: a process that exited and waits to be reaped
/// still holds its pid, and is named by its start time like a running one.
pub fn probe_process_held(pid: u32) -> ProcessProbe {
    probe_process_with(pid, process_start_of, pid_is_free)
}

/// Whether `pid` has exited and only waits to be reaped (Linux: state `Z`
/// or `X` in `/proc/<pid>/stat`): a process that holds nothing and runs
/// nothing, which neither [`session_members`] nor [`session_has_members`]
/// counts. `false` wherever that cannot be read.
#[cfg(target_os = "linux")]
pub fn is_zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            let (_, rest) = stat.rsplit_once(')')?;
            rest.trim_start().chars().next()
        })
        .is_some_and(|state| matches!(state, 'Z' | 'X'))
}

#[cfg(not(target_os = "linux"))]
pub fn is_zombie(_pid: u32) -> bool {
    false
}

/// [`session_members`] over `pids`, with `session_of` as for
/// [`session_has_members_in`].
pub fn session_members_in(
    sid: u32,
    pids: impl IntoIterator<Item = u32>,
    session_of: impl Fn(u32) -> Option<Option<u32>>,
) -> Option<Vec<u32>> {
    let mut members = Vec::new();
    for pid in pids {
        match session_of(pid)? {
            Some(found) if found == sid => members.push(pid),
            _ => {}
        }
    }
    Some(members)
}

/// `getsid(pid)`: `Some(Some(sid))`, `Some(None)` on `ESRCH` (or a pid no
/// process can hold), `None` on any other error.
#[cfg(unix)]
fn session_of_checked(pid: u32) -> Option<Option<u32>> {
    let Some(pid) = libc::pid_t::try_from(pid).ok().filter(|p| *p > 0) else {
        return Some(None);
    };
    // SAFETY: `getsid(2)` has no side effects and only reads the process table.
    let sid = unsafe { libc::getsid(pid) };
    if sid >= 0 {
        return Some(u32::try_from(sid).ok());
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Some(None),
        _ => None,
    }
}

#[cfg(windows)]
fn session_of_checked(_pid: u32) -> Option<Option<u32>> {
    None
}

/// Every process id the OS lists, or `None` when the list cannot be read.
#[cfg(target_os = "linux")]
fn all_pids() -> Option<Vec<u32>> {
    let entries = std::fs::read_dir("/proc").ok()?;
    let mut pids = Vec::new();
    for entry in entries {
        let entry = entry.ok()?;
        if let Some(pid) = entry.file_name().to_str().and_then(|n| {
            (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                .then(|| n.parse::<u32>().ok())
                .flatten()
        }) {
            pids.push(pid);
        }
    }
    // A `/proc` that lists no process at all (not mounted, a bare directory)
    // cannot answer: this process is always in a real one.
    (!pids.is_empty()).then_some(pids)
}

/// See the Linux variant.
#[cfg(target_os = "macos")]
fn all_pids() -> Option<Vec<u32>> {
    // SAFETY: a null buffer of size 0 asks only for the count.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let count = usize::try_from(count).ok().filter(|c| *c > 0)?;
    // Room for processes started between the two calls.
    let capacity = count + 256;
    let mut buffer: Vec<libc::pid_t> = vec![0; capacity];
    let bytes = libc::c_int::try_from(capacity * std::mem::size_of::<libc::pid_t>()).ok()?;
    // SAFETY: `buffer` is a live, writable buffer of exactly `bytes` bytes;
    // the call writes at most that many and answers how many pids it wrote.
    let written = unsafe { libc::proc_listallpids(buffer.as_mut_ptr().cast(), bytes) };
    let written = usize::try_from(written).ok().filter(|w| *w > 0)?;
    // A full buffer may have been cut short: refuse to answer from it.
    if written >= capacity {
        return None;
    }
    buffer.truncate(written);
    Some(
        buffer
            .into_iter()
            .filter_map(|pid| u32::try_from(pid).ok())
            .collect(),
    )
}

/// See the Linux variant.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn all_pids() -> Option<Vec<u32>> {
    None
}

/// R2-fix4 (U2): the parent a placement walk follows ([`ancestry_with`]).
/// [`parent_pid_of`] itself on Unix; on Windows `None`, not `Some(0)`, for a
/// recorded parent that cannot be opened or whose pid was reused.
#[cfg(unix)]
pub fn placement_parent_of(pid: u32) -> Option<u32> {
    parent_pid_of(pid)
}
#[cfg(windows)]
pub use windows::placement_parent_of;

/// PR #1735 second review: whether [`parent_pid_of`] reads parents on this
/// platform: Linux (`/proc`), macOS (`proc_pidinfo`) and Windows (a Toolhelp
/// snapshot). Elsewhere it always answers `None`, so [`descends_from`] can
/// never give positive evidence and a check that requires it would refuse
/// every caller.
pub const ANCESTRY_LOOKUP: bool = cfg!(any(target_os = "linux", target_os = "macos", windows));

/// PRD #1666 hardening M2: whether process `pid` is `root` or descends from
/// it, walking parents with [`parent_pid_of`]. `None` when the walk cannot be
/// completed — the platform cannot read a parent, or a process on the way
/// exited — so the caller can tell "no" from "cannot say".
pub fn descends_from(pid: u32, root: u32) -> Option<bool> {
    descends_from_with(pid, root, parent_pid_of)
}

/// [`descends_from`] over any parent lookup. A walk that runs out of steps
/// ([`Ancestry::TooDeep`]) answers `Some(false)`, as a top-of-tree end does.
pub fn descends_from_with(
    pid: u32,
    root: u32,
    parent_of: impl Fn(u32) -> Option<u32>,
) -> Option<bool> {
    match ancestry_with(pid, root, parent_of) {
        Ancestry::Descends => Some(true),
        Ancestry::Outside | Ancestry::TooDeep => Some(false),
        Ancestry::Unreadable => None,
    }
}

/// How a walk up from a process toward a root ended ([`ancestry_with`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ancestry {
    /// The process is the root or descends from it.
    Descends,
    /// The walk reached a process with no parent to follow (pid 0 or 1, or
    /// one that is its own parent) without meeting the root.
    Outside,
    /// A parent could not be read: the platform cannot, or a process on the
    /// way exited.
    Unreadable,
    /// The walk took [`MAX_ANCESTRY_DEPTH`] steps without ending; a deeper
    /// tree, or a table that changed under the walk.
    TooDeep,
}

/// How many parents [`ancestry_with`] follows before answering
/// [`Ancestry::TooDeep`]. A bound on the walk, not on how deep a process
/// tree can be.
pub const MAX_ANCESTRY_DEPTH: usize = 128;

/// Walk `pid`'s parents with `parent_of` toward `root`, and say how the walk
/// ended. Unlike [`descends_from_with`] it keeps a walk that ran out of steps
/// apart from one that ended outside `root`'s tree.
pub fn ancestry_with(pid: u32, root: u32, parent_of: impl Fn(u32) -> Option<u32>) -> Ancestry {
    let mut current = pid;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        if current == root {
            return Ancestry::Descends;
        }
        if current <= 1 {
            return Ancestry::Outside;
        }
        let Some(parent) = parent_of(current) else {
            return Ancestry::Unreadable;
        };
        if parent == current {
            return Ancestry::Outside;
        }
        current = parent;
    }
    Ancestry::TooDeep
}

/// A [`portable_pty::Child`] stand-in over a real [`std::process::Child`], so the
/// teardown state machines in both backends can be driven against **real**
/// processes without a PTY (PRD #163 review).
///
/// Lives here, not in a backend, because both the shared cross-platform test
/// below and the Windows backend's descendant-leak test need it. Only the three
/// methods the teardown path actually calls do anything; `clone_killer` is not on
/// that path and returns a no-op killer rather than pretending to duplicate the
/// handle.
#[cfg(test)]
pub(crate) mod test_child {
    /// Wraps a real OS child. `Debug` is required by the `portable_pty` traits.
    #[derive(Debug)]
    pub(crate) struct StdChild(pub(crate) std::process::Child);

    fn to_pty_status(status: std::process::ExitStatus) -> portable_pty::ExitStatus {
        portable_pty::ExitStatus::with_exit_code(status.code().unwrap_or(0) as u32)
    }

    /// Stand-in for the killer handle `clone_killer` is contractually required to
    /// produce. Nothing in the teardown path clones a killer, so it is never used.
    #[derive(Debug)]
    struct NoopKiller;

    impl portable_pty::ChildKiller for NoopKiller {
        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(NoopKiller)
        }
    }

    impl portable_pty::ChildKiller for StdChild {
        fn kill(&mut self) -> std::io::Result<()> {
            self.0.kill()
        }
        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(NoopKiller)
        }
    }

    impl portable_pty::Child for StdChild {
        fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
            Ok(self.0.try_wait()?.map(to_pty_status))
        }
        fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
            self.0.wait().map(to_pty_status)
        }
        fn process_id(&self) -> Option<u32> {
            Some(self.0.id())
        }
        #[cfg(windows)]
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            None
        }
    }

    /// A latch the test flips to let a deliberately-wedged reap finally
    /// complete, so a teardown thread can always be joined.
    #[derive(Debug, Default)]
    pub(crate) struct WedgeGate {
        released: std::sync::Mutex<bool>,
        wake: std::sync::Condvar,
    }

    impl WedgeGate {
        fn is_released(&self) -> bool {
            *self.released.lock().unwrap()
        }

        pub(crate) fn block_until_released(&self) {
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.wake.wait(released).unwrap();
            }
        }

        pub(crate) fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.wake.notify_all();
        }
    }

    /// A [`portable_pty::Child`] whose *reap* is under the test's control while
    /// its *kill* is observable. It models a child stuck in uninterruptible
    /// kernel I/O, which no ordinary test process can reproduce deterministically.
    #[derive(Debug)]
    pub(crate) struct WedgedChild {
        /// `Some` makes Unix's production `killpg` target that process group;
        /// `None` drives the portable `Child::kill` fallback on both backends.
        pub(crate) pid: Option<u32>,
        /// A count because phase 1's graceful ask also uses the fallback on Unix.
        pub(crate) kills: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        /// Set when either `wait` or `try_wait` actually collects an exit.
        pub(crate) reaped: std::sync::Arc<std::sync::atomic::AtomicBool>,
        /// Until released, `wait` blocks and `try_wait` reports no exit.
        pub(crate) gate: std::sync::Arc<WedgeGate>,
        /// Models a blocking `wait` that never returns even after `try_wait` can
        /// collect the child, proving the teardown uses nonblocking polling.
        pub(crate) wait_never_returns: bool,
    }

    impl WedgedChild {
        pub(crate) fn new(pid: Option<u32>, gate: std::sync::Arc<WedgeGate>) -> Self {
            Self {
                pid,
                kills: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                reaped: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                gate,
                wait_never_returns: false,
            }
        }
    }

    impl portable_pty::ChildKiller for WedgedChild {
        fn kill(&mut self) -> std::io::Result<()> {
            self.kills.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            unreachable!("no teardown path clones a killer")
        }
    }

    impl portable_pty::Child for WedgedChild {
        fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
            if !self.gate.is_released() {
                return Ok(None);
            }
            self.reaped.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(Some(portable_pty::ExitStatus::with_exit_code(0)))
        }

        fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
            if self.wait_never_returns {
                loop {
                    std::thread::park();
                }
            }
            self.gate.block_until_released();
            self.reaped.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(portable_pty::ExitStatus::with_exit_code(0))
        }

        fn process_id(&self) -> Option<u32> {
            self.pid
        }

        #[cfg(windows)]
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            None
        }
    }

    /// Poll `done` until it holds or `budget` elapses. Returns what it last saw.
    pub(crate) fn holds_within(
        budget: std::time::Duration,
        mut done: impl FnMut() -> bool,
    ) -> bool {
        let deadline = std::time::Instant::now() + budget;
        while std::time::Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        done()
    }

    /// How long a wedged teardown gets before its test calls it hung.
    pub(crate) const WEDGE_TEST_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

    /// Kills phase 1 contributes for a pid-less child before the force phase.
    pub(crate) const PHASE_ONE_FALLBACK_KILLS: usize = if cfg!(unix) { 1 } else { 0 };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PRD #1790 stage 3 (review R1 / audit B1): a start time that cannot be
    /// read is absence only when the kernel says the pid is free; a refused
    /// or failed lookup is unknown, never gone.
    #[test]
    fn a_failed_identity_lookup_is_unknown_and_only_esrch_is_absence() {
        let none = |_| None;
        assert_eq!(
            probe_process_with(42, |_| Some(7), |_| Some(true)),
            ProcessProbe::Started(7)
        );
        assert_eq!(
            probe_process_with(42, none, |_| Some(true)),
            ProcessProbe::Absent
        );
        assert_eq!(
            probe_process_with(42, none, |_| Some(false)),
            ProcessProbe::Unknown,
            "a live process whose start time could not be read"
        );
        assert_eq!(
            probe_process_with(42, none, |_| None),
            ProcessProbe::Unknown
        );
    }

    /// The real probe: this process is running with a start time where the
    /// platform reads one, and a pid no process can hold is absent.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn the_probe_sees_this_process_and_not_a_free_pid() {
        assert!(matches!(
            probe_process(std::process::id()),
            ProcessProbe::Started(_)
        ));
        assert_eq!(pid_is_free(i32::MAX as u32), Some(true));
    }

    /// PRD #1790 stage 3 (audit F4b/S3): a session whose only process has
    /// exited and is held unreaped (a zombie) is empty for every check alike:
    /// the stop proof's member list, the later "is anything left" scan, and
    /// the leader's own probe.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_retained_zombie_is_gone_for_every_session_check() {
        use std::os::unix::process::CommandExt as _;
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "exit 0"]);
        // SAFETY: `setsid` is async-signal-safe and touches nothing shared.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        // Not reaped: it stays a zombie until `wait` below.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !is_zombie(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "never became a zombie"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(probe_process(pid), ProcessProbe::Exited);
        assert_eq!(session_has_members(pid), Some(false));
        assert_eq!(session_members(pid), Some(Vec::new()));
        child.wait().unwrap();
    }

    /// PRD #1790 stage 3: a session is empty only when every listed process
    /// answered; one whose session could not be read makes it unknown, one
    /// that exited meanwhile does not.
    #[test]
    fn a_session_scan_that_could_not_read_a_process_is_unknown() {
        let table = |pid: u32| match pid {
            1 => Some(Some(1)),
            2 => Some(Some(50)),
            3 => Some(None),
            _ => None,
        };
        assert_eq!(session_has_members_in(50, [1, 2, 3], table), Some(true));
        assert_eq!(session_has_members_in(60, [1, 2, 3], table), Some(false));
        assert_eq!(session_has_members_in(60, [1, 4], table), None);
        assert_eq!(session_has_members_in(50, [4, 2], table), Some(true));
    }

    /// The real scan: this process's own session has a member, and a session
    /// no process can be in has none.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn the_session_scan_finds_this_processs_own_session() {
        let own = session_of(std::process::id()).expect("this process's session");
        assert_eq!(session_has_members(own), Some(true));
        assert_ne!(session_has_members(i32::MAX as u32), Some(true));
    }

    /// R2-fix4 (U2): a walk that runs out of steps is told apart from one
    /// that ends outside the root's tree, and from one that cannot read a
    /// parent; [`descends_from_with`] still answers it `Some(false)`.
    #[test]
    fn ancestry_tells_a_walk_out_of_steps_from_one_outside() {
        let chain = |pid: u32| match pid {
            100..300 => Some(pid + 1),
            300 => Some(7),
            _ => None,
        };
        assert_eq!(ancestry_with(100, 7, chain), Ancestry::TooDeep);
        assert_eq!(descends_from_with(100, 7, chain), Some(false));
        assert_eq!(ancestry_with(250, 7, chain), Ancestry::Descends);
        assert_eq!(ancestry_with(250, 9, chain), Ancestry::Unreadable);
        assert_eq!(ancestry_with(250, 9, |_| Some(1)), Ancestry::Outside);
    }

    /// PRD #163 M3 — the platform seam `daemon stop` is built on. On Unix the
    /// graceful step MUST stay a signal: that is the property that lets
    /// `daemon stop` kill a daemon predating every protocol surface (PRD #103's
    /// whole motivation). A regression to `ShutdownProtocol` here would silently
    /// add a `KIND_SHUTDOWN` round-trip to the Unix path and break that.
    #[test]
    fn graceful_stop_delivery_matches_the_platform_mechanism() {
        #[cfg(unix)]
        assert_eq!(GRACEFUL_STOP_DELIVERY, GracefulStopDelivery::Signal);
        #[cfg(windows)]
        assert_eq!(
            GRACEFUL_STOP_DELIVERY,
            GracefulStopDelivery::ShutdownProtocol
        );
    }

    /// The Win32 pid guard: everything but 0 passes through untouched (a Windows
    /// PID is a full `u32`, so there is no overflow arm to check).
    #[test]
    fn checked_target_pid_accepts_every_nonzero_pid() {
        assert_eq!(checked_target_pid(1).unwrap(), 1);
        assert_eq!(checked_target_pid(12345).unwrap(), 12345);
        // Above i32::MAX — legal on Windows, unlike the Unix `pid_t` guards.
        assert_eq!(checked_target_pid(u32::MAX).unwrap(), u32::MAX);
    }

    /// 0 is a wildcard on both Win32 calls this guards, so it must be refused
    /// with `InvalidInput` rather than passed through — the Windows analogue of
    /// the `kill(0, …)` / `killpg(0, …)` broadcast hazard.
    #[test]
    fn checked_target_pid_rejects_zero_pid() {
        let err = checked_target_pid(0).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let msg = err.to_string();
        assert!(msg.contains("wildcard"), "{msg:?}");
    }

    /// PRD #163 review — the teardown contract both backends owe the caller for
    /// the case `close_agent` hits whenever the user closes a pane whose agent
    /// quit on its own: a child that exits **inside** the grace window must be
    /// torn down promptly rather than costing the whole window, and must not panic
    /// on a process there is nothing left to signal.
    ///
    /// Un-gated on purpose — it runs on the Linux and the `windows-latest`
    /// `cargo nextest run` jobs, driving each backend's real state machine against
    /// a real OS process. The Windows-specific half of the same fix (the job is
    /// still terminated, so descendants of an exited child are reaped instead of
    /// leaking) needs a second process in the job and lives in that backend's
    /// tests.
    #[test]
    fn terminating_a_child_that_exits_during_the_grace_window_is_prompt() {
        // A process that exits immediately, on either platform. Deliberately NOT
        // reaped here: production reaps through the teardown call itself, so the
        // pid must still be the child's (a zombie on Unix, a live process object
        // on Windows) when the state machine runs.
        let program = if cfg!(windows) { "cmd" } else { "true" };
        let args: &[&str] = if cfg!(windows) {
            &["/C", "exit", "0"]
        } else {
            &[]
        };
        let spawned = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn a short-lived helper");

        let group = AgentProcessGroup::adopt(Some(spawned.id()));
        let mut child: Box<dyn portable_pty::Child + Send + Sync> =
            Box::new(super::test_child::StdChild(spawned));

        let grace = std::time::Duration::from_secs(3);
        let started = std::time::Instant::now();
        terminate_child_with_grace_and_wait(
            &mut child,
            grace,
            &group,
            std::time::Duration::from_secs(1),
        );
        assert!(
            started.elapsed() < grace,
            "an already-exited child must not cost the full grace window (took {:?})",
            started.elapsed()
        );
    }

    /// PRD #1790 stage 3 (round-4 F5): a `pidfd_open` that fails says the
    /// process is gone only on `ESRCH`; a kernel without the call falls back
    /// to the bare pid; every other failure (a full descriptor table, a
    /// refused call) leaves nothing that may be signalled.
    #[cfg(unix)]
    #[test]
    fn a_pidfd_that_cannot_be_opened_is_not_signalled_by_its_pid() {
        assert_eq!(pidfd_open_failure(Some(libc::ESRCH)), Opened::Absent);
        assert_eq!(pidfd_open_failure(Some(libc::ENOSYS)), Opened::Handle(()));
        for errno in [
            Some(libc::EMFILE),
            Some(libc::ENFILE),
            Some(libc::EPERM),
            Some(libc::EINVAL),
            Some(libc::ENOMEM),
            None,
        ] {
            assert_eq!(pidfd_open_failure(errno), Opened::Unusable, "{errno:?}");
        }
        // A pid no process can hold opens nothing.
        assert!(matches!(ProcessHandle::open(0), Opened::Absent));
        assert!(matches!(ProcessHandle::open(u32::MAX), Opened::Absent));
        // This process, which the test may name, is held, never absent.
        match ProcessHandle::open(std::process::id()) {
            Opened::Handle(handle) => assert_eq!(handle.pid(), std::process::id()),
            Opened::Unusable => {}
            Opened::Absent => panic!("this process reads as absent"),
        }
    }
}
