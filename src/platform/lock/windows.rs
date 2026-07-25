//! Windows spawn lock (PRD #163 M2): a **named mutex** standing in for the Unix
//! exclusive `flock(2)`.
//!
//! `CreateMutexW` + `WaitForSingleObject(INFINITE)` is the idiomatic Win32
//! primitive for cross-process mutual exclusion, and it matches `flock(LOCK_EX)`
//! where it counts: acquisition blocks until granted (so it doubles as the "wait
//! for the in-flight spawn to finish" barrier the callers rely on), and the kernel
//! *notifies* the next waiter when a holder dies without releasing rather than
//! leaving a stale artifact behind (`WAIT_ABANDONED`, see [`acquire_spawn_lock`]).
//!
//! Shape preserved from the Unix backend: [`acquire_spawn_lock`] takes the lock
//! **path**, does not block the runtime, and hands back an RAII [`SpawnLock`]
//! whose `Drop` releases. The path is *not* opened here — it is the identity key
//! the mutex name is derived from (see
//! [`crate::platform::lock::spawn_mutex_name`], which also documents why the name
//! is per-path and how this object doubles as the singleton-daemon guard). No file
//! is created: a kernel object needs no on-disk artifact, and the Unix
//! `spawn.lock` file has no readers other than `flock` itself.
//!
//! **Thread affinity is the one place this cannot be a naive translation.** A
//! Win32 mutex is owned by the *thread* that waited on it: `ReleaseMutex` from any
//! other thread fails with `ERROR_NOT_OWNER`, and until the owning thread releases
//! it or exits, every other waiter blocks. `flock` has no such rule — it belongs
//! to the open file description, so any thread may unlock it. Acquiring on
//! tokio's blocking pool and releasing in `Drop` (which runs on whatever worker
//! polls the caller) would therefore leave the mutex held by an idle pool thread
//! and wedge every later spawn. So each guard owns a **dedicated thread** that
//! waits, parks while the guard is alive, and releases — always the same thread.
//! It also restores an in-process property Unix has for free: two guards in one
//! process sit on two different threads, so they genuinely contend instead of
//! recursively re-entering one thread's ownership.
//!
//! Security: the mutex is created with a protected, current-user-only DACL, the
//! Windows counterpart of the Unix lock file's 0o600-inside-a-0o700-dir
//! (`lock_root_default` deliberately avoids world-writable roots so a foreign uid
//! cannot pre-create the lock entry and DoS daemon startup — PRD #93 round-4
//! auditor BLOCKER). The `Global\` namespace is open to all local users for
//! *creation*, so the residual is the one inherent to any named-object design: a
//! foreign user who creates the object **first** can hold it and stall our daemon
//! start. The DACL closes the much wider hole — that anyone could open and hold
//! the object *we* created. Detecting a pre-squatted object requires comparing its
//! owner SID, which belongs with the rest of the object-security audit in #163 M4
//! (`fsperm`).

use std::path::Path;
use std::sync::mpsc;

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, LocalFree, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, INFINITE, ReleaseMutex, WaitForSingleObject,
};

/// RAII guard for the spawn mutex, mirroring the Unix `flock` guard's lifecycle:
/// held for as long as the caller keeps it, released on `Drop`.
///
/// The mutex itself is owned by the thread that acquired it (see the module
/// docs); this guard is the handle on that thread's lifetime. `Drop` drops
/// `release`, which ends the owner thread's park, then joins it — so the mutex is
/// provably released before `Drop` returns, exactly as the Unix backend's
/// explicit `LOCK_UN` is.
pub struct SpawnLock {
    /// Dropped to signal the owner thread to release. `None` only after `Drop`
    /// has taken it.
    release: Option<mpsc::Sender<()>>,
    owner: Option<std::thread::JoinHandle<()>>,
    name: String,
}

impl Drop for SpawnLock {
    fn drop(&mut self) {
        // Dropping the sender turns the owner thread's blocking `recv()` into an
        // error, which is its cue to `ReleaseMutex` and exit.
        drop(self.release.take());
        if let Some(owner) = self.owner.take()
            && owner.join().is_err()
        {
            // The owner thread only ever parks and releases, so this is
            // unreachable in practice; if it ever happens the mutex was left
            // abandoned (the next waiter still gets it, via WAIT_ABANDONED).
            tracing::warn!(
                mutex = %self.name,
                "the spawn-lock owner thread panicked; the mutex was abandoned rather than \
                 released"
            );
        }
    }
}

/// Windows counterpart to the Unix `acquire_spawn_lock`: acquire the named mutex
/// that stands in for an exclusive `flock(2)` on `path`.
///
/// `WaitForSingleObject` blocks, so — like the Unix `flock` — the wait happens off
/// the runtime; here on the guard's own owner thread rather than on
/// `spawn_blocking`, because the release has to happen on the acquiring thread
/// (see the module docs).
///
/// **`WAIT_ABANDONED`** means the previous holder exited without releasing
/// (crash, `TerminateProcess`, kill-on-job-close). The wait still *succeeds* and
/// this thread now owns the mutex, so it is treated as "acquired, after a crash"
/// — matching Unix, where the kernel drops a dead process's `flock` and the next
/// waiter is granted it with no ceremony. We warn, because it points at a daemon
/// that died mid-spawn, but we do not fail: refusing here would wedge the deck
/// permanently after any hard kill.
pub async fn acquire_spawn_lock(path: &Path) -> std::io::Result<SpawnLock> {
    // One source of truth for "which user is this": the same cached, validated
    // token the named-pipe endpoints are namespaced by, which on Windows *is* the
    // current user's SID string — so the mutex name, its DACL, and the endpoint it
    // guards can never disagree about the owner. Note this shares that function's
    // hard-fail: an unreadable SID panics here too, and unlike the endpoint names
    // there is no `DOT_AGENT_DECK_*` override that bypasses it. Resolved here (it
    // is cached and cheap) so the panic surfaces on the caller's side rather than
    // inside the owner thread.
    let user = crate::platform::paths::endpoint_user_suffix();
    let name = super::spawn_mutex_name(&user, path);

    // Acquisition outcome, awaited without blocking the runtime.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<std::io::Result<()>>();
    // Kept alive by the guard; dropping it releases the mutex.
    let (release_tx, release_rx) = mpsc::channel::<()>();

    let thread_name = name.clone();
    let owner = std::thread::Builder::new()
        .name("spawn-lock".to_string())
        .spawn(move || {
            let held = match create_and_acquire(&thread_name, &user) {
                Ok(held) => held,
                Err(err) => {
                    // Receiver gone (caller cancelled) → nothing to report to.
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            if ready_tx.send(Ok(())).is_ok() {
                // Park until the guard drops its sender. The mutex stays owned by
                // THIS thread for the whole guard lifetime — the entire reason
                // this thread exists.
                let _ = release_rx.recv();
            }
            // Either the guard dropped, or the caller was cancelled before we
            // finished acquiring and there is no guard to release it — both mean
            // "release now". SAFETY: `held.0` is the live mutex handle this
            // thread owns (the wait in `create_and_acquire` returned
            // WAIT_OBJECT_0/WAIT_ABANDONED), released exactly once, by its owning
            // thread.
            if unsafe { ReleaseMutex(held.0) } == 0 {
                tracing::warn!(
                    mutex = %thread_name,
                    error = %std::io::Error::last_os_error(),
                    "ReleaseMutex on the spawn lock failed"
                );
            }
            // `held` drops here, closing the handle.
        })?;

    match ready_rx.await {
        Ok(Ok(())) => Ok(SpawnLock {
            release: Some(release_tx),
            owner: Some(owner),
            name,
        }),
        Ok(Err(err)) => Err(err),
        // The thread ended without sending — it can only do that by panicking.
        Err(_) => Err(std::io::Error::other(format!(
            "the spawn-lock owner thread for {name} exited before acquiring the mutex"
        ))),
    }
}

/// Create-or-open the named mutex and block until this thread owns it. Runs on —
/// and must only ever be called from — the guard's owner thread.
fn create_and_acquire(name: &str, user_sid: &str) -> std::io::Result<OwnedHandle> {
    // Held until the end of this function, so the descriptor `attrs` points at
    // stays alive across the `CreateMutexW` call (which copies it into the object).
    let sd = owner_only_security_descriptor(user_sid)?;
    let attrs = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        // The daemon is spawned DETACHED with explicit stdio; nothing should
        // inherit the lock.
        bInheritHandle: 0,
    };

    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `attrs` is a fully initialized `SECURITY_ATTRIBUTES` whose
    // descriptor is still live (`sd` is in scope), and `wide` is a NUL-terminated
    // UTF-16 name. `binitialowner: 0` means "create it, but do not take
    // ownership" — ownership is taken by the wait below, so a caller that creates
    // the mutex and one that finds it already there follow the identical path.
    let handle = unsafe { CreateMutexW(&attrs, 0, wide.as_ptr()) };
    drop(sd);
    if handle.is_null() {
        let err = std::io::Error::last_os_error();
        return Err(std::io::Error::new(
            err.kind(),
            format!("CreateMutexW({name}) failed: {err}"),
        ));
    }
    let held = OwnedHandle(handle);

    // SAFETY: `held.0` is a live mutex handle; `INFINITE` is the documented
    // "block until granted" timeout.
    match unsafe { WaitForSingleObject(held.0, INFINITE) } {
        WAIT_OBJECT_0 => Ok(held),
        WAIT_ABANDONED => {
            tracing::warn!(
                mutex = %name,
                "spawn lock was abandoned by a previous holder (it exited without releasing \
                 — likely a crashed daemon); acquiring it anyway"
            );
            Ok(held)
        }
        other => {
            let err = std::io::Error::last_os_error();
            Err(std::io::Error::other(format!(
                "WaitForSingleObject({name}) returned {other:#010x}: {err}"
            )))
        }
    }
}

/// Owns a Win32 handle and closes it on drop. Never leaves the owner thread, so
/// it needs no `Send`/`Sync`.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a successful `CreateMutexW` and is closed
        // exactly once, here.
        unsafe { CloseHandle(self.0) };
    }
}

/// Build a protected, current-user-only security descriptor for the mutex from
/// SDDL: `D:P` (a DACL with inheritance blocked) granting `GA` (generic all — for
/// a mutex: synchronize + modify-state) to `user_sid` and to nobody else.
/// Deliberately no `Everyone`/`Users` ACE, and no owner/group/SACL clause, so the
/// defaults apply (creator becomes owner).
///
/// `user_sid` is [`crate::platform::paths::endpoint_user_suffix`], which on
/// Windows *is* the current user's SID in string form — the same value the mutex
/// name is namespaced by, so the object's name and its ACL can never disagree
/// about which user it belongs to.
///
/// Fails closed: if the descriptor cannot be built we do not fall back to the
/// default (`Everyone`-openable) one.
fn owner_only_security_descriptor(user_sid: &str) -> std::io::Result<OwnedLocalSd> {
    let sddl = format!("D:P(A;;GA;;;{user_sid})");
    let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `wide` is a NUL-terminated UTF-16 SDDL string, `sd` a valid
    // out-pointer, and a null size pointer is documented as "don't report the
    // size". On success the callee hands back a `LocalAlloc`'d descriptor, freed
    // by `OwnedLocalSd::drop`.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    } == 0
    {
        let err = std::io::Error::last_os_error();
        return Err(std::io::Error::new(
            err.kind(),
            format!("cannot build the owner-only DACL {sddl:?} for the spawn mutex: {err}"),
        ));
    }
    Ok(OwnedLocalSd(sd))
}

/// Frees the `LocalAlloc`'d security descriptor produced from SDDL.
struct OwnedLocalSd(PSECURITY_DESCRIPTOR);

impl Drop for OwnedLocalSd {
    fn drop(&mut self) {
        // SAFETY: frees exactly the buffer
        // `ConvertStringSecurityDescriptorToSecurityDescriptorW` allocated;
        // `self.0` is not read again.
        unsafe { LocalFree(self.0.cast()) };
    }
}
