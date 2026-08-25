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
///
/// **Refuses a symlink at `dir`** (issue #669). Both of the calls below follow
/// symlinks, so without this guard a same-uid attacker who plants a symlink at
/// the state dir / lock root ahead of the daemon redirects the whole function
/// onto a directory of their choosing: `mkdir(2)` returns `EEXIST`, stdlib's
/// recursive fallback swallows it because `path.is_dir()` follows the link, and
/// the `set_permissions` that follows chmods 0o700 onto the attacker's target.
/// Measured before the fix: `Ok(())`, with an unrelated 0o755 directory left at
/// 0o700. The `lstat` refusal turns that into a named error and leaves the
/// target untouched.
///
/// **This narrows the window; it does not close it.** A residual TOCTOU remains
/// between the `symlink_metadata` here and the `mkdir`/`chmod` that follow — an
/// attacker who wins that race still redirects the chmod. Closing it properly
/// needs a race-free idiom (open the created directory `O_NOFOLLOW|O_DIRECTORY`
/// and `fchmod` the descriptor, so no second name resolution exists to
/// redirect), which is deliberately not done here: a read-open is strictly more
/// privileged than `chmod(2)` by path, so it would refuse to repair a
/// pre-existing 0o000 directory that today it fixes. Treat this as a narrowed
/// window with a known remaining gap, not as an eliminated race.
///
/// Nothing here defends against a symlink at an *ancestor* of `dir`; the guard
/// is scoped to the final component, which is the one this function creates and
/// chmods. (Compare `platform::detach::spawn_daemon_serve_detached_with_exe`,
/// which already opens `daemon.log` with `O_NOFOLLOW` — the same discipline one
/// level down, applied to a file the deck opens rather than a dir it chmods.)
///
/// **The refusal is unconditional, and that has a cost.** The threat model is a
/// *same-uid* attacker, so nothing about the link or its target distinguishes
/// one they planted from one the operator made on purpose — an ownership or
/// mode check on the target would discriminate between nothing. So a deliberate
/// `~/.local/state/dot-agent-deck` → other-disk symlink is refused along with
/// the attack, and the operator has to point `DOT_AGENT_DECK_STATE_DIR` (or
/// `DOT_AGENT_DECK_LOCK_DIR`) at the real path instead. Only the *final*
/// component is affected, so a symlinked ancestor — `~/.local/state` itself —
/// keeps working. The error says which path and why, so the fix is discoverable
/// from the message.
pub fn ensure_owner_only_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    // Fail closed: a symlink is refused, and so is any `lstat` error other than
    // "nothing is there yet" — if we cannot vouch for what sits at the path we
    // do not chmod it.
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "refusing to use {}: it is a symlink, and creating or chmodding it would \
                     follow the link and tighten permissions on another directory — point the \
                     path at the real directory instead of a symlink",
                    dir.display()
                ),
            ));
        }
        Ok(_) => {}
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(source),
    }

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
///
/// Deliberately carries **no** symlink guard, unlike [`ensure_owner_only_dir`]
/// (issue #669): it never chmods anything, and `DirBuilder`'s mode reaches only
/// a directory the call itself created — a symlink at `dir` makes `mkdir(2)`
/// return `EEXIST` and the call creates nothing — so there is no
/// permission-tightening exposure to guard. Pinned by
/// `create_owner_only_dir_never_tightens_through_a_symlink`. What it does share
/// is that a planted symlink silently redirects where the caller's *config
/// write* lands; that is a path-redirection question about the write itself
/// rather than about this seam's mode policy, so it is out of #669's scope.
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("stat the directory")
            .permissions()
            .mode()
            & 0o777
    }

    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }

    /// A same-uid attacker plants a symlink at the path the deck is about to use
    /// for its state dir / lock root, pointing at a directory of their choosing.
    /// `ensure_owner_only_dir` must refuse the path outright rather than follow
    /// it and chmod 0o700 onto the attacker's target.
    #[test]
    fn ensure_owner_only_dir_refuses_a_symlinked_target() {
        let root = tempfile::tempdir().expect("tempdir");
        let victim = root.path().join("victim");
        std::fs::create_dir(&victim).expect("create the victim directory");
        chmod(&victim, 0o755);

        let planted = root.path().join("state");
        std::os::unix::fs::symlink(&victim, &planted).expect("plant the symlink");

        let err = ensure_owner_only_dir(&planted).expect_err("a symlinked target must be refused");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
        assert!(
            err.to_string().contains("symlink"),
            "the error must name the reason: {err}"
        );

        assert_eq!(
            mode_of(&victim),
            0o755,
            "the attacker's target directory must not be chmodded"
        );
        assert!(
            std::fs::symlink_metadata(&planted)
                .expect("lstat the planted path")
                .file_type()
                .is_symlink(),
            "the symlink must be left alone, not replaced by a real directory"
        );
    }

    /// The same refusal for a *dangling* symlink: without the guard this failed
    /// with a bare `AlreadyExists` from `mkdir(2)`'s `EEXIST`, which names
    /// neither the path nor the reason.
    #[test]
    fn ensure_owner_only_dir_refuses_a_dangling_symlink() {
        let root = tempfile::tempdir().expect("tempdir");
        let planted = root.path().join("state");
        std::os::unix::fs::symlink(root.path().join("nowhere"), &planted)
            .expect("plant the dangling symlink");

        let err = ensure_owner_only_dir(&planted).expect_err("a symlinked target must be refused");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
        assert!(err.to_string().contains("symlink"), "{err}");
    }

    /// The control for both refusals: the two shapes the function actually
    /// exists for — creating a fresh nested directory at 0o700, and repairing a
    /// pre-existing loose one — must still work. Without this a guard that
    /// refused *everything* would look like a fix.
    #[test]
    fn ensure_owner_only_dir_still_creates_and_repairs_real_dirs() {
        let root = tempfile::tempdir().expect("tempdir");

        let fresh = root.path().join("a").join("b");
        ensure_owner_only_dir(&fresh).expect("create a fresh nested directory");
        assert_eq!(mode_of(&fresh), 0o700);

        let loose = root.path().join("loose");
        std::fs::create_dir(&loose).expect("create the loose directory");
        chmod(&loose, 0o755);
        ensure_owner_only_dir(&loose).expect("repair a pre-existing loose directory");
        assert_eq!(
            mode_of(&loose),
            0o700,
            "the defense-in-depth repair must survive the symlink guard"
        );

        // Idempotent: a second call on the dir we just made is still fine.
        ensure_owner_only_dir(&loose).expect("re-applying to an existing directory must work");
        assert_eq!(mode_of(&loose), 0o700);
    }

    /// Issue #669's scope note, mechanized: the sibling `create_owner_only_dir`
    /// does **not** share the permission-tightening exposure, because it never
    /// chmods — `DirBuilder`'s mode applies only to a directory the call itself
    /// created, and a symlink at the target means it creates nothing (PRD #127
    /// S2 deliberately leaves a pre-existing directory's mode alone). Whatever
    /// the call returns, the attacker's target keeps its mode.
    #[test]
    fn create_owner_only_dir_never_tightens_through_a_symlink() {
        let root = tempfile::tempdir().expect("tempdir");
        let victim = root.path().join("victim");
        std::fs::create_dir(&victim).expect("create the victim directory");
        chmod(&victim, 0o755);

        let planted = root.path().join("config");
        std::os::unix::fs::symlink(&victim, &planted).expect("plant the symlink");

        let _ = create_owner_only_dir(&planted);
        assert_eq!(
            mode_of(&victim),
            0o755,
            "create-only must never chmod the attacker's target"
        );
    }
}
