//! Windows filesystem security (PRD #42 M1 justified no-ops / skeletons; real
//! behavior is PRD #163).
//!
//! The uniform Unix mode-bit model maps to: a current-user-only **pipe
//! security descriptor** (replaces the socket `0o600` + `verify_socket_trusted`
//! owner/mode check in one stroke), and reliance on the per-user
//! `%LOCALAPPDATA%` directory ACL for the state/lock/config dirs (optionally an
//! explicit `SetNamedSecurityInfo`). There is no `umask` and no inode-mode
//! race for a named pipe, so the bind umask dance disappears.
//!
//! At M1: the dir helpers create the directory (the `%LOCALAPPDATA%` parent is
//! already per-user ACL'd) and the mode/verify helpers are no-ops. Endpoint
//! trust + explicit ACLs land in #163.

use std::path::Path;

/// No umask on Windows — a named pipe has no inode-mode race. Runs `f`
/// directly; the current-user-only pipe security descriptor (#163) replaces the
/// `0o600` socket mode.
pub fn with_socket_umask<T>(f: impl FnOnce() -> T) -> T {
    f()
}

/// Create `dir` (recursively). The per-user `%LOCALAPPDATA%` parent is already
/// ACL'd to the current user; an explicit owner-only ACL
/// (`SetNamedSecurityInfo`) is deferred to #163.
pub fn ensure_owner_only_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Create `dir` (recursively). See [`ensure_owner_only_dir`] — same M1
/// behavior; #163 adds the explicit ACL.
pub fn create_owner_only_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// No-op: Windows `OpenOptions` has no mode bits. The owner-only property comes
/// from the directory ACL (#163).
pub fn set_create_mode_owner_only(_opts: &mut std::fs::OpenOptions) {}

/// No-op: see [`set_create_mode_owner_only`].
pub fn set_file_owner_only(_file: &std::fs::File) -> std::io::Result<()> {
    Ok(())
}

/// No-op: a named pipe has no inode mode to restate. The owner-only property
/// comes from the pipe's current-user security descriptor (#163), set at
/// creation time rather than after bind. Mirrors the Unix post-bind 0o600
/// restate folded into [`crate::platform::ipc::IpcListener::bind`].
pub fn set_endpoint_mode_owner_only(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Endpoint trust on Windows is enforced by the named-pipe security descriptor
/// (the OS refuses foreign connections), not an out-of-band stat. M1 stub
/// trusts the endpoint; the pipe SD lands in #163.
pub fn verify_endpoint_trusted(_path: &Path) -> Result<(), String> {
    Ok(())
}
