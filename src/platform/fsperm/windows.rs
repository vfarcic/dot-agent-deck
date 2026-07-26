//! Windows filesystem & object security (PRD #163 M4 — the release-gating
//! milestone that replaces #42's justified no-ops with real enforcement).
//!
//! The uniform Unix mode-bit model (`umask` + `0o700`/`0o600` + a `stat`-based
//! owner/mode trust check) maps onto three Win32 mechanisms, all built here so
//! the policy lives in one auditable file:
//!
//! | Unix | Windows |
//! |---|---|
//! | socket inode at `0o600` (umask-before-`bind`) | a **pipe security descriptor** with an explicit owner (`O:`) and a protected current-user-only DACL, applied to *every* pipe instance at creation ([`pipe_security_descriptor`]) |
//! | `verify_socket_trusted`: `stat` says uid == ours, mode == `0o600` | **server-SID verification** on the connected pipe handle ([`verify_pipe_server_is_current_user`]), run by both client entry points before a single byte is written |
//! | `0o700` dirs, `0o600` files | a protected current-user-only **DACL** on the directory / on the open file handle ([`set_file_owner_only`], [`ensure_owner_only_dir`]) |
//!
//! ### Why the DACL is built from SDDL
//!
//! `D:P(A;;<rights>;;;<sid>)` is a *protected* DACL (`P` blocks inheritance from
//! the parent container) holding exactly one allow-ACE, for exactly one SID. That
//! is the closest thing Win32 has to "0o600, and nothing the parent directory says
//! can loosen it" — the property a `DOT_AGENT_DECK_STATE_DIR` /
//! `DOT_AGENT_DECK_REMOTES` / `DOT_AGENT_DECK_SCHEDULES` override into a
//! world-writable directory would otherwise destroy. Hand-assembling an `ACL`
//! buffer would be the same bytes with more `unsafe`; the SDDL parser is in
//! `advapi32` and is the documented way to spell this.
//!
//! ### Everything fails closed
//!
//! Every function here returns an error rather than degrading to the default
//! (`Everyone`-readable) security descriptor: a silent fallback is exactly the
//! hole #42 left open. The SID comes from
//! [`crate::platform::paths::current_user_sid`] — the same cached token-derived
//! value the pipe *names* and the spawn mutex are namespaced by, so an object's
//! name and its ACL can never disagree about which user owns it.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT, SE_KERNEL_OBJECT, SetNamedSecurityInfoW, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
};

/// Security-descriptor string for a **named pipe** instance: we are the owner and
/// the only principal in a protected DACL.
///
/// - `O:{sid}` pins the object owner instead of leaving it to the token default.
///   That is what makes [`verify_pipe_server_is_current_user`] a reliable check:
///   the client compares the pipe's owner SID against its own, and Windows only
///   lets a process set an object's owner to a SID it already holds (an arbitrary
///   owner needs `SeRestorePrivilege`), so a foreign local user *cannot* forge a
///   pipe that looks like ours.
/// - `D:P` — protected DACL, no inherited ACEs.
/// - `(A;;GA;;;{sid})` — one allow-ACE granting `GENERIC_ALL`, which the file
///   generic mapping expands to `FILE_ALL_ACCESS`. That deliberately includes
///   `FILE_CREATE_PIPE_INSTANCE`, the right the daemon needs to create the *next*
///   instance on every accept.
///
/// No `Everyone`/`Users`/`SYSTEM` ACE: the Unix socket this replaces was `0o600`,
/// and the residual (a `SYSTEM`-level actor can take ownership) is the same
/// residual `root` has on Unix.
fn pipe_sddl(sid: &str) -> String {
    format!("O:{sid}D:P(A;;GA;;;{sid})")
}

/// Security-descriptor string for a **file or directory** DACL. Only the `D:`
/// clause is used — [`SetSecurityInfo`] / [`SetNamedSecurityInfoW`] are called
/// with `DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION`, so the
/// owner stays whatever the creating token made it and only the DACL is replaced.
///
/// `FA` (`FILE_ALL_ACCESS`) rather than `GA`: these are real file-system objects,
/// so the concrete file rights are what the ACL editor and `icacls` will show, and
/// what an inherited-ACE diff is readable against.
fn file_sddl(sid: &str) -> String {
    format!("D:P(A;;FA;;;{sid})")
}

/// No umask on Windows — a named pipe has no inode-mode race, so there is no
/// window between "endpoint exists" and "endpoint is owner-only" to close. The
/// pipe's security descriptor ([`pipe_security_descriptor`]) is supplied to
/// `CreateNamedPipeW` itself, which is strictly stronger than the Unix
/// umask-before-`bind` dance: the object never exists with any other DACL.
///
/// Justified no-op (PRD #163 Edge Cases: every `cfg(unix)` permission site gets a
/// Windows counterpart *or* a justified no-op). Runs `f` directly.
pub fn with_socket_umask<T>(f: impl FnOnce() -> T) -> T {
    f()
}

/// Create `dir` (recursively) **and unconditionally re-apply** a protected
/// current-user-only DACL — the counterpart of the Unix
/// `DirBuilder::mode(0o700)` + follow-up `set_permissions(0o700)`.
///
/// The unconditional re-apply is the whole point of this variant: a directory
/// left behind by a stale install (or created by hand under a permissive parent)
/// would otherwise keep an inherited `Users`-readable ACL. `%LOCALAPPDATA%` is
/// already per-user ACL'd, so in the default deployment this is defense in depth;
/// under a `DOT_AGENT_DECK_STATE_DIR` / `DOT_AGENT_DECK_LOCK_DIR` override into a
/// shared directory it is the only thing standing between the daemon log (which
/// carries hook payloads and agent task strings) and every local account.
pub fn ensure_owner_only_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    apply_owner_only_dacl_to_path(dir)
}

/// Create `dir` (recursively) with a protected current-user-only DACL, **without**
/// re-applying it to a pre-existing directory — the counterpart of the Unix
/// `DirBuilder::mode(0o700)`-only variant, which likewise leaves an existing
/// directory's mode alone so we never surprise-tighten a directory we did not
/// make (PRD #127 S2). Used by the `schedules.toml` atomic-write path.
///
/// "Did we create it?" is answered by probing first. `DirBuilder`'s mode has the
/// same shape of race on Unix (the kernel resolves it, we do it in user space), and
/// erring either way is benign: losing the probe means we skip a tightening we
/// would have been allowed to do, never that we loosen anything.
pub fn create_owner_only_dir(dir: &Path) -> io::Result<()> {
    let preexisting = dir.is_dir();
    std::fs::create_dir_all(dir)?;
    if preexisting {
        return Ok(());
    }
    apply_owner_only_dacl_to_path(dir)
}

/// No-op **by necessity**, not by choice: `std::fs::OpenOptions` has no hook for
/// a `SECURITY_ATTRIBUTES`, so there is no Windows analogue of Unix's
/// `OpenOptionsExt::mode(0o600)` — the create-time half of the owner-only
/// guarantee.
///
/// The guarantee itself is **not** dropped: every caller of this function also
/// calls [`set_file_owner_only`] on the freshly-opened handle, which applies the
/// protected owner-only DACL for real. PRD #163 M4 moved those calls to run
/// *immediately after `open`, before the first content byte is written*, so the
/// window in which a secret-bearing `remotes.toml`/`schedules.toml`/`session.toml`
/// temp file exists under a loosened inherited ACL contains an empty file only.
pub fn set_create_mode_owner_only(_opts: &mut std::fs::OpenOptions) {}

/// Apply a protected current-user-only DACL to an already-open file — the
/// counterpart of the Unix `fchmod(0o600)` re-assert, and (see
/// [`set_create_mode_owner_only`]) the *only* place the owner-only property is
/// actually established on Windows.
///
/// Handle-based [`SetSecurityInfo`] rather than the path-based
/// [`SetNamedSecurityInfoW`]: the handle is anchored to the file we just opened, so
/// there is no second name resolution for an attacker to redirect between the
/// `open` and the ACL write.
pub fn set_file_owner_only(file: &std::fs::File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;

    let dacl = owner_only_dacl(&file_sddl(&current_user_sid()?))?;
    let handle = file.as_raw_handle() as HANDLE;
    // SAFETY: `handle` is the live handle owned by `file` for the duration of this
    // call. `dacl.acl` points into `dacl`'s still-owned security descriptor. The
    // two null SID arguments are the documented "do not change the owner/group"
    // form, consistent with the `*_SECURITY_INFORMATION` flags naming only the
    // DACL. `SetSecurityInfo` copies what it needs and retains no argument.
    let rc = unsafe {
        SetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl.acl,
            ptr::null(),
        )
    };
    win32_error_to_result(rc, "SetSecurityInfo (owner-only file DACL)")
}

/// No-op: a named pipe has no inode mode to restate after the fact. The
/// owner-only property is established *at creation* by the security descriptor
/// [`pipe_security_descriptor`] hands `CreateNamedPipeW`, which is strictly
/// stronger than the Unix post-bind `0o600` restate this mirrors — there is no
/// moment at which the object exists with a weaker DACL.
///
/// Justified no-op (PRD #163 Edge Cases).
pub fn set_endpoint_mode_owner_only(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Out-of-band trust check for a daemon endpoint, mirroring the Unix
/// "`stat` says: a socket, owned by our uid, at mode `0o600`".
///
/// A `\\.\pipe\…` name has nothing to `stat`, so the Windows analogue is to open
/// it as a client and ask the kernel who owns the pipe object. That is exactly
/// what [`crate::platform::ipc::IpcClient::connect`] does before it returns, so
/// this is a connect-and-drop.
///
/// **The production trust boundary is not here.** On Unix this check is
/// out-of-band and the connect that follows is unguarded, so `daemon_attach` has
/// to call it. On Windows verification is welded into *both* client entry points
/// ([`crate::platform::ipc::IpcStream::connect`] and `IpcClient::connect`), so
/// every connection is verified whether or not anyone calls this — fail-closed by
/// construction rather than by remembering. This exists so the seam keeps its
/// meaning on both platforms and any future out-of-band caller gets a real answer
/// instead of `Ok(())`.
pub fn verify_endpoint_trusted(path: &Path) -> Result<(), String> {
    match crate::platform::ipc::IpcClient::connect(path) {
        Ok(_verified_and_dropped) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => Err(err.to_string()),
        Err(err) => Err(format!("cannot open the named pipe: {err}")),
    }
}

// ---------------------------------------------------------------------------
// Pipe security descriptor + server-SID verification (the #163 [BLOCKER])
// ---------------------------------------------------------------------------

/// Build the security descriptor every pipe instance is created with: owner =
/// the current user, protected DACL granting the current user and nobody else.
///
/// Returned as an owning handle because `CreateNamedPipeW` reads the descriptor
/// *during* the call — the caller must keep this alive across it (see
/// [`OwnedSecurityDescriptor::security_attributes`]).
///
/// Fails closed: an unreadable SID or an unparsable SDDL string is an error, never
/// a fallback to the default descriptor (whose DACL grants `Everyone` read access
/// — the pipe-squat hole this closes).
pub fn pipe_security_descriptor() -> io::Result<OwnedSecurityDescriptor> {
    security_descriptor_from_sddl(&pipe_sddl(&current_user_sid()?))
}

/// Verify that the kernel object behind `handle` was created by the current user,
/// by comparing the **object's owner SID** with ours.
///
/// Two callers, both security-critical:
///
/// 1. **The [BLOCKER]'s client half.** Both named-pipe client entry points
///    ([`crate::platform::ipc::IpcStream::connect`] and `IpcClient::connect`) run
///    this on the connected pipe handle **before the first byte is written**, so
///    the direct analogue of Unix's `metadata.uid() != our_uid` check on the
///    socket inode holds on Windows too. A foreign local user who wins the race to
///    `\\.\pipe\dot-agent-deck-<our-sid>-hook` gets a pipe owned by *their* SID,
///    and this refuses it before we leak hook payloads into it or accept a forged
///    reply from it.
/// 2. **The spawn mutex.** [`crate::platform::lock`] runs this when
///    `CreateMutexW` reports the object already existed, which detects a
///    pre-squatted `Global\dot-agent-deck-spawn-…` — the residual PRD #163 M2 left
///    for this milestone. The DACL we install protects an object *we* create; only
///    the owner check catches one we merely opened.
///
/// Sound because Windows will not let a process set an object owner to a SID its
/// token does not hold (an arbitrary owner needs `SeRestorePrivilege`), and because
/// both object kinds are created here with an explicit `O:<our-sid>` clause rather
/// than relying on the token's default owner.
pub fn verify_object_owner_is_current_user(handle: HANDLE) -> Result<(), String> {
    let owner =
        object_owner_sid(handle).map_err(|err| format!("cannot read the owner SID: {err}"))?;
    let ours =
        current_user_sid().map_err(|err| format!("cannot read the current user's SID: {err}"))?;
    super::endpoint_owner_is_trusted(&owner, &ours)
}

/// Owner SID of the kernel object behind `handle`, in canonical string form.
///
/// `SE_KERNEL_OBJECT` (not `SE_FILE_OBJECT`): both callers hold kernel objects (a
/// named-pipe instance, a named mutex). The object type only selects the
/// generic-rights mapping, which is irrelevant when asking for
/// `OWNER_SECURITY_INFORMATION` alone — but it is the honest label.
///
/// Reading the owner needs `READ_CONTROL`, which every caller holds:
/// `GENERIC_READ` expands to `STANDARD_RIGHTS_READ | …`, `STANDARD_RIGHTS_READ`
/// *is* `READ_CONTROL`, and `MUTEX_ALL_ACCESS` includes it too.
fn object_owner_sid(handle: HANDLE) -> io::Result<String> {
    let mut owner: PSID = ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `handle` is a live pipe handle owned by the caller for this call.
    // `owner`/`sd` are valid out-pointers; the nulls are the documented "do not
    // report this component" form. On success the callee `LocalAlloc`s one
    // descriptor (freed by the guard below) with `owner` pointing *into* it, so
    // the SID must be read while the guard is alive.
    let rc = unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut sd,
        )
    };
    win32_error_to_result(rc, "GetSecurityInfo (object owner SID)")?;
    let _sd = OwnedSecurityDescriptor(sd);

    if owner.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the object has no owner SID",
        ));
    }
    sid_to_string(owner)
}

// ---------------------------------------------------------------------------
// Win32 plumbing
// ---------------------------------------------------------------------------

/// The current user's SID string, or an `io::Error` — never a panic and never a
/// fallback. See [`crate::platform::paths::current_user_sid`].
fn current_user_sid() -> io::Result<String> {
    crate::platform::paths::current_user_sid()
}

/// A `LocalAlloc`'d security descriptor, freed on drop. Used both for descriptors
/// we build from SDDL and for the one `GetSecurityInfo` hands back.
pub struct OwnedSecurityDescriptor(PSECURITY_DESCRIPTOR);

impl OwnedSecurityDescriptor {
    /// A `SECURITY_ATTRIBUTES` pointing at this descriptor, for
    /// `CreateNamedPipeW`. Borrowing `self` is what ties the struct's lifetime to
    /// the call: the descriptor must outlive it (the kernel copies the descriptor
    /// into the new object *during* creation, and reads nothing afterwards).
    ///
    /// `bInheritHandle: 0` — the daemon is spawned `DETACHED_PROCESS` with
    /// explicit stdio and nothing should inherit a listening pipe instance.
    pub fn security_attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}

impl Drop for OwnedSecurityDescriptor {
    fn drop(&mut self) {
        if self.0.is_null() {
            return;
        }
        // SAFETY: frees exactly the buffer `advapi32` `LocalAlloc`'d for us
        // (either from SDDL conversion or from `GetSecurityInfo`); `self.0` is
        // never read again.
        unsafe { LocalFree(self.0.cast()) };
    }
}

/// A DACL borrowed out of an owned security descriptor. The `acl` pointer is only
/// valid while `_sd` is alive, which the struct enforces by owning it.
struct OwnedDacl {
    _sd: OwnedSecurityDescriptor,
    acl: *mut ACL,
}

/// Parse `sddl` into a security descriptor.
fn security_descriptor_from_sddl(sddl: &str) -> io::Result<OwnedSecurityDescriptor> {
    let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `wide` is a NUL-terminated UTF-16 SDDL string that outlives the
    // call, `sd` is a valid out-pointer, and a null size pointer is the
    // documented "do not report the size" form. On success the callee hands back
    // a `LocalAlloc`'d descriptor, freed by `OwnedSecurityDescriptor::drop`.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            ptr::null_mut(),
        )
    } == 0
    {
        let err = io::Error::last_os_error();
        return Err(io::Error::new(
            err.kind(),
            format!("cannot build the owner-only security descriptor {sddl:?}: {err}"),
        ));
    }
    Ok(OwnedSecurityDescriptor(sd))
}

/// Parse `sddl` and extract its DACL, keeping the descriptor alive alongside it.
///
/// A descriptor whose DACL is *absent* (`daclpresent == 0`) or NULL would mean
/// "grant everyone everything" if we passed it to `SetSecurityInfo`, so both are
/// rejected — the fail-closed direction. Our own SDDL always carries a `D:`
/// clause, so this only fires if that literal is ever broken.
fn owner_only_dacl(sddl: &str) -> io::Result<OwnedDacl> {
    let sd = security_descriptor_from_sddl(sddl)?;
    let mut present: i32 = 0;
    let mut acl: *mut ACL = ptr::null_mut();
    let mut defaulted: i32 = 0;
    // SAFETY: `sd.0` is a valid descriptor owned by `sd`; the three out-pointers
    // are stack locals that outlive the call. On success `acl` points *into* the
    // descriptor, which `OwnedDacl` keeps alive.
    if unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut acl, &mut defaulted) } == 0 {
        let err = io::Error::last_os_error();
        return Err(io::Error::new(
            err.kind(),
            format!("cannot read the DACL out of {sddl:?}: {err}"),
        ));
    }
    if present == 0 || acl.is_null() {
        return Err(io::Error::other(format!(
            "{sddl:?} produced a NULL DACL (which would grant everyone access)"
        )));
    }
    Ok(OwnedDacl { _sd: sd, acl })
}

/// Apply a protected current-user-only DACL to the file or directory at `path`.
///
/// Path-based because directories are created through `std::fs` and we never hold
/// a handle to them. `SE_FILE_OBJECT` covers both files and directories.
fn apply_owner_only_dacl_to_path(path: &Path) -> io::Result<()> {
    let dacl = owner_only_dacl(&file_sddl(&current_user_sid()?))?;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call,
    // `dacl.acl` points into a descriptor `dacl` still owns, and the two null SID
    // arguments are the documented "do not change the owner/group" form matching
    // the DACL-only `*_SECURITY_INFORMATION` flags.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl.acl,
            ptr::null(),
        )
    };
    win32_error_to_result(
        rc,
        &format!(
            "SetNamedSecurityInfoW (owner-only DACL on {})",
            path.display()
        ),
    )
}

/// Canonical string form (`S-1-5-21-…`) of a SID.
///
/// The trust comparison is done on these strings rather than with `EqualSid`
/// because the conversion is a bijection — `ConvertSidToStringSidW` is the
/// canonical rendering, and both sides of the comparison come from it — and
/// because it makes the decision ([`super::endpoint_owner_is_trusted`]) pure data,
/// so the fail-closed rules are unit-testable on Linux CI as well as on Windows.
fn sid_to_string(sid: PSID) -> io::Result<String> {
    let mut wide: *mut u16 = ptr::null_mut();
    // SAFETY: `sid` is a valid SID owned by the caller's still-live descriptor and
    // `wide` is a valid out-pointer; on success the callee hands back a
    // `LocalAlloc`'d NUL-terminated string, freed below.
    if unsafe { ConvertSidToStringSidW(sid, &mut wide) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut len = 0usize;
    // SAFETY: `wide` is NUL-terminated, so the scan stops inside the allocation.
    while unsafe { *wide.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` UTF-16 units of the live `LocalAlloc`'d buffer.
    let out = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(wide, len) });
    // SAFETY: frees exactly the buffer `ConvertSidToStringSidW` allocated; `wide`
    // is not read again.
    unsafe { LocalFree(wide.cast()) };
    Ok(out)
}

/// Map a `WIN32_ERROR`-returning security API onto `io::Result`. These functions
/// return the error code directly instead of setting the thread's last error, so
/// `io::Error::last_os_error()` would report something unrelated.
fn win32_error_to_result(rc: u32, what: &str) -> io::Result<()> {
    if rc == ERROR_SUCCESS {
        return Ok(());
    }
    let err = io::Error::from_raw_os_error(rc as i32);
    Err(io::Error::new(err.kind(), format!("{what} failed: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    // These run on the `windows-latest` CI job (`cargo nextest run`), where they
    // exercise the real advapi32 calls against real objects — the part a Linux
    // `cargo check --target` cannot reach. The pure-data half of the trust
    // decision is tested on every platform in `super::super::tests`.

    /// The [BLOCKER]'s server half: a real security descriptor is built, it names
    /// the current user as owner, and its DACL is present and non-NULL (a NULL
    /// DACL would grant everyone access — the failure mode this whole milestone
    /// exists to prevent).
    #[test]
    fn pipe_security_descriptor_is_owner_only_and_has_a_real_dacl() {
        let sid = current_user_sid().expect("read the current user SID");
        let sddl = pipe_sddl(&sid);
        assert!(sddl.starts_with(&format!("O:{sid}")), "{sddl}");
        assert!(sddl.contains("D:P("), "the DACL must be protected: {sddl}");
        assert_eq!(
            sddl.matches(&sid).count(),
            2,
            "owner and the single ACE must both be us: {sddl}"
        );

        let sd = pipe_security_descriptor().expect("build the pipe security descriptor");
        let attrs = sd.security_attributes();
        assert!(!attrs.lpSecurityDescriptor.is_null());
        assert_eq!(attrs.bInheritHandle, 0);

        // And the file variant's DACL really parses out of the descriptor.
        owner_only_dacl(&file_sddl(&sid)).expect("extract the owner-only file DACL");
    }

    /// A malformed SDDL string must be an error, never a silent fall back to the
    /// default (`Everyone`-readable) descriptor.
    #[test]
    fn a_broken_sddl_string_fails_closed() {
        assert!(security_descriptor_from_sddl("this is not sddl").is_err());
        // `D:NO_ACCESS_CONTROL` is valid SDDL for a NULL DACL — i.e. "grant
        // everyone everything". It must not survive DACL extraction.
        assert!(owner_only_dacl("D:NO_ACCESS_CONTROL").is_err());
    }

    /// The file/dir ACL half, end to end against the real filesystem: a directory
    /// and a file both take the protected owner-only DACL, and re-applying is
    /// idempotent (`ensure_owner_only_dir` re-applies on every call by design).
    #[test]
    fn owner_only_dacls_apply_to_a_real_dir_and_file() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("state").join("nested");

        ensure_owner_only_dir(&dir).expect("create + ACL a fresh dir");
        ensure_owner_only_dir(&dir).expect("re-applying to an existing dir must work");
        create_owner_only_dir(&dir).expect("create-only on an existing dir is a no-op");
        create_owner_only_dir(&root.path().join("fresh")).expect("create-only on a fresh dir");

        let file = std::fs::File::create(dir.join("secrets.toml")).expect("create file");
        set_file_owner_only(&file).expect("apply the owner-only file DACL");
        set_file_owner_only(&file).expect("re-applying must be idempotent");
    }
}
