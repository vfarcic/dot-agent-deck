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
///
/// **The restore is a `Drop`, so it also runs if `f` unwinds** (PRD #742 M11's
/// F4). Every caller today hands this a `bind(2)` that returns `io::Result` and
/// panics for nothing, so this changes no behaviour that exists — what it
/// removes is the dependence on that staying true. A panic escaping `f` with
/// the old shape left the process at `0o177` permanently, and that is not a
/// quiet failure: `bind_trusted_socket`'s own comment records measuring four
/// sibling tests dying with `EACCES` inside their own temp roots because every
/// `tempfile::tempdir()` created while the mask was up landed at `0o600` with
/// no search bit. Restoring from a guard costs nothing and turns that from a
/// standing assumption about every future `f` into a property of this function.
///
/// The guard is declared after `_guard`, so it drops first: the mask is back
/// before [`UMASK_LOCK`] is released, and the next cooperating caller never
/// sees `0o177`. (A panic poisons that mutex; the `into_inner` above is what
/// keeps a poisoned lock from turning one panicking caller into every later
/// caller's panic.)
pub fn with_socket_umask<T>(f: impl FnOnce() -> T) -> T {
    /// Restores the mask this swapped out, on the ordinary path and on an
    /// unwind alike.
    struct Restore(libc::mode_t);
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: as below — `umask(2)` swaps a per-process value and
            // cannot fail.
            unsafe {
                libc::umask(self.0);
            }
        }
    }

    let _guard = UMASK_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // SAFETY: `umask(2)` is a thread-safe libc call that simply swaps a
    // per-process value. `Restore` puts the previous mask back as soon as `f`
    // is done, so other code (file creation elsewhere) is unaffected.
    let _restore = Restore(unsafe { libc::umask(0o177) });
    f()
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
/// **The `lstat` is the cheap guard; the descriptor below is the real one**
/// (issue #1121 round two). This comment used to say the race-free idiom was
/// deliberately not used, because a read-open is more privileged than
/// `chmod(2)` by path and would refuse to repair a pre-existing `0o000`
/// directory. Both halves of that were true and the conclusion was still
/// wrong: the idiom is used now, and the `0o000` case is handled by falling
/// back to the path chmod **only** on `EACCES`, which a planted symlink cannot
/// produce (`O_NOFOLLOW` answers one with `ELOOP`).
///
/// What the function does on Unix, in order: refuse a symlink by `lstat`;
/// create the parents recursively at `0o700`; create the final component with
/// **one non-recursive** `mkdir(2)`, which does not follow a link at that
/// component; then open the result `O_RDONLY|O_DIRECTORY|O_NOFOLLOW`, `fstat`
/// it and `fchmod` **the descriptor**. There is no second pathname resolution
/// for an attacker to redirect, so the window the old comment described — plant
/// a symlink after the `lstat`, have `create_dir_all` follow it and the
/// path-based `set_permissions` chmod the target — is gone. That matters more
/// after this issue than before it: [`crate::endpoint_resolve::ensure_endpoint_dir`]
/// now calls this at a **world-writable-parent** boundary (`/tmp`), where the
/// racing uid is a *foreign* one rather than the same-uid attacker the rest of
/// this comment is written against.
///
/// **The residual, stated at its real width.** The `EACCES` repair arm does
/// chmod by path, after a fresh `lstat`, so a directory swapped in between
/// those two calls is chmodded to `0o700`. It buys an attacker very little:
/// `chmod(2)` refuses a directory the calling uid does not own, so the only
/// thing reachable that way is tightening a directory the invoking user
/// already owns — and reaching the arm at all requires first presenting
/// something that answers `EACCES` rather than `ELOOP`. The arm is re-checked
/// by descriptor afterwards, so nothing proceeds on a directory the `fstat`
/// does not vouch for.
///
/// Nothing here defends against a symlink at an *ancestor* of `dir`; the guard
/// is scoped to the final component, which is the one this function creates and
/// chmods. An ancestor a foreign uid controls is therefore still outside what
/// this can promise — for the endpoint directory that ancestor is the system
/// temp dir, which is `0o1777` and root-owned, so it is not the reachable case
/// there. (Compare `platform::detach::spawn_daemon_serve_detached_with_exe`,
/// which already opens `daemon.log` with `O_NOFOLLOW` — the same discipline one
/// level down, applied to a file the deck opens rather than a dir it chmods.)
///
/// **The refusal is unconditional, and that has a cost — unchanged by the
/// descriptor work above.** For the same-uid attacker this function was first
/// written against, nothing about the link or its target distinguishes one they
/// planted from one the operator made on purpose — an ownership or mode check
/// on the target would discriminate between nothing. So a deliberate
/// `~/.local/state/dot-agent-deck` → other-disk symlink is refused along with
/// the attack, and the operator has to point `DOT_AGENT_DECK_STATE_DIR` (or
/// `DOT_AGENT_DECK_LOCK_DIR`) at the real path instead. Only the *final*
/// component is affected, so a symlinked ancestor — `~/.local/state` itself —
/// keeps working. The error says which path and why, so the fix is discoverable
/// from the message, and the `O_NOFOLLOW` arm renders the identical message so
/// a deliberate link planted a moment before the call reads no differently.
///
/// **The other fail-closed arm says why too** (issue #1121). When the directory
/// exists and another uid owns it the `set_permissions` below returns `EPERM`,
/// which unwrapped reads `Operation not permitted` and names neither the path
/// nor the owner. [`chmod_refusal`] wraps it: which directory, which uid owns
/// it, which uid we are, and that a `DOT_AGENT_DECK_*` path override is the way
/// out. The remedy is named generically here on purpose — this function serves
/// the state dir, the lock root and (via
/// [`crate::endpoint_resolve::ensure_endpoint_dir`]) the endpoint directory,
/// and each has a different override — so the caller closest to the operator
/// names the specific variable.
pub fn ensure_owner_only_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    // Fail closed: a symlink is refused, and so is any `lstat` error other than
    // "nothing is there yet" — if we cannot vouch for what sits at the path we
    // do not chmod it. The descriptor dance below is what makes the refusal
    // race-free; this is the arm that produces the *readable* error for the
    // overwhelmingly common non-racing case.
    refuse_symlink_at(dir)?;

    // Parents first, recursively and at the mode the whole call used to use, so
    // an intermediate directory this creates lands exactly where it always did.
    // Splitting them off leaves the final component — the one a foreign uid can
    // race in a world-writable parent — as the only one created by a call that
    // cannot follow a link.
    if let Some(parent) = dir.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }

    // One non-recursive `mkdir(2)`. It does not follow a symlink at the final
    // component — a planted link answers `EEXIST`, it does not create through
    // the link — so every "something was already there" case, hostile or not,
    // falls into the descriptor path below rather than into a path-based chmod.
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(source) => return Err(source),
    }

    let handle = open_dir_nofollow(dir)?;
    // `File::set_permissions` is `fchmod(2)`: it acts on the inode this
    // descriptor already holds, so there is no name for anything to swap.
    handle
        .set_permissions(std::fs::Permissions::from_mode(0o700))
        .map_err(|source| chmod_refusal(dir, source))
}

/// [`ensure_owner_only_dir`]'s `lstat` guard: refuse a symlink at `dir`, and
/// refuse any `lstat` error other than "nothing is there yet".
fn refuse_symlink_at(dir: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(symlink_refusal(dir)),
        Ok(_) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(source),
    }
}

/// The refusal [`refuse_symlink_at`] and [`open_dir_nofollow`] share, so a link
/// caught by the `lstat` and one caught by `O_NOFOLLOW` read identically to an
/// operator — the difference between them is only *when* it was planted.
fn symlink_refusal(dir: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "refusing to use {}: it is a symlink, and creating or chmodding it would \
             follow the link and tighten permissions on another directory — point the \
             path at the real directory instead of a symlink",
            dir.display()
        ),
    )
}

/// Open `dir` as a directory without following a link at its final component.
///
/// The descriptor this returns is what [`ensure_owner_only_dir`] `fchmod`s, so
/// this is the seam the whole race-free claim rests on. A link planted after
/// the `lstat` is refused in the same words the `lstat` would have used, and
/// anything else that is not a directory gets a refusal naming that instead.
///
/// **The two are not distinguishable from the errno**, which is why there is an
/// `lstat` in the classifier and not just a match. `open(2)` documents `ELOOP`
/// for `O_NOFOLLOW` on a symlink, but Linux answers `ENOTDIR` when `O_DIRECTORY`
/// is set as well — measured here, not read: the first version of this function
/// matched `ELOOP` alone and reported a symlink-to-directory as "not a
/// directory". Both errnos therefore go to the same classifier, which `lstat`s
/// the path to choose the wording. That `lstat` decides nothing but the words:
/// either way the call is refused, so a race on it cannot change an outcome.
///
/// `EACCES` is the one arm that goes back to a pathname: a directory we own at
/// a mode with no read bit cannot be opened even by its owner, while `chmod(2)`
/// by path still works on it — that is the pre-existing `0o000` repair the old
/// comment on [`ensure_owner_only_dir`] cited as the reason not to use
/// descriptors at all. It is guarded by a fresh `lstat` and re-verified by
/// re-opening, and [`ensure_owner_only_dir`]'s doc states the residual.
fn open_dir_nofollow(dir: &Path) -> std::io::Result<std::fs::File> {
    match open_dir_nofollow_once(dir) {
        Ok(handle) => Ok(handle),
        Err(source) if is_link_or_not_a_directory(&source) => Err(not_a_usable_directory(dir)),
        Err(source) if source.kind() == std::io::ErrorKind::PermissionDenied => {
            use std::os::unix::fs::PermissionsExt;
            refuse_symlink_at(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|source| chmod_refusal(dir, source))?;
            open_dir_nofollow_once(dir).map_err(|source| {
                if is_link_or_not_a_directory(&source) {
                    not_a_usable_directory(dir)
                } else {
                    source
                }
            })
        }
        Err(source) => Err(source),
    }
}

/// The two errnos an `O_NOFOLLOW|O_DIRECTORY` open reports for "the entry is
/// there but it is not a directory we will follow into". See
/// [`open_dir_nofollow`] for why they are one case and not two.
fn is_link_or_not_a_directory(source: &std::io::Error) -> bool {
    matches!(
        source.raw_os_error(),
        Some(libc::ELOOP) | Some(libc::ENOTDIR)
    )
}

/// The refusal for an entry [`open_dir_nofollow`] would not open: a symlink
/// gets [`symlink_refusal`]'s wording, anything else gets its own.
fn not_a_usable_directory(dir: &Path) -> std::io::Error {
    let is_symlink = std::fs::symlink_metadata(dir)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false);
    if is_symlink {
        return symlink_refusal(dir);
    }
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "refusing to use {}: it exists and is not a directory — remove or rename it \
             if it is stale, or point the relevant DOT_AGENT_DECK_* path override \
             somewhere else",
            dir.display()
        ),
    )
}

/// The raw open [`open_dir_nofollow`] classifies the failures of.
fn open_dir_nofollow_once(dir: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(dir)
}

/// Turn [`ensure_owner_only_dir`]'s `set_permissions` failure into something an
/// operator can act on (issue #1121).
///
/// The reachable case that matters is the one the fail-closed behaviour exists
/// for: the directory already exists and **another uid owns it**, so
/// `chmod(2)` returns `EPERM` for us. Unwrapped that reads `Operation not
/// permitted` with no path, no owner and no remedy — three lookups away from
/// being actionable. The wrapper stats the directory and, when the owner is
/// not us, says so and names both uids; anything else keeps the original
/// wording and only gains the path.
///
/// The stat is best-effort and deliberately not fail-closed: it runs only to
/// *explain* a failure that has already been decided, so a stat that itself
/// errors falls back to the generic arm rather than inventing a reason.
fn chmod_refusal(dir: &Path, source: std::io::Error) -> std::io::Error {
    use std::os::unix::fs::MetadataExt;

    let our_uid = crate::platform::paths::current_uid();
    let owner_uid = std::fs::metadata(dir).ok().map(|metadata| metadata.uid());
    match owner_uid {
        Some(owner_uid) if owner_uid != our_uid => std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            foreign_owner_refusal(dir, owner_uid, our_uid, &source),
        ),
        _ => std::io::Error::new(
            source.kind(),
            format!("could not set mode 0o700 on {}: {source}", dir.display()),
        ),
    }
}

/// The message [`chmod_refusal`] renders for a directory another uid owns.
///
/// Pure data, split out for the same reason as [`endpoint_uid_is_trusted`]: the
/// arm it describes needs a directory owned by a second account, which a test
/// process cannot create without one or without root, so the *wording* is what
/// is testable on any host.
fn foreign_owner_refusal(
    dir: &Path,
    owner_uid: u32,
    our_uid: u32,
    source: &std::io::Error,
) -> String {
    format!(
        "refusing to use {}: it is owned by uid {owner_uid} (we are uid {our_uid}), so it cannot \
         be made owner-only for us and whatever we put inside it would sit in another user's \
         directory ({source}) — remove or rename it if it is stale, or point the relevant \
         DOT_AGENT_DECK_* path override at a directory only you can write",
        dir.display()
    )
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

/// Why [`verify_endpoint_trusted`] refuses a symlink at the endpoint path
/// (issue #1020). A named constant rather than an inline literal so the tests
/// can assert it exactly, which is what pins that the symlink clause runs
/// *before* the type and uid clauses — a link to a regular file otherwise
/// refuses for being a regular file and the test still passes.
const SYMLINK_REFUSAL: &str = "it is a symlink, so its own owner and mode decide nothing — an \
                               endpoint must be the socket inode itself, not a link to one";

/// The pure-data core of [`verify_endpoint_trusted`]'s ownership clause: an
/// endpoint is trusted only when its owning uid is exactly ours.
///
/// Split out for the same reason as the Windows SID analogue
/// [`super::endpoint_owner_is_trusted`] and
/// [`crate::config::config_owner_is_trusted`] — the *refusal* arm needs a
/// socket owned by another account, which a test process cannot create without
/// a second account or root. As pure data the rule is exhaustively testable on
/// any host, which is what pins the foreign-uid denial (PRD #741 M1).
///
/// Fails closed on any mismatch in either direction: uid 0 is not a wildcard
/// on either side, and the error names both uids so an operator can see who
/// squatted the endpoint.
fn endpoint_uid_is_trusted(owner_uid: u32, our_uid: u32) -> Result<(), String> {
    if owner_uid != our_uid {
        return Err(format!("owned by uid {owner_uid} (expected {our_uid})"));
    }
    Ok(())
}

/// Verify `path` is a Unix socket owned by the current uid at mode 0o600.
/// Returns `Err(reason)` describing the first failed check; the caller wraps it
/// in its own error type.
///
/// Defends against a same-uid attacker pre-creating a socket at the attach path
/// before the real daemon binds: in that scenario `bind(2)` fails with
/// `EADDRINUSE` for the daemon and `connect(2)` succeeds for us against the
/// attacker's socket. Validating ownership and mode out-of-band closes the gap.
///
/// **The stat is NOT an anchor, and this comment used to say it was** (issue
/// #1121 round two). The wording was "the FD we then connect to is anchored to
/// the inode the kernel resolves during this single call", and that is false:
/// the `lstat` here and the `connect(2)` a caller makes afterwards are two
/// separate pathname resolutions, and a probe that connects and drops its
/// stream before the real connect makes a third. What is actually true is
/// narrower, and still worth having:
///
/// - A **stable** foreign entry — a socket, link or file another uid planted
///   and left there — is refused, and refused without a `connect(2)` being
///   attempted or the entry being touched.
/// - In a **sticky** directory a foreign uid cannot replace a *live*
///   victim-owned inode, because only the entry's owner may unlink it. The
///   reachable replacement window is the one where the name is genuinely
///   free: an old daemon unlinks its socket during shutdown, and a foreign
///   process binds a permissive listener before the next check.
///
/// That window is closed one layer down rather than here, by the peer-uid
/// refusal welded into every Unix connect entry point
/// ([`crate::platform::ipc`]'s `refuse_foreign_peer`) — a credential the
/// kernel records for the connection itself, which no amount of care with a
/// pathname can substitute for. This check still runs first and still earns
/// its place: it is what keeps the deck from connecting to a foreign entry at
/// all, rather than connecting and then dropping the stream.
///
/// **The stat is an `lstat`, so a symlink at the endpoint path is refused on its
/// own account and its target decides nothing** (issue #1020). It used to be
/// `std::fs::metadata`, which reads *through* a link: the owner and mode tested
/// were the target's, so a link owned by anyone at all was accepted as long as
/// it pointed at some socket of ours. Same trade, same reasoning as
/// [`ensure_owner_only_dir`]'s refusal — refuse the link rather than try to
/// distinguish a planted one from a deliberate one.
///
/// **What that is worth depends on the directory, and it is worth nothing under
/// the premise the paragraph above states.** A same-uid attacker who can plant a
/// link can bind a real `0o600` socket of their own at the same path instead, so
/// against *that* actor this clause adds no refusal. What it closes is the
/// foreign-uid case the `/tmp` fallback in
/// [`crate::platform::paths::attach_socket_path`] opens: with `XDG_RUNTIME_DIR`
/// unset — an ordinary ssh session or a container — the endpoint lands in a
/// world-writable directory, where another uid can create a symlink they own
/// and, before this, have it accepted. Under `XDG_RUNTIME_DIR` (mode `0o700`,
/// ours) no foreign uid can create the entry in the first place.
///
/// Only the **final** component is lstat'd; a symlinked ancestor is resolved as
/// usual, which is deliberate — macOS's `/tmp` is a symlink to `/private/tmp`
/// and a `$TMPDIR` or `$XDG_RUNTIME_DIR` under one is ordinary. The property is
/// "the endpoint is the inode we check", not "no link is involved in reaching
/// it".
pub fn verify_endpoint_trusted(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let metadata =
        std::fs::symlink_metadata(path).map_err(|source| format!("stat failed: {source}"))?;

    // Ahead of the type clause so the reason names the link. Reported as "not a
    // Unix domain socket" it would be true but actively misleading about a path
    // that does resolve to one.
    if metadata.file_type().is_symlink() {
        return Err(SYMLINK_REFUSAL.to_string());
    }

    if !metadata.file_type().is_socket() {
        return Err("not a Unix domain socket".to_string());
    }

    endpoint_uid_is_trusted(metadata.uid(), crate::platform::paths::current_uid())?;

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
            .expect("stat the path")
            .permissions()
            .mode()
            & 0o777
    }

    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }

    /// A `tempfile::tempdir()` whose mode is restated after creation.
    ///
    /// [`with_socket_umask`] raises the **process** mask to `0o177` for the
    /// duration of a bind, and its own doc records what that costs a
    /// concurrent thread: a `tempfile::tempdir()` created in that window comes
    /// out at `0o600`, with no search bit, and every later write inside it
    /// fails `EACCES`. `cargo test` runs this module's tests as threads in one
    /// process and this module is where the mask is raised, so the two meet
    /// here more than anywhere else — measured while round two of issue #1121
    /// was adding three tests to it, as `ensure_owner_only_dir_still_creates_and_repairs_real_dirs`
    /// failing with `Permission denied` roughly one run in three.
    ///
    /// Restating the mode is the cheap, local fix. It is not a fix for the
    /// window itself, which cannot be closed from this side: nothing makes an
    /// unrelated thread's directory creation take [`UMASK_LOCK`].
    fn sandbox() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("tempdir");
        chmod(root.path(), 0o700);
        root
    }

    /// Issue #1121: the wording [`chmod_refusal`] renders when the directory
    /// belongs to another uid. Everything an operator needs to act is in one
    /// line — which directory, who owns it, who we are, and the way out — where
    /// before there was a bare `Operation not permitted`.
    ///
    /// This asserts the **message**, not the branch. Reaching the branch needs
    /// a directory owned by a second account, which a test process cannot
    /// create without one or without root; the wording is the part that is
    /// testable on any host, exactly as for [`endpoint_uid_is_trusted`].
    #[test]
    fn the_foreign_owner_refusal_names_the_directory_the_owner_and_the_way_out() {
        let source = std::io::Error::from_raw_os_error(libc::EPERM);
        let rendered =
            foreign_owner_refusal(Path::new("/tmp/dot-agent-deck-4242"), 4242, 1000, &source);

        assert!(rendered.contains("/tmp/dot-agent-deck-4242"), "{rendered}");
        assert!(rendered.contains("uid 4242"), "{rendered}");
        assert!(rendered.contains("uid 1000"), "{rendered}");
        assert!(
            rendered.contains("DOT_AGENT_DECK_"),
            "the refusal must name the escape hatch: {rendered}"
        );
        assert!(
            rendered.contains(&source.to_string()),
            "the underlying errno must survive, so an unexpected cause is still \
             readable: {rendered}"
        );
    }

    /// The other arm of [`chmod_refusal`], and the one that is reachable
    /// in-test: a `set_permissions` failure with no foreign owner behind it
    /// keeps the original wording and gains the path.
    ///
    /// Driven with a path that does not exist, which is what
    /// `ensure_owner_only_dir` hands the chmod when its own `mkdir` produced
    /// nothing — the shape `state_dir`'s empty-override guard records having
    /// measured as `ENOENT`.
    #[test]
    fn a_chmod_failure_with_no_foreign_owner_keeps_its_own_reason_and_gains_the_path() {
        let root = sandbox();
        let absent = root.path().join("absent");
        let err = chmod_refusal(&absent, std::io::Error::from_raw_os_error(libc::ENOENT));

        assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{err}");
        let rendered = err.to_string();
        assert!(
            rendered.contains(&absent.display().to_string()),
            "the path is the thing the bare errno was missing: {rendered}"
        );
        assert!(rendered.contains("0o700"), "{rendered}");
    }

    /// A same-uid attacker plants a symlink at the path the deck is about to use
    /// for its state dir / lock root, pointing at a directory of their choosing.
    /// `ensure_owner_only_dir` must refuse the path outright rather than follow
    /// it and chmod 0o700 onto the attacker's target.
    #[test]
    fn ensure_owner_only_dir_refuses_a_symlinked_target() {
        let root = sandbox();
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
        let root = sandbox();
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
        let root = sandbox();

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

    /// Issue #1121 round two: the symlink a foreign uid plants *after* the
    /// `lstat` guard has run. That window cannot be opened from a test — the
    /// point is that there is no longer a second pathname resolution to race —
    /// so this drives [`open_dir_nofollow`], the seam the whole claim rests on,
    /// against a link directly. `O_NOFOLLOW` must answer `ELOOP`, the refusal
    /// must read like the `lstat` one, and the target must keep its mode.
    #[test]
    fn the_descriptor_open_refuses_a_symlink_and_leaves_its_target_alone() {
        let root = sandbox();
        let victim = root.path().join("victim");
        std::fs::create_dir(&victim).expect("create the victim directory");
        chmod(&victim, 0o755);

        let planted = root.path().join("endpoints");
        std::os::unix::fs::symlink(&victim, &planted).expect("plant the symlink");

        let err = open_dir_nofollow(&planted).expect_err("O_NOFOLLOW must refuse a symlink");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
        assert!(err.to_string().contains("symlink"), "{err}");
        assert_eq!(
            mode_of(&victim),
            0o755,
            "nothing may be chmodded through the link"
        );
    }

    /// The `0o000` repair the old comment cited as the reason not to use a
    /// descriptor at all. A directory we own with no read bit cannot be opened
    /// even by its owner, so the `EACCES` arm falls back to a path chmod and
    /// then re-opens — and the outcome the caller sees must be the same `0o700`
    /// it always was.
    #[test]
    fn ensure_owner_only_dir_still_repairs_a_directory_it_cannot_open() {
        let root = sandbox();
        let unreadable = root.path().join("unreadable");
        std::fs::create_dir(&unreadable).expect("create the directory");
        chmod(&unreadable, 0o000);
        assert!(
            std::fs::read_dir(&unreadable).is_err(),
            "the premise of this test is a directory its owner cannot open"
        );

        ensure_owner_only_dir(&unreadable).expect("repair an unreadable directory");
        assert_eq!(mode_of(&unreadable), 0o700);
    }

    /// A regular file sitting at the directory's name. Before the descriptor
    /// work this reached a path-based `chmod` and turned that file owner-only;
    /// now `O_DIRECTORY` answers `ENOTDIR` and the refusal names the path.
    #[test]
    fn ensure_owner_only_dir_refuses_a_regular_file_at_the_path() {
        let root = sandbox();
        let planted = root.path().join("endpoints");
        std::fs::write(&planted, b"not a directory").expect("plant a regular file");
        chmod(&planted, 0o644);

        let err = ensure_owner_only_dir(&planted).expect_err("a regular file must be refused");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
        assert!(
            err.to_string().contains(&planted.display().to_string()),
            "the refusal must name the path: {err}"
        );
        assert_eq!(
            mode_of(&planted),
            0o644,
            "the planted file must not be chmodded"
        );
        assert_eq!(
            std::fs::read(&planted).expect("the planted file survives"),
            b"not a directory"
        );
    }

    /// Issue #669's scope note, mechanized: the sibling `create_owner_only_dir`
    /// does **not** share the permission-tightening exposure, because it never
    /// chmods — `DirBuilder`'s mode applies only to a directory the call itself
    /// created, and a symlink at the target means it creates nothing (PRD #127
    /// S2 deliberately leaves a pre-existing directory's mode alone). Whatever
    /// the call returns, the attacker's target keeps its mode.
    #[test]
    fn create_owner_only_dir_never_tightens_through_a_symlink() {
        let root = sandbox();
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

    /// Bind a real Unix socket at `path` and leave it at mode 0o600 — the state
    /// `platform::ipc::IpcListener::bind` produces, reached through that path's
    /// second half ([`set_endpoint_mode_owner_only`]) rather than its first.
    /// The returned listener must be kept alive for the test's duration.
    ///
    /// Deliberately does **not** go through [`with_socket_umask`]: umask is
    /// process-global and its own doc says the lock serializes only cooperating
    /// callers, so borrowing it here would set 0o177 for the whole test binary
    /// for the duration of the `bind`. Measured — every `tempfile::tempdir()`
    /// created by a parallel test in that window lands at 0o600 with no search
    /// bit, and four tests here died with EACCES inside their own temp roots.
    /// The restate is deterministic under any ambient umask and perturbs
    /// nothing, which is what a fixture wants.
    fn bind_trusted_socket(path: &Path) -> std::os::unix::net::UnixListener {
        let listener = std::os::unix::net::UnixListener::bind(path).expect("bind the socket");
        set_endpoint_mode_owner_only(path).expect("restate 0o600 on the socket inode");
        listener
    }

    /// PRD #741 M1, the accept arm: a real `UnixListener` bound the way the
    /// daemon binds it — our uid, mode exactly 0o600 — must be trusted.
    ///
    /// This is also what pins the `& 0o777` mask in the mode clause: a socket's
    /// raw `st_mode` is `0o140600`, so a refactor that dropped the mask would
    /// compare `0o140600 != 0o600` and refuse *every* endpoint, including this
    /// one. And it pins that `endpoint_uid_is_trusted` is really wired in —
    /// inverting that comparison refuses our own socket here.
    #[test]
    fn verify_endpoint_trusted_accepts_our_own_0o600_socket() {
        let root = sandbox();
        let endpoint = root.path().join("attach.sock");
        let _listener = bind_trusted_socket(&endpoint);

        assert_eq!(
            mode_of(&endpoint),
            0o600,
            "the fixture must land the inode at exactly 0o600"
        );
        verify_endpoint_trusted(&endpoint).expect("our own 0o600 socket must be trusted");
    }

    /// The mode clause is **exact equality** against 0o600, not a mask, so a
    /// *tighter* mode is refused along with every looser one. That is
    /// surprising enough to pin deliberately: a refactor to `mode & 0o077 != 0`
    /// ("nothing for group or other") would keep accepting 0o400 and 0o000 and
    /// silently pass a test that only exercised the loose direction.
    #[test]
    fn verify_endpoint_trusted_refuses_every_mode_but_exactly_0o600() {
        let root = sandbox();
        let endpoint = root.path().join("attach.sock");
        let _listener = bind_trusted_socket(&endpoint);

        for mode in [
            0o644, // the classic "arrived under the ambient umask" shape
            0o755, // …and the other one
            0o660, // group-readable: another account in our group could connect
            0o666, // world-writable
            0o601, // a single other-bit is still a leak
            0o700, // one bit *added* over 0o600, and still refused
            0o400, // strictly tighter than 0o600 — refused, not accepted
            0o000, // and so is the completely locked-down inode
        ] {
            chmod(&endpoint, mode);
            let Err(err) = verify_endpoint_trusted(&endpoint) else {
                panic!("mode 0o{mode:o} must be refused");
            };
            assert_eq!(err, format!("mode is 0o{mode:o} (expected 0o600)"));
        }

        // The control: restored to exactly 0o600 it is trusted again, so the
        // loop above is pinning the mode clause rather than something the first
        // chmod broke for good.
        chmod(&endpoint, 0o600);
        verify_endpoint_trusted(&endpoint).expect("restored to 0o600, it is trusted again");
    }

    /// The type clause: anything at the path that is not a Unix domain socket
    /// is refused, and it is refused *first* — a regular file at a wrong mode
    /// reports the type, not the mode — so the precedence of the three checks
    /// is pinned along with the checks themselves.
    #[test]
    fn verify_endpoint_trusted_refuses_a_path_that_is_not_a_socket() {
        use std::os::unix::ffi::OsStrExt;

        let root = sandbox();

        // A regular file at exactly the mode a socket would be accepted at, so
        // the refusal can only be coming from the file-type clause.
        let plain = root.path().join("plain");
        std::fs::write(&plain, b"").expect("create the regular file");
        chmod(&plain, 0o600);
        assert_eq!(
            verify_endpoint_trusted(&plain).expect_err("a regular file must be refused"),
            "not a Unix domain socket"
        );

        // …and a regular file at a *wrong* mode still reports the type, which
        // is what pins the check order.
        chmod(&plain, 0o644);
        assert_eq!(
            verify_endpoint_trusted(&plain).expect_err("a regular file must be refused"),
            "not a Unix domain socket",
            "the type clause must be reported before the mode clause"
        );

        // A FIFO: the special file closest to a socket, and the one a check
        // asking "is it special?" rather than "is it a socket?" would let
        // through. `metadata` only stats it, so nothing blocks on a reader.
        let fifo = root.path().join("fifo");
        let c_fifo = std::ffi::CString::new(fifo.as_os_str().as_bytes()).expect("CString");
        // SAFETY: `mkfifo(3)` reads a NUL-terminated path and a mode; `c_fifo`
        // outlives the call and there is nothing returned to own.
        let rc = unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo: {}", std::io::Error::last_os_error());
        assert_eq!(
            verify_endpoint_trusted(&fifo).expect_err("a FIFO must be refused"),
            "not a Unix domain socket"
        );

        // A directory, likewise. Restored to 0o700 afterwards so the tempdir
        // can still clean itself up.
        let dir = root.path().join("dir");
        std::fs::create_dir(&dir).expect("create the directory");
        chmod(&dir, 0o600);
        assert_eq!(
            verify_endpoint_trusted(&dir).expect_err("a directory must be refused"),
            "not a Unix domain socket"
        );
        chmod(&dir, 0o700);
    }

    /// Nothing at the path at all: the `stat` failure is surfaced verbatim
    /// rather than flattened into a generic refusal, so an operator can tell
    /// "the daemon never bound" apart from "something untrusted is squatting
    /// the path".
    #[test]
    fn verify_endpoint_trusted_refuses_a_path_with_nothing_at_it() {
        let root = sandbox();

        let missing = root.path().join("never-bound.sock");
        let err = verify_endpoint_trusted(&missing).expect_err("an absent endpoint is refused");
        assert!(err.starts_with("stat failed:"), "{err}");
        assert!(err.contains("os error 2"), "ENOENT must be named: {err}");

        // A dangling symlink used to read as absent, because `metadata`
        // followed links and the `ENOENT` came from the target. Since issue
        // #1020 it reads as what it is — something squatting the path — which
        // is the distinction this test exists to draw. The symlink test below
        // owns the assertion; here we only pin that it is no longer filed under
        // "the daemon never bound".
        let dangling = root.path().join("dangling.sock");
        std::os::unix::fs::symlink(root.path().join("nowhere"), &dangling)
            .expect("plant the dangling symlink");
        let err = verify_endpoint_trusted(&dangling).expect_err("a dangling symlink is refused");
        assert!(
            !err.starts_with("stat failed:"),
            "a planted link must not be reported as an absent endpoint: {err}"
        );
        assert!(err.contains("symlink"), "{err}");
    }

    /// Issue #1020: a symlink at the endpoint path is refused, and the socket it
    /// points at is **not** what decides.
    ///
    /// This inverts the `…_stats_through_a_symlink_to_a_trusted_socket`
    /// characterization test PRD #741 M1 left here to make exactly this change
    /// go red deliberately. The old body asserted that a
    /// link to a perfectly trusted socket was itself trusted; under the `/tmp`
    /// fallback that link can be owned by another uid, so accepting it hands
    /// our client to an endpoint a stranger chose.
    ///
    /// The strong form of the property is the *second* half: the same target,
    /// named directly, is still trusted. A refusal that also broke the real
    /// socket would be a regression wearing a fix's clothes.
    #[test]
    fn verify_endpoint_trusted_refuses_a_symlink_to_a_trusted_socket() {
        let root = sandbox();
        let real = root.path().join("real.sock");
        let _listener = bind_trusted_socket(&real);

        let link = root.path().join("link.sock");
        std::os::unix::fs::symlink(&real, &link).expect("plant the symlink");

        let err = verify_endpoint_trusted(&link)
            .expect_err("a symlink must be refused however trusted its target is");
        assert!(
            err.contains("symlink"),
            "the reason must name the link, not the target: {err}"
        );

        // The target is untouched and still reachable by its own name — the
        // refusal is about the link, not about the socket.
        verify_endpoint_trusted(&real).expect("the real socket is still trusted by its own name");
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("lstat the planted link")
                .file_type()
                .is_symlink(),
            "verification must not mutate the path it refuses"
        );
    }

    /// …and the target decides nothing *at all*, which is the property that
    /// makes the refusal worth having. Every shape of target — a trusted
    /// socket, a regular file, a directory, nothing — produces the same single
    /// refusal, so no attacker-controlled target can steer which clause fires.
    ///
    /// Without this, a `symlink_metadata` that had been placed *after* the type
    /// or uid clause would still pass the test above while reporting the
    /// target's business for three of these four cases.
    #[test]
    fn verify_endpoint_trusted_refuses_a_symlink_whatever_it_points_at() {
        let root = sandbox();

        let socket = root.path().join("real.sock");
        let _listener = bind_trusted_socket(&socket);
        let plain = root.path().join("plain");
        std::fs::write(&plain, b"").expect("write the plain file");
        chmod(&plain, 0o600);
        let dir = root.path().join("dir");
        std::fs::create_dir(&dir).expect("create the directory");
        let nowhere = root.path().join("nowhere");

        for (label, target) in [
            ("a trusted socket", &socket),
            ("a 0o600 regular file", &plain),
            ("a directory", &dir),
            ("nothing at all", &nowhere),
        ] {
            let link = root
                .path()
                .join(format!("link-{}", label.replace(' ', "-")));
            std::os::unix::fs::symlink(target, &link).expect("plant the symlink");
            assert_eq!(
                verify_endpoint_trusted(&link)
                    .expect_err(&format!("a symlink to {label} must be refused")),
                SYMLINK_REFUSAL,
                "a symlink to {label} must refuse for being a symlink, not for what it points at"
            );
        }
    }

    /// The legitimate case that a clumsier fix would break: only the **final**
    /// component is lstat'd, so a socket reached through a symlinked *ancestor*
    /// is still trusted.
    ///
    /// This is not a hypothetical. macOS's `/tmp` is a symlink to
    /// `/private/tmp`, and the per-user `$TMPDIR` that `tempfile` uses there
    /// lives under `/var`, itself a symlink to `/private/var` — so on the
    /// `build-macos` runner the sibling tests here are *already* resolving
    /// through links. An implementation that reached for `canonicalize` and a
    /// path comparison, or `O_NOFOLLOW` over the whole path, would refuse every
    /// endpoint on that platform and pass every test that only plants a link at
    /// the endpoint itself.
    #[test]
    fn verify_endpoint_trusted_accepts_a_socket_under_a_symlinked_ancestor() {
        let root = sandbox();
        let real_dir = root.path().join("real-dir");
        std::fs::create_dir(&real_dir).expect("create the real directory");
        let endpoint = real_dir.join("attach.sock");
        let _listener = bind_trusted_socket(&endpoint);

        let via_link = root.path().join("linked-dir");
        std::os::unix::fs::symlink(&real_dir, &via_link).expect("symlink the parent directory");

        verify_endpoint_trusted(&via_link.join("attach.sock"))
            .expect("a symlinked ancestor is ordinary and must not be refused");
    }

    /// The foreign-uid denial, at the level where it is decidable without a
    /// second account — the Unix mirror of
    /// `endpoint_owner_trust_accepts_only_our_own_sid` in the parent module.
    /// The only accepted case is "the endpoint's owner uid is exactly ours".
    #[test]
    fn endpoint_uid_trust_accepts_only_our_own_uid() {
        endpoint_uid_is_trusted(1000, 1000).expect("our own uid must be trusted");
        endpoint_uid_is_trusted(0, 0).expect("a root deck's own root-owned socket is still ours");
        endpoint_uid_is_trusted(u32::MAX, u32::MAX).expect("the value itself decides nothing");
    }

    /// …and everything else is refused, with both uids named in the reason so
    /// the operator can see who squatted the endpoint. The mirror of
    /// `endpoint_owner_trust_refuses_a_foreign_or_missing_owner`.
    #[test]
    fn endpoint_uid_trust_refuses_a_foreign_uid() {
        let err =
            endpoint_uid_is_trusted(0, 1000).expect_err("a root-owned socket must be refused");
        assert!(err.contains("uid 0"), "the squatter must be named: {err}");
        assert!(err.contains("expected 1000"), "we must be named: {err}");

        // A one-digit difference is still a different account.
        assert!(endpoint_uid_is_trusted(1001, 1000).is_err());
        // Refused in the other direction too: running as root does not make
        // another user's socket ours.
        assert!(endpoint_uid_is_trusted(1000, 0).is_err());
        // uid 0 is a wildcard on neither side.
        assert!(endpoint_uid_is_trusted(u32::MAX, 1000).is_err());
        assert!(endpoint_uid_is_trusted(0, u32::MAX).is_err());
    }

    /// The umask restore runs on an unwind, not only on the ordinary return
    /// (PRD #742 M11's F4). No caller hands this a panicking body today; what
    /// the guard removes is the dependence on that staying true, because the
    /// failure mode is process-wide and silent — the mask stays at `0o177` for
    /// the life of the process and every later file creation loses its mode
    /// bits, which is the `EACCES` cascade `bind_trusted_socket`'s comment
    /// records measuring.
    ///
    /// Reads the mask the only way `umask(2)` offers: swap a value in and put
    /// it straight back. Sound here for the same reason the rest of this
    /// module's process-global work is — nextest is process-per-test.
    #[test]
    fn a_panicking_body_still_restores_the_process_umask() {
        fn current() -> libc::mode_t {
            // SAFETY: `umask(2)` swaps a per-process value and cannot fail;
            // the second call puts back what the first read.
            unsafe {
                let prev = libc::umask(0o022);
                libc::umask(prev);
                prev
            }
        }

        let before = current();
        // The default hook would print a backtrace for a panic this test is
        // deliberately causing, which reads as a failure in the log of a
        // passing run.
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let caught = std::panic::catch_unwind(|| {
            let _: () = with_socket_umask(|| panic!("the body this test hands in"));
        });
        std::panic::set_hook(hook);

        assert!(
            caught.is_err(),
            "the panic must propagate to the caller, not be swallowed by the guard"
        );
        assert_eq!(
            current(),
            before,
            "a body that unwound must still leave the process umask where it found it"
        );
    }
}
