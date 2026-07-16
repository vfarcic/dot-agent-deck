//! Unix detached-daemon spawn: `setsid(2)` + `O_NOFOLLOW`/0o600 log +
//! `/dev/null` stdin. Behavior-preserving lift of the former
//! `daemon_attach::spawn_daemon_serve_detached_with_exe`.

use std::path::Path;

/// Spawn `exe daemon serve` as a detached background process that survives the
/// parent's exit, returning the spawned daemon's pid.
///
/// - The binary is located via [`std::env::current_exe`] by the caller (passed
///   in as `exe`) rather than `$PATH` because non-interactive ssh shells
///   routinely skip `~/.local/bin`.
/// - `setsid(2)` runs in the child via `pre_exec` so the daemon becomes its own
///   session leader and won't receive `SIGHUP` when the parent shell exits.
/// - stdin is `/dev/null` and stdout/stderr append to `<state_dir>/daemon.log`.
///   The log is opened with `O_NOFOLLOW` and mode 0o600 so a same-uid attacker
///   can't pre-place a symlink to redirect daemon output (which contains hook
///   payloads and agent task strings) and the log file itself isn't
///   world-readable on the default umask.
///
/// We do not wait for the child here — the spawned daemon stays up after this
/// returns. Callers poll the attach socket to know when the daemon is ready.
pub fn spawn_daemon_serve_detached_with_exe(state_dir: &Path, exe: &Path) -> std::io::Result<u32> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;

    crate::platform::fsperm::ensure_owner_only_dir(state_dir)?;
    let log_path = state_dir.join("daemon.log");
    let stdout = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(&log_path)
    {
        Ok(f) => f,
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            // O_NOFOLLOW + open of a symlink fails with ELOOP. A symlink at
            // the daemon log path means someone placed it there ahead of us;
            // refuse to write through it rather than silently following.
            return Err(std::io::Error::other(format!(
                "daemon log path {} is a symlink — refusing to follow (someone may have planted it to redirect daemon output)",
                log_path.display()
            )));
        }
        Err(e) => return Err(e),
    };
    let stderr = stdout.try_clone()?;
    let stdin = std::fs::File::open("/dev/null")?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .arg("serve")
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr);

    // SAFETY: `pre_exec` runs in the child between fork and exec. Only
    // async-signal-safe libc calls are permitted here; `setsid(2)` is on
    // POSIX's async-signal-safe list. We do nothing else in the closure.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    // Spawn and immediately drop the handle. The child is now its own session
    // leader; when this process exits, init reaps the child. We don't wait —
    // the caller will poll the attach socket. The pid is returned for tests
    // that need to clean up the spawned daemon; the production wrapper
    // discards it.
    let child = cmd.spawn()?;
    Ok(child.id())
}
