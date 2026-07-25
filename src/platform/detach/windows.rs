//! Windows detached-daemon spawn (PRD #163 M2).
//!
//! Replaces the Unix `setsid` + `pre_exec` with process-creation flags set before
//! `spawn()`:
//!
//! - `DETACHED_PROCESS` — the child gets no console at all (it does not inherit
//!   the parent's and none is allocated). This is the Windows analogue of "no
//!   controlling terminal", i.e. of the reason `setsid(2)` is called: a console
//!   process would otherwise receive `CTRL_CLOSE_EVENT`/`CTRL_LOGOFF_EVENT` when
//!   the launching console goes away, which is precisely the `SIGHUP` the Unix
//!   backend escapes.
//! - `CREATE_NEW_PROCESS_GROUP` — the daemon heads its own process group instead
//!   of joining the parent's Ctrl-C / Ctrl-Break group, so an interactive Ctrl-C
//!   in the launching shell cannot reach it. (Note this does *not* make the
//!   daemon a `GenerateConsoleCtrlEvent` target: that requires sharing a console,
//!   and `DETACHED_PROCESS` means it has none. #163 M3's `CTRL_BREAK_EVENT` grace
//!   window is for the *agents* the daemon spawns into ConPTYs; the daemon itself
//!   is stopped through the `KIND_SHUTDOWN` protocol, escalating to
//!   `TerminateProcess`.)
//! - `CREATE_BREAKAWAY_FROM_JOB`, **conditionally** — see
//!   [`job_breakaway_needed`]. Without it, a daemon launched from inside a
//!   kill-on-job-close Job Object (a CI runner, a VS Code integrated terminal, a
//!   task-scheduler wrapper, `cmd /c start`-style supervisors) inherits the job
//!   and is killed when the launching process's job handle closes — the exact
//!   "survives the parent's exit" property this function exists to provide.
//!
//! stdin is `NUL`. The Unix `O_NOFOLLOW`/0o600 defense on the log has no
//! `OpenOptions`-level Win32 equivalent; the mitigation is the per-user
//! `%LOCALAPPDATA%` directory ACL, audited with the rest of the permission sites
//! in #163 M4 (`fsperm`).

use std::path::Path;

use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
use windows_sys::Win32::System::JobObjects::{
    IsProcessInJob, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, QueryInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS, GetCurrentProcess,
};

/// Windows counterpart to the Unix detached spawn. Returns the spawned daemon's
/// pid. See the module docs for the Unix→Windows mapping.
///
/// We do not wait for the child — the spawned daemon stays up after this
/// returns; callers poll the attach endpoint to know when it is ready.
pub fn spawn_daemon_serve_detached_with_exe(state_dir: &Path, exe: &Path) -> std::io::Result<u32> {
    crate::platform::fsperm::ensure_owner_only_dir(state_dir)?;
    let log_path = state_dir.join("daemon.log");
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;

    if job_breakaway_needed() {
        match spawn_with_flags(exe, &log_path, base | CREATE_BREAKAWAY_FROM_JOB) {
            Ok(pid) => return Ok(pid),
            // `CreateProcess` rejects `CREATE_BREAKAWAY_FROM_JOB` with
            // ERROR_ACCESS_DENIED when the job does not permit breakaway. We
            // only ask for it after seeing `JOB_OBJECT_LIMIT_BREAKAWAY_OK`, but
            // the limits can change between the query and the spawn, and nested
            // jobs (Windows 8+) mean an outer job may refuse what the inner one
            // advertised. Retry without it rather than fail the spawn outright:
            // a daemon that dies with the job is still better than no daemon.
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
                tracing::warn!(
                    "job object refused CREATE_BREAKAWAY_FROM_JOB ({e}); spawning the daemon \
                     inside the job — it will be killed when the job closes"
                );
            }
            Err(e) => return Err(e),
        }
    }

    spawn_with_flags(exe, &log_path, base)
}

/// Spawn `exe daemon serve` with `flags`, wiring stdin to `NUL` and
/// stdout/stderr to an append handle on `log_path`.
///
/// The stdio handles are opened per attempt (rather than once by the caller)
/// because `Stdio::from(File)` consumes them, so the breakaway retry above needs
/// a fresh set.
fn spawn_with_flags(exe: &Path, log_path: &Path, flags: u32) -> std::io::Result<u32> {
    use std::os::windows::process::CommandExt;

    // Plain owner-dir open: the `%LOCALAPPDATA%` parent is per-user ACL'd, so we
    // rely on the directory ACL rather than a per-open `O_NOFOLLOW` (which has no
    // `OpenOptions` equivalent on Windows).
    let stdout = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let stderr = stdout.try_clone()?;
    let stdin = std::fs::File::open("NUL")?;

    let child = std::process::Command::new(exe)
        .arg("daemon")
        .arg("serve")
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .creation_flags(flags)
        .spawn()?;
    Ok(child.id())
}

/// Whether the detached daemon must break away from the current Job Object to
/// survive the parent's exit.
///
/// True only when all three hold:
///
/// 1. this process is in a job at all (`IsProcessInJob`);
/// 2. that job sets `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` — otherwise job
///    membership is harmless and breaking away would needlessly escape whatever
///    limits the supervisor set;
/// 3. the job sets `JOB_OBJECT_LIMIT_BREAKAWAY_OK` and *not*
///    `JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK`. Silent breakaway already detaches
///    every child automatically, and `CREATE_BREAKAWAY_FROM_JOB` is documented to
///    require the explicit `BREAKAWAY_OK` limit — asking for it otherwise just
///    earns an ERROR_ACCESS_DENIED.
///
/// Any failure to determine the state answers `false`: the fallback is the plain
/// detached spawn, which is what every non-job launch wants.
///
/// Note on nested jobs (Windows 8+): a NULL job handle queries the *immediate*
/// job of the calling process, so an outer job's limits are not visible here.
/// That is why [`spawn_daemon_serve_detached_with_exe`] also handles a runtime
/// refusal instead of trusting this answer.
fn job_breakaway_needed() -> bool {
    let mut in_job: i32 = 0;
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle needing no release; a
    // NULL job handle asks "is it in ANY job"; `in_job` is a valid out-pointer.
    if unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) } == 0
        || in_job == 0
    {
        return false;
    }

    // SAFETY: `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` is a plain-old-data struct
    // of integers, pointers and a nested `IO_COUNTERS`, so an all-zero value is
    // valid and is what the API expects for an out-parameter.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
    // SAFETY: a NULL job handle means "the job this process is in" (documented);
    // the buffer and its true byte length are passed together, and a NULL return
    // length is allowed.
    let ok = unsafe {
        QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&raw mut info).cast(),
            size,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
            "cannot read the current job object's limits; spawning the daemon without \
             CREATE_BREAKAWAY_FROM_JOB"
        );
        return false;
    }

    let limits = info.BasicLimitInformation.LimitFlags;
    let kill_on_close = limits & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0;
    let breakaway_ok = limits & JOB_OBJECT_LIMIT_BREAKAWAY_OK != 0;
    let silent_breakaway = limits & JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK != 0;

    if kill_on_close && !breakaway_ok && !silent_breakaway {
        tracing::warn!(
            "launched inside a kill-on-job-close job object that forbids breakaway; the \
             detached daemon will be killed when the job closes"
        );
    }
    kill_on_close && breakaway_ok && !silent_breakaway
}
