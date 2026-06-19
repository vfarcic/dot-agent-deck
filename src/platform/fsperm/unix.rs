//! Unix filesystem security: `umask`/0o700/0o600 mode bits + socket
//! owner/mode/type verification. Behavior-preserving lift of the permission
//! sites in `daemon.rs`, `daemon_attach.rs`, `remote.rs`, and `schedule_cli.rs`.

use std::path::Path;
use std::sync::Mutex;

/// umask is process-global, so serialize the bind-with-restrictive-umask dance
/// to keep concurrent tests from racing each other's restore. NOTE: this lock
/// only serializes *cooperating* callers that go through [`with_socket_umask`].
/// Any other code path that calls `umask(2)` directly bypasses the lock and can
/// still race with the swap-and-restore here — so don't treat this as a
/// process-global umask guard.
static UMASK_LOCK: Mutex<()> = Mutex::new(());

/// Run `f` (typically a socket `bind(2)`) with the process umask temporarily
/// set to `0o177`, restoring the previous mask afterward. The kernel creates
/// the socket inode with mode `0o777 & ~umask`, so a mask of `0o177` strips the
/// owner-execute bit and all group/other bits and produces `0o600` directly —
/// closing the TOCTOU window between `bind` and a post-bind `chmod`, where a
/// local attacker could connect via the world-readable inode that exists
/// between the two calls.
///
/// Only the umask/mode policy lives here; the socket bind itself stays at the
/// call site (M2 owns the transport).
pub fn with_socket_umask<T>(f: impl FnOnce() -> T) -> T {
    let _guard = UMASK_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // SAFETY: `umask(2)` is a thread-safe libc call that simply swaps a
    // per-process value. We restore the previous mask immediately after `f`
    // so other code (file creation elsewhere) is unaffected.
    let prev = unsafe { libc::umask(0o177) };
    let result = f();
    unsafe {
        libc::umask(prev);
    }
    result
}

/// Create `dir` (recursively) with mode 0o700 **and re-apply the mode to
/// pre-existing directories** — the defense-in-depth pattern shared by the
/// former `daemon_attach::prepare_state_dir` and `daemon::ensure_lock_root`.
/// `DirBuilder::mode(0o700)` only applies to a directory freshly created by the
/// call; an existing dir at looser permissions (stale install, prior
/// misconfigured run) would otherwise stay world-readable, so the unconditional
/// follow-up `set_permissions(0o700)` repairs it.
///
/// `DirBuilder::recursive(true)` makes the mkdir idempotent (stdlib converts
/// `AlreadyExists` to `Ok(())` for an existing directory), so concurrent
/// first-time callers don't fight; real I/O errors still surface.
pub fn ensure_owner_only_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Create `dir` (recursively) with mode 0o700, **without** re-applying the mode
/// to a pre-existing directory. `DirBuilder`'s mode applies only to directories
/// it newly creates, so an existing shared dir keeps its mode — we don't
/// surprise-tighten a dir we didn't make (PRD #127 S2). Used by the
/// `schedules.toml` atomic-write path.
pub fn create_owner_only_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Apply owner-only (0o600) creation mode to an `OpenOptions` builder so the
/// file is created without the group/other bits the default umask would leave.
/// Used by the owner-only atomic config writes (`remotes.toml`,
/// `schedules.toml`, which may carry secrets).
pub fn set_create_mode_owner_only(opts: &mut std::fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    opts.mode(0o600);
}

/// Re-assert owner-only (0o600) permissions on an already-open file. Defense in
/// depth: if a stale temp file from a crashed previous save existed,
/// `OpenOptions::mode()` would NOT have re-applied the bits, so re-set them
/// explicitly before the rename.
pub fn set_file_owner_only(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}

/// Re-assert owner-only (0o600) mode on a freshly-bound socket inode by path.
/// Defense in depth folded into [`crate::platform::ipc::IpcListener::bind`]
/// (PRD #42 M2): the umask-before-`bind(2)` already created the inode at 0o600,
/// but restating it makes the requirement explicit and covers any future code
/// path that binds without the umask dance. Lifts the post-bind
/// `set_permissions(SOCKET_MODE)` restates from `daemon.rs` and
/// `daemon_protocol::bind_attach_listener`.
pub fn set_endpoint_mode_owner_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Verify `path` is a Unix socket owned by the current uid at mode 0o600.
/// Returns `Err(reason)` describing the first failed check; the caller wraps it
/// in its own error type.
///
/// Defends against a same-uid attacker pre-creating a socket at the attach path
/// before the real daemon binds: in that scenario `bind(2)` fails with
/// `EADDRINUSE` for the daemon and `connect(2)` succeeds for us against the
/// attacker's socket. Validating ownership and mode out-of-band closes the gap.
/// Stat is not racy here because we never re-stat after this check — the FD we
/// then connect to is anchored to the inode the kernel resolves during this
/// single call (and any swap underneath us produces an obvious connection error
/// from `UnixStream::connect`).
pub fn verify_endpoint_trusted(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let metadata = std::fs::metadata(path).map_err(|source| format!("stat failed: {source}"))?;

    if !metadata.file_type().is_socket() {
        return Err("not a Unix domain socket".to_string());
    }

    let our_uid = crate::platform::paths::current_uid();
    if metadata.uid() != our_uid {
        return Err(format!(
            "owned by uid {} (expected {})",
            metadata.uid(),
            our_uid
        ));
    }

    let mode = metadata.mode() & 0o777;
    if mode != 0o600 {
        return Err(format!("mode is 0o{mode:o} (expected 0o600)"));
    }

    Ok(())
}
