//! Disk-backed scratch directories for tests that do **not** link the e2e
//! harness — issue #322.
//!
//! **This file must stay free of `crate::` paths.** It is `#[path]`-included by
//! every integration-test crate that needs a disk-backed scratch dir, and there
//! `crate::` resolves to *that* crate's root rather than the library's, so a
//! single such path breaks all of them at once. `linkage-check` rule 9 enforces
//! it mechanically (issue #474); the note above `mod tests` below has the
//! measurement that makes the arrangement worth keeping.
//!
//! `tests/common/mod.rs` holds the real machinery: the candidate ladder,
//! `DAD_E2E_TMPDIR`, the free-space pre-flight, the descriptor-relative
//! validation walk, the per-process root and its `atexit` sweep. Two kinds of
//! test process cannot reach any of it, because neither links that file — the
//! lib target's own `#[cfg(test)]` unit tests, and the integration crates that
//! deliberately do not pull the PTY harness in. Until this module they all
//! allocated in the OS temp dir, which on this project's dev box is the
//! RAM-backed `/tmp` the whole issue is about. Measured during a full recorded
//! `cargo test-e2e`: `src/dispatch.rs`'s e2e-gated dispatch test held a live
//! 184 KiB `/tmp/.tmpYN3lNF` containing a cloned repo and its worktree.
//!
//! This is deliberately the *small* version of that resolution. It is weaker
//! than the harness's in four ways, each a choice rather than an oversight:
//!
//! - It resolves **only** the default private base, `/var/tmp/dad-e2e-<uid>`.
//!   It does not read `DAD_E2E_TMPDIR`. A second, weaker reading of a
//!   security-sensitive variable is worse than not honouring it at all — so a
//!   base you moved with that variable does not move these allocations.
//! - It judges that directory **by name** (`symlink_metadata`), not with the
//!   harness's `openat`/`O_NOFOLLOW` descriptor walk. A symlink, a non-directory
//!   or a directory owned by somebody else is refused, but the check and the
//!   use are two lookups rather than one descriptor.
//! - A refusal is **not fatal**. The harness stops, because it seeds real agent
//!   credentials and silently landing on tmpfs is exactly the failure #322 is
//!   about. Nothing routed through here seeds credentials, and the fallback is
//!   the OS temp dir — precisely what these call sites did before this module
//!   existed, so falling back cannot be a regression.
//! - There is **no per-process root and no exit hook**. Every directory is a
//!   `TempDir` that drops on the normal path. A SIGKILLed process leaves one
//!   behind under the private base, and the `dad-unit-` prefix is what lets
//!   `cargo xtask clean-e2e-tmp` reap it by default.
//!
//! Do not grow this into a second copy of the harness. If a test needs the
//! ladder, the pre-flight or a seeded HOME, it needs `tests/common/`.

use std::io;
use std::path::PathBuf;

/// Directory-name prefix for everything this module allocates.
///
/// Distinct from the harness's `dad-tests-<pid>-*` on purpose: these are not
/// process roots, carry no fixture and no seeded HOME, and the forensic
/// signature documented for `dad-tests-*` leftovers does not apply to them.
/// It is on the reaper's owned-prefix list (`clean_tmp::OWNED_PREFIXES`), which
/// is the only reason a SIGKILL leftover under the private base is reclaimable
/// — `tempfile`'s default `.tmp*` is not reaped there.
const PREFIX: &str = "dad-unit-";

/// Allocate a scratch directory under the private disk-backed base.
///
/// Returns `io::Result` rather than panicking so it substitutes for
/// `tempfile::tempdir()` at a call site without disturbing its `.unwrap()` /
/// `.expect(…)`.
pub fn tempdir() -> io::Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(PREFIX);
    match private_base() {
        Some(base) => builder.tempdir_in(base),
        None => builder.tempdir(),
    }
}

/// `/var/tmp/dad-e2e-<uid>`, created 0700 or verified as ours; `None` when it
/// cannot be trusted, which sends the caller to the OS temp dir.
#[cfg(unix)]
fn private_base() -> Option<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    // SAFETY: `geteuid` takes no arguments, always succeeds, and cannot fail
    // or leave errno meaningful. Same call the harness makes.
    let uid = unsafe { libc::geteuid() };
    let base = PathBuf::from("/var/tmp").join(format!("dad-e2e-{uid}"));

    // `mkdir(2)` applies the mode itself; never chmod'ed afterwards, so the
    // directory is never briefly wider than 0700. An absent `/var/tmp`, a
    // read-only filesystem or a permission error is an ordinary environment
    // difference — fall through quietly, exactly as the harness's ladder does
    // for a merely *absent* parent.
    match std::fs::DirBuilder::new().mode(0o700).create(&base) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(_) => return None,
    }

    let meta = std::fs::symlink_metadata(&base).ok()?;
    if meta.is_dir() && meta.uid() == uid && meta.permissions().mode() & 0o7777 == 0o700 {
        return Some(base);
    }

    // Present but untrustworthy is the surprising case and the one worth
    // naming: a symlink somebody planted, or a directory left at a looser
    // mode. Say so once rather than silently reverting to the OS temp dir.
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "warning: {} is not a directory owned by uid {uid} at mode 0700 — unit-test temp \
             dirs fall back to the OS temp dir (issue #322). `ls -ld {}` then `rm -rf` it.",
            base.display(),
            base.display()
        );
    });
    None
}

/// No POSIX ownership or mode bits to judge a candidate by, and no `/var/tmp`.
/// The OS temp dir is the only option, which is what the harness's own ladder
/// falls to on this platform.
#[cfg(not(unix))]
fn private_base() -> Option<PathBuf> {
    None
}

// These two run once more in every crate that `#[path]`-includes this file —
// `tests/assemble_changelog.rs`, `tests/codex_hook_ingestion.rs`,
// `tests/codex_hooks_safety.rs`, `tests/daemon_protocol.rs`,
// `tests/devin_hook_ingestion.rs`, `tests/features.rs`,
// `tests/hook_rule_identification.rs`, `tests/pane_close.rs`,
// `tests/rehydration.rs` and `tests/worktree_reclaim.rs`, ten of them as of
// #474. That is the whole cost of sharing the module this way — two extra
// fast-tier executions per crate, against the ~530 that pulling
// `tests/common/mod.rs` in would have duplicated. Measured across the six added
// at once: 2,315 → 2,327.
// Keeping it self-contained is what makes that price available, so a `crate::`
// reference added here would take it away — which is why `linkage-check` rule 9
// now refuses one instead of leaving the constraint to this comment (#474).
#[cfg(test)]
mod tests {
    /// A scratch dir comes back, and it really is a directory on disk —
    /// whichever rung the resolution landed on.
    #[test]
    fn tempdir_allocates_a_real_directory() {
        let dir = super::tempdir().expect("allocate a unit-test tempdir");
        assert!(dir.path().is_dir(), "{} is not a dir", dir.path().display());
    }

    /// When the private base is usable, the dir is *inside* it and carries the
    /// reapable prefix. Conditioned on `private_base()` rather than asserted
    /// unconditionally: on Windows, or on a machine whose `/var/tmp` cannot
    /// hold the base, falling back to the OS temp dir is the documented
    /// behaviour, not a failure.
    #[test]
    fn tempdir_lands_under_the_private_base_when_it_is_usable() {
        let Some(base) = super::private_base() else {
            return;
        };
        let dir = super::tempdir().expect("allocate a unit-test tempdir");
        assert_eq!(dir.path().parent(), Some(base.as_path()));
        let name = dir
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        assert!(
            name.starts_with(super::PREFIX),
            "{name} does not carry the reapable `{}` prefix",
            super::PREFIX
        );
    }
}
