//! Windows detached-daemon spawn (PRD #42 M1 compiling stub; full behavior is
//! PRD #163).
//!
//! Replaces the Unix `setsid` + `pre_exec` with process-creation flags set
//! before `spawn()`: `DETACHED_PROCESS` (no inherited console — the Windows
//! analogue of "no controlling terminal / won't get the parent's SIGHUP") and
//! `CREATE_NEW_PROCESS_GROUP` (the child isn't in the parent's Ctrl-C/Break
//! group). stdin is `NUL`. The `O_NOFOLLOW` symlink defense on the log has no
//! clean `OpenOptions`-level Win32 equivalent; the mitigation is the per-user
//! `%LOCALAPPDATA%` directory ACL (#163). Job-Object breakaway
//! (`CREATE_BREAKAWAY_FROM_JOB`) for kill-on-job-close supervisors is deferred
//! to #163.

use std::path::Path;

// Win32 process-creation flag constants (stable ABI values). Spelled out here
// rather than pulled from `windows-sys` to keep this M1 stub dependency-light;
// #163 may switch to the typed constants when it wires up Job Objects.
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Windows counterpart to the Unix detached spawn. Returns the spawned
/// daemon's pid. See the module docs for the Unix→Windows mapping.
pub fn spawn_daemon_serve_detached_with_exe(state_dir: &Path, exe: &Path) -> std::io::Result<u32> {
    use std::os::windows::process::CommandExt;

    crate::platform::fsperm::ensure_owner_only_dir(state_dir)?;
    let log_path = state_dir.join("daemon.log");
    // Plain owner-dir open: the `%LOCALAPPDATA%` parent is per-user ACL'd, so
    // we rely on the directory ACL rather than a per-open `O_NOFOLLOW` (which
    // has no `OpenOptions` equivalent on Windows).
    let stdout = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let stderr = stdout.try_clone()?;
    let stdin = std::fs::File::open("NUL")?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .arg("serve")
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);

    let child = cmd.spawn()?;
    Ok(child.id())
}
