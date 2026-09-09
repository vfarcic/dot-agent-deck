//! `cargo xtask clean-e2e-tmp` — reap stale e2e harness temp dirs (issue #322).
//!
//! The harness nests everything it creates under one `dad-tests-<pid>-*` root
//! per test process, removed by an `atexit` hook on the normal-exit path. A
//! process that is SIGKILLed — nextest's `slow-timeout terminate-after`, or an
//! interrupted run — never reaches that hook and leaves its root behind. On a
//! RAM-backed `/tmp` those leftovers are resident memory until someone notices,
//! and the failure mode is self-amplifying: the more the suite fails, the less
//! headroom the next run has.
//!
//! # Ownership decides, age is only the fallback (issue #461)
//!
//! The `<pid>` in the name is the process that **created** the root, so it
//! answers "is the creator still running?". That is a far better proxy for "is
//! anyone still using this?" than age is — but it is **not the same question**,
//! and the gap is deliberate in this codebase: the deck's lazily-spawned daemon
//! `setsid`s out of the test's process group and can outlive it, as can a
//! wrapped agent two `setsid` levels down. A dead owner therefore does *not*
//! prove the tree is unreferenced, which is why the dead branch carries
//! [`DEAD_PID_MIN_AGE`]. Do not remove that floor on the strength of "the PID
//! answers it directly", because it does not.
//!
//! Age alone was a bad proxy in both directions. Measured: 280 roots totalling
//! 6.2 GB, every one with a dead owning PID, were refused because the oldest was
//! 4h09m and the default threshold is 6h — on a 14 GB tmpfs with 5 MiB of swap
//! left and an e2e compile about to start. In the other direction, a genuine
//! suite still running past the threshold was *eligible* to have its own live
//! root deleted out from under it.
//!
//! So the PID decides wherever it can, and `--older-than` only filters the cases
//! it cannot settle. The decision is four branches:
//!
//! - **dead PID** → reap, once the root is at least [`DEAD_PID_MIN_AGE`] old.
//!   `--older-than` does not suppress it; the floor is separate and much
//!   shorter, and exists for the orphan window described above.
//! - **live PID** → keep, at any age, unconditionally.
//! - **live PID with `--ignore-liveness`** → the age rule decides. The operator
//!   asserts that liveness is untrustworthy here, which is the reboot case: a
//!   leftover root's PID can be reissued to an unrelated long-lived process,
//!   and nothing this program can measure distinguishes that from a running
//!   suite. Never inferred — see "Why there is no recycled-PID branch" below.
//! - **no usable PID** — an untagged `.tmp*` dir, a pre-fix lock dir, a
//!   malformed name, or a platform with no `kill(2)` — the age rule decides.
//!
//! Liveness is `kill(pid, 0)`, in which `EPERM` counts as **alive**: the process
//! exists, it merely is not ours, and reading that as dead would delete a live
//! run's root.
//!
//! One thing `kill(2)` cannot tell us: it answers about the **caller's** PID
//! namespace. A container that bind-mounts `/tmp` from the host would have a
//! host-side reaper probe host PIDs against in-container names. No workflow here
//! does that — the tooling is `devbox`, same namespace — but the answer is
//! "about a different namespace", not "unavailable", so no fallback triggers.
//! # Why there is no recycled-PID branch
//!
//! Issue #461 originally called for a fourth branch: a live PID *proven* to have
//! started after the root already existed is not the root's owner, so let age
//! decide. Two successive attempts to build that proof were wrong in the same
//! direction — deleting a live run's working directory — and the branch is
//! deliberately gone rather than patched a third time.
//!
//! The comparison is unsound in principle, not merely mis-implemented. Ordering
//! a process start against a directory timestamp has to bridge through the wall
//! clock: the directory side is only ever a stored `CLOCK_REALTIME` value, and
//! the process side has to be reconstructed as boot time plus a tick counter.
//! Linux's `getboottime64()` contract says outright that `settimeofday` shifts
//! the boot time behind `/proc/stat`'s `btime`, while inode timestamps are never
//! retroactively adjusted. So a forward clock step — admin action, a VM clock
//! correction, a time-sync daemon, suspend/resume — moves a *live* process's
//! reconstructed start forward while its root's timestamp stays put, and a
//! one-hour correction is enough to push a process that started a second before
//! creating its root an hour "after" it. The bias lands squarely on the
//! deletion-unsafe side, and no input available here turns it back into positive
//! proof. (The attempt before that read the `/proc/<pid>` directory's mtime,
//! which is a dentry *lookup* time — Linux instantiates that inode lazily — and
//! deleted live roots for a different reason with the same shape.)
//!
//! Dropping the branch is still the right trade, but the cost is **not** simply
//! "a deferral", and an earlier version of this comment claiming a PID collision
//! is always *transient* was wrong. Within one boot it is: the colliding process
//! exits, and the next run classifies the root `dead-pid` and reaps it. **Across
//! a reboot it is not.** A leftover root outlives the boot, low PIDs are handed
//! to long-lived system units early in the next one, and the root is then pinned
//! `live-pid` for the whole life of that boot — possibly re-colliding after the
//! next reboot. That matters most on a filesystem not cleared at boot, which is
//! where the harness roots are heading (`/var/tmp`, issue #322), and those roots
//! hold real agent credentials.
//!
//! `--ignore-liveness` is the answer, and its shape is deliberate: the operator
//! supplies the one fact the program cannot measure — that the machine rebooted,
//! so liveness here is meaningless — and the roots then fall back to the age
//! rule rather than being reaped outright. That keeps the judgement out of the
//! code, which is the whole point of deleting the inferred branch.
//!
//! Do not reinstate it, in any form — including a report-only "possibly
//! recycled" annotation. That would keep the whole `/proc` parsing surface,
//! which is exactly where the wrong answers came from, in exchange for a hint
//! nobody can act on differently.
//!
//! No *behavioural* test can enforce that, and the tests in this file do not
//! pretend to. Tripping a reinstated comparison needs a root whose timestamp
//! predates its owner's start by more than the comparison's margin, and a
//! directory's birth time cannot be backdated by any portable API — a test can
//! set mtime and nothing else, so the only gap it can manufacture is
//! milliseconds, far inside the five-minute margin the deleted code used and
//! inside any plausible replacement. Restore that code verbatim and every
//! behavioural test here still passes. What guards it instead is a source-level
//! scan (`source_has_no_pid_recycling_machinery`, which reads this very file),
//! the shape of [`owner_of`], which takes no timestamp and so cannot express
//! the comparison, and code review. None of the three is a substitute for
//! reading the diff.
//!
//! Two limits on that scan, stated so nobody mistakes it for a fence. It reads
//! **this file only**, so the same inference reintroduced in a new module and
//! called from `SystemProbe::is_alive` would pass every guard here untouched —
//! that impl is the seam to watch. And it is a *source* scan, so it constrains
//! spelling rather than behaviour: an equivalent comparison written with none of
//! the forbidden tokens would pass it. Since issue #489 the scan does at least
//! **run**: `cargo test-fast` and all three CI build jobs select `--workspace`,
//! which reaches this crate's tests. Before that the workspace had no
//! `default-members`, so they were compiled by `cargo clippy --workspace
//! --all-targets` and executed by nothing.
//!
//! # What this will and will not delete
//!
//! Deleting by prefix in a shared `/tmp` is only safe for names this repo
//! actually owns:
//!
//! - `dad-tests-*` — the current harness root. Ours, unambiguously.
//! - `dad-unit-*` — scratch dirs from tests that do not link the harness at all
//!   (`src/test_temp.rs`). Not process roots: no fixture, no seeded HOME, and
//!   no exit hook, so a SIGKILL leaves one behind with nothing else to reclaim
//!   it. Named rather than left as `.tmp*` precisely so this command can.
//! - `dot-agent-deck-test-lock-*` — the pre-fix lock dirs. Also ours; still
//!   present in bulk on machines that ran the suite before the leak was fixed.
//! - `.tmp*` — **not** reaped unless `--include-untagged` is passed. That is
//!   the `tempfile` crate's *default* prefix, so it belongs to every Rust
//!   program on the machine, not just this suite. Globbing it blindly can
//!   delete a live temp dir owned by something else entirely.
//!
//! Dry-run is the default; `--apply` is required to remove anything.
//!
//! # Where it looks
//!
//! The **standard** roots only, from [`standard_roots`]: the harness's private
//! `/var/tmp/dad-e2e-<uid>` parent, and the system temp dir (where the roots
//! used to live, and still do on the last-resort rung of the harness ladder).
//!
//! Placement and deletion are deliberately different trust decisions, so a
//! `DAD_E2E_TMPDIR` that moved the harness somewhere else does **not** silently
//! become a directory this command deletes from — it prints a hint naming it,
//! and `--root <path>` is how you opt in.
//!
//! # The boundary this command has to prove for itself
//!
//! `/var/tmp/dad-e2e-<uid>` is what makes "everything under here is ours" true,
//! and the *harness* proving it is not enough: this is the half that **deletes**,
//! and the name is predictable in a world-writable directory, so another local
//! user can occupy it before the victim's first run. Every root is therefore
//! vetted here in its own right ([`vet_root`]) before a single entry is read —
//! the private parent must be a real directory, owned by this UID, with no
//! group or other bits, inside a `/var/tmp` that is itself a root-owned sticky
//! directory. A symlink at that name is **refused, never followed**. Scanning
//! and removal then run against the path vetting resolved, not the spelling it
//! was handed, so nothing can be retargeted underneath the walk.
//! `dad-tests-*` carries the owning PID and is decided by liveness; `dad-unit-*`
//! and `dot-agent-deck-test-lock-*` carry none, so the age rule decides them.
//!
//! Dry-run is the default; `--apply` is required to remove anything.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

/// Directory-name prefixes this repo owns outright and may reap by default.
const OWNED_PREFIXES: &[&str] = &["dad-tests-", "dad-unit-", "dot-agent-deck-test-lock-"];

/// The prefix that carries the owning PID: `dad-tests-<pid>-<random>`.
const PID_TAGGED_PREFIX: &str = "dad-tests-";

/// The `tempfile` crate's default prefix — shared with every other Rust
/// program, so it is opt-in only.
const UNTAGGED_PREFIX: &str = ".tmp";

const DEFAULT_MAX_AGE_HOURS: u64 = 6;

/// The harness's explicit temp-base override. Read here only to *mention* it —
/// see [`override_hint`].
const TEMP_BASE_ENV: &str = "DAD_E2E_TMPDIR";

/// The shared directory the harness's private parent lives in.
#[cfg(unix)]
const SHARED_VAR_TMP: &str = "/var/tmp";

/// The longest `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS` any test in this
/// repository pins — the orphan window [`DEAD_PID_MIN_AGE`] is derived from.
///
/// The harness default is 300 s (`tests/common/mod.rs` pins it at both
/// `env_clear` sites, and `tests/common/child_lifetime_bound.rs` clamps an
/// ambient value to it), but a test may legitimately pin a longer one for
/// itself: `orchestration_dispatch_002` pins **900** so the daemon outlives
/// that test's own 300 s work budget, without which the failure dump reports
/// `NO PANE — never spawned at all` for roles the same dump renders alive
/// (issues #663 / #665). A `TuiDeck` builder's `extra_env` is applied *after*
/// the harness pin, so such a value reaches the child verbatim and the clamp
/// never sees it — that clamp is an **ambient**-value guarantee, not a
/// universal one.
///
/// So this constant is what the floor below is actually entitled to assume, and
/// it is not a preference: raising it raises the floor with it. Issue #679 is
/// what happens when the two drift — the floor was 600 s (2× 300) while this
/// was already 900, so between 600 s and 900 s `--apply` could delete a root a
/// still-entitled daemon was writing under.
///
/// **Checked, not assumed.** linkage-check rule 11 fails the build when a pin
/// under `tests/` exceeds this value, so the next cap raise is a red build that
/// names this constant rather than a silent re-opening of #679. There is
/// deliberately no per-line opt-out: unlike rules 8 and 10, an exception here
/// would not be a local judgement call — any pinned cap *is* how long an orphan
/// may write, so the only correct response to a longer one is to raise this
/// number and let the floor follow.
pub(crate) const MAX_PINNED_ORPHAN_CAP_SECS: u64 = 900;

/// Minimum age before a **dead-owner** root is reaped, because owner death is
/// not the same thing as the tree being unreferenced.
///
/// The name carries the *test process's* PID, but the processes that test
/// spawned do not die with it. `tests/common/mod.rs` says so at the group-kill
/// site: "The deck's own lazy-spawned daemon setsid's into a separate session
/// and escapes this group — its `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS` cap is
/// the net for that." `src/daemon_attach.rs` runs `setsid(2)` in `pre_exec` so
/// the daemon outlives its parent, and a *wrapped* agent sits two `setsid`
/// levels down, in a group the deck cannot signal at all.
///
/// So after nextest SIGKILLs a test, an orphan can keep writing under the dead
/// test's root for up to that cap. Reaping a dead owner instantly would hand
/// `remove_dir_all` the working directory of a live process — and the moment a
/// developer is most likely to run `--apply` is right after a run died, which
/// is exactly when orphans are still alive.
///
/// **"Up to that cap" is narrower than it reads, and issue #861 is what pinned
/// the scope down.** It holds for the class the paragraph above names — the
/// deck's lazily-spawned daemon, and a wrapped agent's child since #657/#661 —
/// because each is either one of this project's own binaries enforcing the cap
/// in-process, or is `killpg`'d at the deadline by a forked reaper that holds it
/// independently of any owner. It did **not** hold for a descendant that
/// `setsid`s out of every group those reapers can signal: measured on the shared
/// dev box, pid 2043710 carried `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS=300` in
/// its own environment at `PPID 1` and had been alive **four days** — about
/// 1170x its cap.
///
/// That did not make this floor unsafe, and the reason is worth stating rather
/// than assuming, because the obvious reading points the wrong way. No finite
/// floor bounds an unbounded writer, so for that class a larger number would
/// have bought nothing. And the floor was never what protected that class: this
/// reaper decides `live-pid` from the **test process's** pid in the root's name
/// and never from an orphan's, so an escapee has never held its own root against
/// `--apply` at any age. That was already known and already accepted — 221 such
/// orphans were censused for #668, "each still holding a working directory the
/// tooling had already deleted" — with the cost named as the process itself:
/// retained inodes, a polluted `ps`, and the chance of it re-creating paths
/// under a root that was just removed.
///
/// So #861 changed the claim rather than the number, and then narrowed the gap
/// it exposed: `src/lifetime_tag.rs` gives the deadline a way to find a process
/// by a per-spawn tag in its environment instead of by a process group, so an
/// escapee that carries the tag is now bounded by the same cap on Linux for as
/// long as its reaper lives. That leaves this derivation better supported than
/// when it was written and still not universal — a `SIGKILL` of the reaper
/// itself, a descendant that rebuilt its environment, and a non-Linux host each
/// remain outside it.
///
/// The old age-only rule never had this problem: its 6h floor was 72× the
/// orphan cap. This floor restores that protection at 2× the cap instead of
/// 72×, which is ample, and costs #461 nothing — the case it was filed for was
/// 280 roots whose youngest was 4h09m.
///
/// **The 2× is against [`MAX_PINNED_ORPHAN_CAP_SECS`], not against the 300 s
/// default** (issue #679). It was hard-coded at 600 s and read as "2× 300"
/// while one test already pinned 900, which left the derivation 300 s short of
/// what it claimed. Deriving it here is what makes rule 11 able to enforce the
/// pair: the guard and the floor now read the same number. The cost is that a
/// killed run's roots are held 30 minutes instead of 10 before `--apply` can
/// take them — real, but small next to #461's own case (280 roots, youngest
/// 4h09m) and paid on the safe side.
///
/// Note this is a timestamp on the **keep** side. That is the safe bias, and
/// the exact inverse of the deleted PID-recycling comparison, which biased
/// toward deletion (see the module docs).
pub(crate) const DEAD_PID_MIN_AGE: Duration = Duration::from_secs(MAX_PINNED_ORPHAN_CAP_SECS * 2);

/// Per-directory lines printed before the per-reason summary. A machine that
/// has been leaking for a few hours accumulates hundreds of roots, and 280
/// lines of path bury the one number the user needs; the summary below the list
/// is always complete, and the truncation says how much it dropped.
const MAX_LISTED: usize = 20;

/// Work budget for one root's size walk, in directory entries and in depth.
///
/// Before issue #461 the walk only ever ran on age-eligible roots. It now runs
/// on every owned root including fresh ones and live-owner ones that are about
/// to be kept, so an enormous — or deliberately planted — tree under a
/// world-writable `/tmp` would otherwise burn unbounded CPU and I/O on every
/// invocation, dry run included. Past the budget the walk stops and the size is
/// reported as a lower bound. Sizing is presentation only and never reaches
/// [`classify`], so truncating it cannot change a single reap/keep verdict.
const MAX_SIZE_WALK_ENTRIES: usize = 50_000;
const MAX_SIZE_WALK_DEPTH: usize = 64;

/// A directory to look in, plus the facts that differ between them.
struct ScanRoot {
    /// Original spelling. Every message uses this, not the canonical form.
    path: PathBuf,
    /// Whether `--include-untagged` applies here. Issue #322: `.tmp*` is the
    /// `tempfile` crate's default prefix, so the flag is confined to the
    /// historical system temp root (and to roots the user names by hand);
    /// letting it follow the widened root set would put `cargo`'s own build
    /// output and other tooling's persistent tempdirs in range.
    untagged_ok: bool,
    /// Named with `--root`. An unreadable root the user asked for by hand is an
    /// error; an absent standard root is normal and silent.
    required: bool,
    /// Whether this is the harness's own private, UID-scoped parent — the one
    /// the "everything under it is ours by construction" argument rests on, and
    /// therefore the one that has to be *proved* private before this command
    /// deletes anything under it. See [`private_root_verdict`].
    private: bool,
}

/// A root that passed [`vet_root`]: what to show, and what to actually touch.
struct VettedRoot {
    /// Original spelling, for every message.
    shown: PathBuf,
    /// The validated, fully-resolved path `read_dir` and `remove_dir_all`
    /// operate on. Kept separate from `shown` deliberately: resolving only for
    /// de-duplication while scanning and deleting the *original* spelling is
    /// what let a retargetable symlink make the object deleted differ from the
    /// object listed.
    scan: PathBuf,
    untagged_ok: bool,
}

/// The private, UID-scoped parent the harness puts its per-process roots in.
///
/// Mirrors `private_parent_name()` in `tests/common/mod.rs`, duplicated rather
/// than shared because an xtask crate cannot depend on an integration-test
/// module. Unix-only and `cfg`-gated to match the harness exactly: on Windows
/// `/var/tmp` is a root-relative path on the current drive that the harness
/// never uses but `--apply` could still delete from.
#[cfg(unix)]
fn private_parent() -> PathBuf {
    // SAFETY: `geteuid` takes no arguments, always succeeds, and touches no
    // memory this process owns.
    let uid = unsafe { libc::geteuid() };
    Path::new(SHARED_VAR_TMP).join(format!("dad-e2e-{uid}"))
}

/// The roots reaped without being asked for by name.
///
/// Both are places the *current* machine's harness puts roots. It cannot infer
/// another worktree's leftovers, or a `DAD_E2E_TMPDIR` that is no longer
/// exported — run it where the run that leaked them ran, or name the directory
/// with `--root`.
fn standard_roots() -> Vec<ScanRoot> {
    let mut roots = vec![
        // The historical root: where every leftover from before issue #322
        // sits, and where the harness still lands when no private parent is
        // usable.
        ScanRoot {
            path: std::env::temp_dir(),
            untagged_ok: true,
            required: false,
            private: false,
        },
    ];
    // Listed first because it is where a current run's roots are; `insert`
    // rather than building the vec in order so the Windows build is not left
    // with a one-armed `cfg` around the whole literal.
    #[cfg(unix)]
    roots.insert(
        0,
        ScanRoot {
            path: private_parent(),
            untagged_ok: false,
            required: false,
            private: true,
        },
    );
    roots
}

/// Why the shared directory the private parent lives in — `/var/tmp` — cannot
/// be trusted to hold it. Pure, so a foreign owner can be injected.
///
/// `/var/tmp` is mode 1777 on every normal system: world-writable, but sticky,
/// so only an entry's own owner may rename or remove it. That is what makes the
/// private parent's name un-hijackable once it exists. World-writable *without*
/// the sticky bit is the shape where any local user could swap our parent for
/// theirs between this check and the deletion below, so it is refused.
#[cfg(unix)]
fn shared_parent_verdict(
    path: &Path,
    is_symlink: bool,
    is_dir: bool,
    uid: u32,
    mode: u32,
    euid: u32,
) -> Option<String> {
    let path = path.display();
    if is_symlink {
        return Some(format!("{path} is a symlink, not a real directory"));
    }
    if !is_dir {
        return Some(format!("{path} is not a directory"));
    }
    if uid != 0 && uid != euid {
        return Some(format!(
            "{path} is owned by uid {uid}, neither root nor {euid}"
        ));
    }
    if mode & 0o022 != 0 && mode & 0o1000 == 0 {
        return Some(format!(
            "{path} is mode 0o{mode:o} — group/world-writable without the sticky \
             bit, so any local user could swap the directory below it"
        ));
    }
    None
}

/// Why the harness's private, UID-scoped parent must not be scanned or deleted
/// from. Pure, so the foreign-owner case — which needs `chown` to build on disk
/// — can be driven with injected values.
///
/// This is the check the whole design rested on and did not have. The *harness*
/// verifies this directory before it writes anything under it, and refuses to
/// start when it is foreign; the reaper, which is the half that **deletes**,
/// verified nothing at all. `/var/tmp/dad-e2e-<uid>` is a predictable name in a
/// world-writable directory, so another local user can create it before the
/// victim's first run — and `--apply` would then have scanned their 0777
/// directory and removed the `dad-tests-*` children they put in it. A symlink at
/// the same name was worse: it was resolved for de-duplication and then the
/// original spelling was scanned and deleted through, redirecting the reaper
/// wherever the attacker pointed.
#[cfg(unix)]
fn private_root_verdict(
    path: &Path,
    is_symlink: bool,
    is_dir: bool,
    uid: u32,
    mode: u32,
    euid: u32,
) -> Option<String> {
    let shown = path.display();
    if is_symlink {
        // Refused, never followed: the harness would refuse it too, and
        // following it is exactly how the reaper gets aimed somewhere else.
        return Some(format!(
            "{shown} is a symlink — refused rather than followed; the harness's \
             private parent is always a real directory"
        ));
    }
    if !is_dir {
        return Some(format!("{shown} is not a directory"));
    }
    if uid != euid {
        return Some(format!(
            "{shown} is owned by uid {uid}, not {euid} — it is not this user's \
             harness parent, so nothing under it is this command's to delete"
        ));
    }
    if mode & 0o077 != 0 {
        return Some(format!(
            "{shown} is mode 0o{mode:o}, not owner-only — another user can write \
             into it, so what is under it is not ours by construction"
        ));
    }
    None
}

/// Filesystem-facing adapter over [`private_root_verdict`], including the
/// [`shared_parent_verdict`] check on the directory that holds it.
#[cfg(unix)]
fn private_root_objection(path: &Path, euid: u32) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
        let meta = match std::fs::symlink_metadata(parent) {
            Ok(meta) => meta,
            Err(e) => return Some(format!("cannot stat {}: {e}", parent.display())),
        };
        if let Some(why) = shared_parent_verdict(
            parent,
            meta.file_type().is_symlink(),
            meta.is_dir(),
            meta.uid(),
            meta.permissions().mode() & 0o7777,
            euid,
        ) {
            return Some(why);
        }
    }
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        // Nothing there at all: not an objection, just an absence. The caller
        // distinguishes the two.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return Some(format!("cannot stat {}: {e}", path.display())),
    };
    private_root_verdict(
        path,
        meta.file_type().is_symlink(),
        meta.is_dir(),
        meta.uid(),
        meta.permissions().mode() & 0o7777,
        euid,
    )
}

/// What one root turned out to be.
enum RootVerdict {
    /// Safe to read from and delete under, at this fully-resolved path.
    Scan(PathBuf),
    /// Nothing at the path. Normal for a standard root on a machine that has
    /// never run the suite.
    Absent,
    /// Present and not safe. Never scanned, never deleted from.
    Refused(String),
}

/// Decide whether a root may be scanned, and at which path.
///
/// Two separate jobs. For the private parent, prove the privacy boundary the
/// deletion rests on ([`private_root_verdict`]) — the check whose absence is the
/// blocker this closes. For every root, resolve the spelling **once** here and
/// hand that resolved path to the scan and the deletion, so a symlinked
/// component cannot be retargeted between listing a directory and removing it.
///
/// What this still does not do is hold the root open as a descriptor and
/// enumerate relative to it: `std` offers no `read_dir`-from-`fd` and no
/// `remove_dir_all`-from-`fd`, so that would mean an FFI directory walk of its
/// own. The residual is one lookup wide — between `read_dir` here and
/// `remove_dir_all` below, an entry could be swapped by whoever can write in the
/// root. Under the private parent (0o700, proved ours above) nobody can; under a
/// sticky system temp dir only the entry's own owner can, and our entries are
/// ours; under a hand-named `--root` it is the operator's directory and their
/// call.
fn vet_root(root: &ScanRoot) -> RootVerdict {
    #[cfg(unix)]
    if root.private {
        // SAFETY: `geteuid` takes no arguments, always succeeds, and touches no
        // memory this process owns.
        let euid = unsafe { libc::geteuid() };
        if let Some(why) = private_root_objection(&root.path, euid) {
            return RootVerdict::Refused(why);
        }
    }
    let meta = match std::fs::symlink_metadata(&root.path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return RootVerdict::Absent,
        Err(e) => {
            return RootVerdict::Refused(format!("cannot stat {}: {e}", root.path.display()));
        }
    };
    // A FIFO, socket or plain file at a root name is refused everywhere, not
    // just under the private rule — `read_dir` on one blocks or errors, and
    // `remove_dir_all` on one is not what anybody asked for. A symlink is not
    // judged here: the private arm above has already refused it, and for the
    // system temp dir or a hand-named `--root` following it is the documented
    // behaviour, resolved exactly once by the `canonicalize` below.
    if !meta.file_type().is_symlink() && !meta.is_dir() {
        return RootVerdict::Refused(format!("{} is not a directory", root.path.display()));
    }
    match std::fs::canonicalize(&root.path) {
        Ok(resolved) => RootVerdict::Scan(resolved),
        // A dangling symlink: `symlink_metadata` found the link, `canonicalize`
        // cannot find its target.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => RootVerdict::Absent,
        Err(e) => RootVerdict::Refused(format!("cannot resolve {}: {e}", root.path.display())),
    }
}

/// Identity of a directory for de-duplication: its canonical path when it
/// exists, otherwise its own spelling. Never shown to the user.
fn canonical_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The roots a `--root` list turns into.
///
/// A `--root` that names the harness's OWN private parent gets the private
/// treatment, not the hand-named one. Without this, spelling one directory two
/// ways gave it two different security postures: the standard-root path proves
/// ownership, mode `0o700` and a sticky root-owned holder, and refuses a symlink
/// outright ([`private_root_verdict`]), while `--root` skipped all of it and
/// `canonicalize`d the name instead — so a symlink another local user planted at
/// the predictable `/var/tmp/dad-e2e-<uid>` before the victim's first run was
/// **followed** rather than refused. It also flipped `untagged_ok` on at the one
/// location [`usage`] promises `--include-untagged` never applies to.
///
/// Matched by resolved directory rather than by spelling, so macOS's
/// `/var` → `private/var` alias cannot route around it.
///
/// Pure apart from the `canonicalize` in [`canonical_key`], so the posture can
/// be asserted without building a `/var/tmp` fixture.
fn explicit_scan_roots(paths: &[PathBuf]) -> Vec<ScanRoot> {
    let private_key = private_parent_key();
    paths
        .iter()
        .map(|path| {
            let is_private = private_key
                .as_ref()
                .is_some_and(|private| &canonical_key(path) == private);
            ScanRoot {
                path: path.clone(),
                // Naming a directory by hand is the deliberate act the flag's
                // warning is about, so it is honoured here — except for the
                // private parent, which is never in range.
                untagged_ok: !is_private,
                required: true,
                private: is_private,
            }
        })
        .collect()
}

/// Resolved identity of the harness's private parent, so a hand-named `--root`
/// can be recognised as that same directory however it was spelled.
///
/// `None` off Unix, where there is no `/var/tmp` rung to recognise and every
/// `--root` is therefore an ordinary hand-named directory.
fn private_parent_key() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        Some(canonical_key(&private_parent()))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Drop roots that name the same directory twice.
///
/// Lexical comparison is not enough: a symlink, a `TMPDIR` spelled with a
/// trailing `/.`, or an explicit `--root` that happens to alias a standard one
/// would each be walked twice — doubling the dry-run totals, and under
/// `--apply` failing the second `remove_dir_all` with `NotFound`, reporting an
/// otherwise-successful cleanup as a failure. The first spelling wins, so
/// display keeps whatever the user or the harness actually said.
fn dedup_roots(roots: Vec<ScanRoot>) -> Vec<ScanRoot> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut out: Vec<ScanRoot> = Vec::new();
    for root in roots {
        let key = canonical_key(&root.path);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(root);
    }
    out
}

/// `DAD_E2E_TMPDIR` is a *placement* decision; deleting from it is a separate
/// one. When it is set but not among the roots being scanned, say so rather
/// than either scanning it silently or leaving the user to wonder why the
/// command found nothing.
fn override_hint(roots: &[ScanRoot]) -> Option<String> {
    let base = PathBuf::from(std::env::var_os(TEMP_BASE_ENV).filter(|v| !v.is_empty())?);
    let key = canonical_key(&base);
    if roots.iter().any(|r| canonical_key(&r.path) == key) {
        return None;
    }
    Some(format!(
        "note: {TEMP_BASE_ENV}={} is set but is NOT scanned — where the harness \
         may write and what this command may delete are separate decisions. \
         Add `--root {}` to reap it too.",
        base.display(),
        base.display(),
    ))
}

struct Options {
    max_age: Duration,
    apply: bool,
    include_untagged: bool,
    /// Fall a **live**-PID root back to the age rule instead of keeping it
    /// unconditionally. The escape hatch for a PID that was reused across a
    /// reboot; see [`Reason::LivenessIgnored`].
    ignore_liveness: bool,
}

/// What the PID embedded in a root's name says about who owns the root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// A PID no live process holds: the root is definitively abandoned.
    Dead,
    /// A live PID. The root is kept and no timestamp is consulted, unless the
    /// operator passes `--ignore-liveness`. There is deliberately no *inferred*
    /// "but the PID might have been reused" escape hatch — the only escape is
    /// the explicit flag (see the module docs).
    Live,
    /// No PID to go on: an untagged or malformed name, or a platform on which
    /// liveness cannot be determined.
    Unknown,
}

/// Why a root was reaped or kept, so the report attributes each decision
/// instead of restating an age fact (issue #461).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reason {
    /// Owning process is gone. Reaped once past [`DEAD_PID_MIN_AGE`], kept
    /// below it so a `setsid`'d orphan still writing under the root is not
    /// deleted out from under itself.
    DeadPid,
    /// Owning process is still running — kept whatever its age.
    LivePid,
    /// A live PID that `--ignore-liveness` demoted to the age rule, for the
    /// reboot case where the number was reused by an unrelated process.
    LivenessIgnored,
    /// There was no usable PID, so the age rule decided.
    UntaggedAge,
}

impl Reason {
    /// Fixed order, so the summary reads the same way on every run.
    const ALL: [Reason; 4] = [
        Reason::DeadPid,
        Reason::LivePid,
        Reason::LivenessIgnored,
        Reason::UntaggedAge,
    ];

    fn label(self) -> &'static str {
        match self {
            Reason::DeadPid => "dead-pid",
            Reason::LivePid => "live-pid",
            Reason::LivenessIgnored => "live-aged",
            Reason::UntaggedAge => "untagged",
        }
    }

    /// One-line justification for the summary. Every age-sensitive reason names
    /// the threshold it was judged against and which side of it the dirs fell
    /// on; only [`Reason::LivePid`] is unconditional.
    fn note(self, reap: bool, max_age: Duration) -> String {
        match self {
            Reason::DeadPid => {
                if reap {
                    format!(
                        "owning process is gone; older than {}",
                        human_duration(DEAD_PID_MIN_AGE)
                    )
                } else {
                    format!(
                        "owning process is gone, but younger than {} — a spawned daemon may still be writing here",
                        human_duration(DEAD_PID_MIN_AGE)
                    )
                }
            }
            Reason::LivePid => {
                "owning process is still running — never reaped (--ignore-liveness overrides)"
                    .to_string()
            }
            Reason::LivenessIgnored => {
                let age = human_duration(max_age);
                let side = if reap { "older" } else { "younger" };
                format!("--ignore-liveness: liveness not trusted; {side} than {age}")
            }
            Reason::UntaggedAge => {
                let age = human_duration(max_age);
                let side = if reap { "older" } else { "younger" };
                format!("no owning PID in the name; {side} than {age}")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Verdict {
    reap: bool,
    reason: Reason,
}

struct Candidate {
    path: PathBuf,
    bytes: u64,
    /// The size walk hit its budget, so `bytes` is a lower bound.
    size_truncated: bool,
    age: Duration,
    verdict: Verdict,
}

/// The one process fact the ownership decision needs, behind a trait so the
/// classification matrix can be driven from a table rather than from real PIDs.
///
/// The dead-PID test used to spawn a child, `wait()` it, and then probe the
/// number — but the kernel may reassign a PID the moment it is reaped, so under
/// PID churn that test observed an unrelated live process (issue #461 review).
/// Injecting the probe removes the race and makes every branch of [`owner_of`]
/// reachable without arranging a real process to match.
trait ProcessProbe {
    /// `Some(true)` alive, `Some(false)` dead, `None` where this platform
    /// cannot tell.
    fn is_alive(&self, pid: i32) -> Option<bool>;
}

/// The real probe: `kill(pid, 0)`.
struct SystemProbe;

impl ProcessProbe for SystemProbe {
    fn is_alive(&self, pid: i32) -> Option<bool> {
        pid_is_alive(pid)
    }
}
pub fn run(args: &[String]) -> ExitCode {
    let (opts, explicit_roots) = match parse_args(args) {
        Ok(Some(parsed)) => parsed,
        Ok(None) => return ExitCode::SUCCESS, // --help
        Err(msg) => {
            eprintln!("xtask clean-e2e-tmp: {msg}");
            usage();
            return ExitCode::from(2);
        }
    };

    let temp_roots = dedup_roots(if explicit_roots.is_empty() {
        standard_roots()
    } else {
        explicit_scan_roots(&explicit_roots)
    });
    let searched = temp_roots
        .iter()
        .map(|r| r.path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if let Some(hint) = override_hint(&temp_roots) {
        println!("{hint}");
    }
    let restricted: Vec<String> = temp_roots
        .iter()
        .filter(|r| !r.untagged_ok)
        .map(|r| r.path.display().to_string())
        .collect();
    if opts.include_untagged && !restricted.is_empty() {
        println!(
            "note: --include-untagged does NOT apply to {} — `{UNTAGGED_PREFIX}*` is \
             every Rust program's default prefix and those roots hold more than \
             this suite's leftovers.",
            restricted.join(", "),
        );
    }
    // Vetted BEFORE anything is read, let alone removed. A standard root that
    // cannot be proved safe is skipped loudly rather than scanned; one the user
    // named by hand is a hard error, because silently skipping it would read as
    // "nothing to reap".
    let mut vetted: Vec<VettedRoot> = Vec::new();
    for root in &temp_roots {
        match vet_root(root) {
            RootVerdict::Scan(scan) => vetted.push(VettedRoot {
                shown: root.path.clone(),
                scan,
                untagged_ok: root.untagged_ok,
            }),
            // A standard root that is simply absent is normal — a machine that
            // has never run the suite has no private parent yet.
            RootVerdict::Absent if !root.required => {}
            RootVerdict::Absent => {
                eprintln!(
                    "xtask clean-e2e-tmp: {} does not exist",
                    root.path.display()
                );
                return ExitCode::FAILURE;
            }
            RootVerdict::Refused(why) => {
                eprintln!("xtask clean-e2e-tmp: REFUSED to scan {why}.");
                eprintln!(
                    "  Nothing under it was read or removed. Look at it — \
                     `ls -ld {}` — and remove it by hand if it is yours.",
                    root.path.display(),
                );
                if root.required {
                    return ExitCode::FAILURE;
                }
            }
        }
    }

    let outcome = match sweep(&vetted, &searched, &opts, &SystemProbe) {
        Ok(outcome) => outcome,
        Err(e) => {
            eprintln!("xtask clean-e2e-tmp: {e}");
            return ExitCode::FAILURE;
        }
    };

    print!("{}", outcome.report);
    if outcome.removed > 0 || !outcome.failures.is_empty() {
        println!(
            "removed {} dir(s), freed {}",
            outcome.removed,
            human_size(outcome.freed, outcome.freed_truncated)
        );
    }
    for line in &outcome.failures {
        eprintln!("{line}");
    }
    if !outcome.failures.is_empty() {
        eprintln!("{} dir(s) could not be removed", outcome.failures.len());
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Returns the options plus the `--root` list. The roots ride alongside
/// `Options` rather than inside it so that `Options` — which `collect` reads and
/// which issue #461's rewrite of this file owns — stays untouched.
fn parse_args(args: &[String]) -> Result<Option<(Options, Vec<PathBuf>)>, String> {
    let mut opts = Options {
        max_age: Duration::from_secs(DEFAULT_MAX_AGE_HOURS * 3600),
        apply: false,
        include_untagged: false,
        ignore_liveness: false,
    };
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--apply" => opts.apply = true,
            "--include-untagged" => opts.include_untagged = true,
            "--ignore-liveness" => opts.ignore_liveness = true,
            "--root" => {
                let raw = it
                    .next()
                    .ok_or_else(|| "--root needs a directory".to_string())?;
                let path = PathBuf::from(raw);
                if !path.is_absolute() {
                    return Err(format!("--root needs an absolute path, got {raw:?}"));
                }
                roots.push(path);
            }
            "--older-than" => {
                let raw = it
                    .next()
                    .ok_or_else(|| "--older-than needs a value in hours".to_string())?;
                let hours: u64 = raw
                    .parse()
                    .map_err(|_| format!("--older-than expects whole hours, got {raw:?}"))?;
                // `saturating_mul`: the value is user input, and an hour count
                // near `u64::MAX` would otherwise panic the whole command in a
                // debug build before it can clean anything. Saturating gives an
                // absurdly distant threshold, which is what such input means.
                opts.max_age = Duration::from_secs(hours.saturating_mul(3600));
            }
            "-h" | "--help" => {
                usage();
                return Ok(None);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(Some((opts, roots)))
}

fn usage() {
    println!(
        "usage: cargo xtask clean-e2e-tmp [--older-than <hours>] [--apply] \
         [--include-untagged] [--ignore-liveness] [--root <dir>]..."
    );
    println!();
    println!("Reaps stale e2e harness temp dirs left by SIGKILLed test processes.");
    println!("Dry-run by default; --apply is required to delete.");
    println!();
    println!("Scans the standard roots — the harness's private /var/tmp/dad-e2e-<uid>");
    println!("parent and the system temp dir. It cannot infer another worktree's");
    println!("leftovers, so run it where the run that leaked them ran.");
    println!();
    println!("A `{PID_TAGGED_PREFIX}<pid>-*` root is decided by whether that PID is still");
    println!("alive. A dead owner is reaped once the root is at least");
    println!(
        "{}, a floor separate from --older-than that exists because a",
        human_duration(DEAD_PID_MIN_AGE)
    );
    println!("daemon the test spawned can outlive it and keep writing there. A live owner is");
    println!("never reaped. The --older-than threshold decides roots with no usable");
    println!("PID — untagged, `dad-unit-*`, lock dirs, malformed names, and hosts with");
    println!("no way to ask.");
    println!();
    println!("  --older-than <hours>  age threshold for the fallback cases only");
    println!(
        "                        (default: {DEFAULT_MAX_AGE_HOURS}). It does NOT hold back a dead PID."
    );
    println!("  --apply               actually remove the directories");
    println!("  --ignore-liveness     do NOT trust `live-pid`: judge those roots by age");
    println!("                        too. For when the machine has REBOOTED and a stale");
    println!("                        root's PID has been reused by an unrelated process,");
    println!("                        which otherwise pins it for the life of that boot.");
    println!("                        Never reap a live root while a suite is running.");
    println!("  --root <dir>          scan this absolute path INSTEAD of the standard");
    println!("                        roots. Repeatable. Needed for a base you moved");
    println!("                        with {TEMP_BASE_ENV}: where the harness may write");
    println!("                        and what this command may delete are separate");
    println!("                        decisions, so the override is never scanned");
    println!("                        automatically.");
    println!("  --include-untagged    ALSO reap `{UNTAGGED_PREFIX}*` dirs. These use the");
    println!("                        tempfile crate's DEFAULT prefix and are shared with");
    println!("                        every Rust program on this machine — only use this");
    println!("                        when no other Rust build or tool is running. Applies");
    println!("                        to the system temp dir and to any OTHER --root you");
    println!("                        name; never to the private parent, even when you");
    println!("                        name that parent with --root yourself.");
}

/// What one end-to-end pass over `temp_root` did.
struct Sweep {
    report: String,
    removed: usize,
    freed: u64,
    freed_truncated: bool,
    /// One message per directory that could not be removed; the caller decides
    /// where they go (stderr) and turns a non-empty list into a failing exit.
    failures: Vec<String>,
}

/// Collect, decide, report, and — only under `--apply` — delete, across every
/// vetted root.
///
/// This exists as one function so a test can drive the **real** deletion path
/// over a directory holding both a reapable and a kept root. `collect` returns
/// kept candidates too (the report has to explain survivors), which makes "only
/// the `reap` half is ever handed to `remove_dir_all`" the single
/// highest-consequence invariant in this file — and one that a test calling
/// `collect` alone can never observe.
///
/// Issue #322 widened this from one directory to the vetted set, so the walk and
/// the removal both run against `VettedRoot::scan` — the path vetting resolved —
/// and never against the spelling it was handed.
fn sweep(
    roots: &[VettedRoot],
    searched: &str,
    opts: &Options,
    probe: &dyn ProcessProbe,
) -> std::io::Result<Sweep> {
    let mut candidates = Vec::new();
    for root in roots {
        // Per-root view: `--include-untagged` is confined to the roots that
        // allow it (issue #322), so it is masked here rather than inside
        // `collect`.
        let scoped = Options {
            max_age: opts.max_age,
            apply: opts.apply,
            include_untagged: opts.include_untagged && root.untagged_ok,
            ignore_liveness: opts.ignore_liveness,
        };
        // `root.scan`, not `root.shown`: the resolved path vetting produced, so
        // the tree walked here is the tree vetting judged.
        match collect(&root.scan, &scoped, probe) {
            Ok(mut found) => candidates.append(&mut found),
            // Vetting already stat'ed it, so a `NotFound` here is a race with
            // something else removing the root — nothing left to reap.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(std::io::Error::new(
                    e.kind(),
                    format!("cannot read {}: {e}", root.shown.display()),
                ));
            }
        }
    }
    // `collect` sorts within one root; re-sort so the biggest offender is first
    // across all of them.
    candidates.sort_by_key(|c| std::cmp::Reverse(c.bytes));

    let (reap, keep): (Vec<Candidate>, Vec<Candidate>) =
        candidates.into_iter().partition(|c| c.verdict.reap);

    let mut out = report(searched, &reap, &keep, opts.max_age);
    let mut removed = 0usize;
    let mut freed = 0u64;
    let mut freed_truncated = false;
    let mut failures = Vec::new();

    if !reap.is_empty() {
        if opts.apply {
            for c in &reap {
                match std::fs::remove_dir_all(&c.path) {
                    Ok(()) => {
                        removed += 1;
                        // Saturating, like every other size accumulation here:
                        // apparent sizes are attacker-influenced (sparse files
                        // cost no blocks), and a panicking total would abort a
                        // sweep that has already deleted part of its work list.
                        freed = freed.saturating_add(c.bytes);
                        freed_truncated |= c.size_truncated;
                    }
                    // `{:?}` not `.display()`: see `report`.
                    Err(e) => failures.push(format!("  failed to remove {:?}: {e}", c.path)),
                }
            }
        } else {
            let _ = writeln!(out);
            let _ = writeln!(
                out,
                "dry run — nothing removed. Re-run with --apply to delete."
            );
        }
    }

    Ok(Sweep {
        report: out,
        removed,
        freed,
        freed_truncated,
        failures,
    })
}

fn collect(
    temp_root: &Path,
    opts: &Options,
    probe: &dyn ProcessProbe,
) -> std::io::Result<Vec<Candidate>> {
    let now = SystemTime::now();
    let mut out = Vec::new();
    for entry in std::fs::read_dir(temp_root)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        // `symlink_metadata` so a symlink is never mistaken for a directory —
        // a planted `dad-tests-*` symlink must not redirect the walk or the
        // removal outside the temp root.
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_owned(name, opts.include_untagged) {
            continue;
        }
        // mtime is the only timestamp this tool reads, and it feeds the age
        // fallback alone. Ownership is decided from the PID with no timestamp
        // involved at all.
        let age = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .unwrap_or_default();
        // Every owned dir is collected, kept ones included: the report has to
        // be able to say WHY a root survived, which it cannot do for entries
        // that were filtered away before they were ever seen.
        let verdict = classify(
            owner_of(name, probe),
            age,
            opts.max_age,
            opts.ignore_liveness,
        );
        let size = dir_size(&path);
        out.push(Candidate {
            bytes: size.bytes,
            size_truncated: size.truncated,
            path,
            age,
            verdict,
        });
    }
    // Biggest first. `Reverse` rather than a flipped `cmp` so
    // `unnecessary_sort_by` stays quiet under the workspace-wide clippy
    // gate (issue #436) — same ordering, same stability.
    out.sort_by_key(|c| std::cmp::Reverse(c.bytes));
    Ok(out)
}

fn is_owned(name: &str, include_untagged: bool) -> bool {
    if OWNED_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return true;
    }
    include_untagged && name.starts_with(UNTAGGED_PREFIX)
}

/// The whole reap/keep decision, kept free of the filesystem so it can be
/// exercised directly for every combination of ownership and age.
fn classify(owner: Owner, age: Duration, max_age: Duration, ignore_liveness: bool) -> Verdict {
    match owner {
        // Not subject to `--older-than` — that is the case issue #461 was filed
        // for, where 6.2 GB of provably-dead roots were refused for being four
        // hours old. But it IS subject to a short floor of its own: the owning
        // PID is the test process, and a daemon it spawned can outlive it by up
        // to `MAX_PINNED_ORPHAN_CAP_SECS` while still writing here. See
        // `DEAD_PID_MIN_AGE`.
        Owner::Dead => Verdict {
            reap: age >= DEAD_PID_MIN_AGE,
            reason: Reason::DeadPid,
        },
        // Unconditional by default, and the reason no *process* timestamp
        // appears in this function: every attempt to qualify this branch by
        // guessing whether the PID had been reused ended up deleting live
        // roots. `--ignore-liveness` is the operator-driven form of the same
        // escape — it demotes the root to the age rule rather than reaping it
        // outright, and it is never inferred.
        Owner::Live => {
            if ignore_liveness {
                Verdict {
                    reap: age >= max_age,
                    reason: Reason::LivenessIgnored,
                }
            } else {
                Verdict {
                    reap: false,
                    reason: Reason::LivePid,
                }
            }
        }
        Owner::Unknown => Verdict {
            reap: age >= max_age,
            reason: Reason::UntaggedAge,
        },
    }
}

/// Classify a root from the PID in its name and nothing else.
///
/// No filesystem timestamp reaches this function. A live PID means keep, at any
/// age and whatever the root's timestamps say; see the module docs for why the
/// recycled-PID branch was removed rather than repaired.
fn owner_of(name: &str, probe: &dyn ProcessProbe) -> Owner {
    let Some(pid) = parse_pid(name) else {
        return Owner::Unknown;
    };
    match probe.is_alive(pid) {
        Some(false) => Owner::Dead,
        Some(true) => Owner::Live,
        // The platform cannot answer, so fall back to age exactly as an
        // untagged name would.
        None => Owner::Unknown,
    }
}

/// The `<pid>` out of a `dad-tests-<pid>-<random>` name. `None` for every other
/// shape, including the pre-fix lock dirs and untagged `.tmp*` names, which
/// carry no PID at all.
fn parse_pid(name: &str) -> Option<i32> {
    let rest = name.strip_prefix(PID_TAGGED_PREFIX)?;
    let digits = rest.split('-').next()?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // PID 0 is never a real process here, and `kill(0, 0)` addresses the whole
    // process group rather than one process — reject it rather than ask.
    digits.parse::<i32>().ok().filter(|pid| *pid > 0)
}

/// `Some(true)` alive, `Some(false)` dead, `None` where this platform cannot
/// tell.
///
/// `kill(pid, 0)` runs the existence and permission checks and sends no signal.
/// `ESRCH` is the ONLY answer that means dead: `EPERM` means the process exists
/// and simply is not ours, and reading that as dead would delete a live run's
/// root. Anything else unexpected is treated as alive for the same reason.
///
/// This is available on every Unix, not just Linux. Only a genuinely non-Unix
/// host — where there is no `kill(2)` to ask — returns `None` and falls back to
/// the age rule.
#[cfg(unix)]
fn pid_is_alive(pid: i32) -> Option<bool> {
    // SAFETY: `kill` with signal 0 sends nothing and touches no memory; `pid`
    // is a positive integer, so this can never address a process group.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Some(true);
    }
    Some(std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH))
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: i32) -> Option<bool> {
    None
}

/// The human-facing report, built as a string so a test can assert that it
/// attributes each decision. Issue #461: the old output said "no owned dirs
/// older than 6h" — an age fact, when the question was ownership.
///
/// Candidate paths are rendered with `{:?}`, never `.display()`. `/tmp` is mode
/// 1777, so any local user can create a `dad-tests-<dead-pid>-<suffix>` whose
/// suffix carries a newline, a CSI sequence, or an OSC one the terminal may act
/// on (OSC 52 writes the clipboard). Such a name reaches the report in the
/// **default dry run**, and needs only to clear the ten-minute floor rather than
/// the old six-hour threshold — and a *kept* root is listed now too, so an
/// attacker does not even need the name to be reapable. `Path`'s `Debug` escapes
/// control characters; `Display` passes them through verbatim.
fn report(searched: &str, reap: &[Candidate], keep: &[Candidate], max_age: Duration) -> String {
    let mut out = String::new();
    if reap.is_empty() && keep.is_empty() {
        let _ = writeln!(
            out,
            "nothing to reap in {searched} (no dirs this repo owns)"
        );
        return out;
    }

    write_listing(&mut out, reap);

    if reap.is_empty() {
        let _ = writeln!(out, "nothing to reap in {searched}");
    } else {
        let _ = writeln!(
            out,
            "reap: {} dir(s), {} in {searched}",
            reap.len(),
            total_size(reap),
        );
        write_breakdown(&mut out, reap, true, max_age);
    }

    if !keep.is_empty() {
        // Survivors are listed, not merely counted. A `live-pid` root that is
        // really a PID collision across a reboot is indistinguishable from a
        // running suite's root in a bare count, and it is the one class the
        // reaper will not settle on its own — so the operator needs to see its
        // size and age to decide whether `--ignore-liveness` is warranted.
        let _ = writeln!(out, "kept:");
        write_listing(&mut out, keep);
        let _ = writeln!(out, "keep: {} dir(s), {}", keep.len(), total_size(keep));
        write_breakdown(&mut out, keep, false, max_age);
    }

    if reap.iter().chain(keep).any(|c| c.size_truncated) {
        let _ = writeln!(
            out,
            "  sizes shown with ≥ are lower bounds: the walk stopped at {MAX_SIZE_WALK_ENTRIES} entries or depth {MAX_SIZE_WALK_DEPTH}. Size is reporting only — it never changes a reap/keep decision."
        );
    }
    out
}

/// The per-directory lines for one group, capped at [`MAX_LISTED`] with the
/// truncation announcing itself. Shared by the reap and keep groups so a
/// survivor is described exactly as precisely as a casualty.
fn write_listing(out: &mut String, cands: &[Candidate]) {
    for c in cands.iter().take(MAX_LISTED) {
        let _ = writeln!(
            out,
            "  {:>9}  {:<9} {:>5} old  {:?}",
            human_size(c.bytes, c.size_truncated),
            c.verdict.reason.label(),
            human_duration(c.age),
            c.path,
        );
    }
    if cands.len() > MAX_LISTED {
        let _ = writeln!(
            out,
            "  … and {} more (all counted in the summary below)",
            cands.len() - MAX_LISTED
        );
    }
}

/// Counts and sizes per reason — the part that stays readable when there are
/// 280 candidates.
fn write_breakdown(out: &mut String, cands: &[Candidate], reap: bool, max_age: Duration) {
    for reason in Reason::ALL {
        let matching: Vec<&Candidate> = cands
            .iter()
            .filter(|c| c.verdict.reason == reason)
            .collect();
        if matching.is_empty() {
            continue;
        }
        let bytes = sum_bytes(matching.iter().map(|c| c.bytes));
        let truncated = matching.iter().any(|c| c.size_truncated);
        let _ = writeln!(
            out,
            "  {:<9} {:>4} dir(s)  {:>9}  {}",
            reason.label(),
            matching.len(),
            human_size(bytes, truncated),
            reason.note(reap, max_age),
        );
    }
}

/// Summed size of a group, carrying the lower-bound marker if any member's walk
/// was truncated.
fn total_size(cands: &[Candidate]) -> String {
    human_size(
        sum_bytes(cands.iter().map(|c| c.bytes)),
        cands.iter().any(|c| c.size_truncated),
    )
}

/// Every size total in this file goes through here, and it saturates rather
/// than wrapping.
///
/// `Iterator::sum` panics on overflow in a debug build and wraps silently in a
/// release one, and these are *apparent* sizes: a sparse file costs no blocks,
/// so any local user can park three 8-exabyte files in a world-writable `/tmp`
/// under a `dad-tests-*` name and overflow a `u64`. The auditor did exactly
/// that and aborted the plain `cargo xtask clean-e2e-tmp` with `attempt to add
/// with overflow` before it could clean anything — which defeats the whole
/// point of a tool you reach for when the machine is already in trouble.
/// Saturating prints an implausible number; panicking or wrapping prints a
/// wrong one or nothing at all.
fn sum_bytes(sizes: impl Iterator<Item = u64>) -> u64 {
    sizes.fold(0u64, |acc, b| acc.saturating_add(b))
}

/// Apparent size of one tree, plus whether the walk gave up before finishing.
struct DirSize {
    bytes: u64,
    truncated: bool,
}

/// Recursive apparent size, bounded by [`MAX_SIZE_WALK_ENTRIES`] and
/// [`MAX_SIZE_WALK_DEPTH`].
///
/// # What the symlink handling does and does not guarantee
///
/// Every entry is stat'd with `symlink_metadata`, so **a symlink is never
/// descended as observed**: it contributes its own length and nothing more, and
/// a symlink loop cannot spin the walk. That is a statement about what the walk
/// *sees*, not a race-free guarantee, and the earlier flat claim that it "never
/// follows symlinks" was too absolute.
///
/// `read_dir` resolves by path. Between the moment a child is observed to be a
/// directory and the moment it is opened, a local user with write access inside
/// the tree can replace that path with a symlink, and the open follows the
/// replacement — so the *sizing* walk can be steered outside the candidate. The
/// `symlink_metadata` re-check below catches a swap that is still in place when
/// the directory is opened; a swap undone again inside that window is not
/// detected.
///
/// The residual consequence is bounded and presentation-only: at worst some
/// other tree's entries are counted into a size, capped by the entry and depth
/// budgets. No name and no content from outside the candidate is ever printed,
/// and sizing never reaches [`classify`], so it cannot move a reap/keep verdict.
///
/// It is not a deletion escape either — but **not** because [`collect`] `lstat`ed
/// the path. That check and the removal are separated by the whole classify and
/// size-walk pass, and in a mode-1777 `/tmp` a user can `rename(2)` their own
/// entry into a symlink inside that window (the sticky bit does not help: they
/// are modifying an entry they own). The removal is safe because
/// `std::fs::remove_dir_all` does not follow a symlink at the path it is given
/// and has been `openat`-based since the CVE-2022-21658 fix. Attributing the
/// safety to the earlier `lstat` would license replacing it with a hand-rolled
/// recursive delete, which is precisely where this bug class gets introduced.
fn dir_size(path: &Path) -> DirSize {
    dir_size_bounded(path, MAX_SIZE_WALK_ENTRIES, MAX_SIZE_WALK_DEPTH)
}

fn dir_size_bounded(path: &Path, max_entries: usize, max_depth: usize) -> DirSize {
    let mut bytes = 0u64;
    let mut seen = 0usize;
    let mut truncated = false;
    let mut stack = vec![(path.to_path_buf(), 0usize)];
    'walk: while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        // The path was a real directory when it was queued, but `read_dir`
        // re-resolved it by name and would have followed a symlink swapped in
        // since. Re-stat before spending any of the budget here: it costs one
        // `lstat` per directory and rejects a swap that is still in place.
        if !std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        for entry in rd.flatten() {
            seen += 1;
            if seen > max_entries {
                truncated = true;
                break 'walk;
            }
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_dir() {
                if depth + 1 > max_depth {
                    truncated = true;
                    continue;
                }
                stack.push((entry.path(), depth + 1));
            } else {
                // Saturating: see `sum_bytes`. `meta.len()` is the apparent
                // size, so three sparse files are enough to overflow a `u64`.
                bytes = bytes.saturating_add(meta.len());
            }
        }
    }
    DirSize { bytes, truncated }
}

/// A size, marked `≥` when the walk that produced it was cut short.
fn human_size(bytes: u64, truncated: bool) -> String {
    if truncated {
        format!("≥ {}", human_bytes(bytes))
    } else {
        human_bytes(bytes)
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Minutes below an hour, hours below two days, then days.
///
/// The sub-hour case is not cosmetic. Now that a dead owner is reaped from
/// [`DEAD_PID_MIN_AGE`] rather than from six hours, most rows on a freshly-dead
/// machine are minutes old, and truncating to whole hours printed every one of
/// them as `0h` — including the `DEAD_PID_MIN_AGE` threshold itself, which made
/// the floor's own message read "older than 0h".
fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let hours = secs / 3600;
    if hours == 0 {
        format!("{}m", secs / 60)
    } else if hours < 48 {
        format!("{hours}h")
    } else {
        format!("{}d", hours / 24)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// The vetted-root list for a scratch directory, so the classification
    /// tests can drive the real [`sweep`] without building a `/var/tmp` fixture.
    /// Vetting itself is exercised by the root tests further down.
    fn one_root(path: &Path) -> Vec<VettedRoot> {
        vec![VettedRoot {
            shown: path.to_path_buf(),
            scan: path.to_path_buf(),
            untagged_ok: true,
        }]
    }

    fn opts(max_age: Duration) -> Options {
        Options {
            max_age,
            apply: false,
            include_untagged: false,
            ignore_liveness: false,
        }
    }

    const HOUR: Duration = Duration::from_secs(3600);

    fn candidate(name: &str, bytes: u64, age_hours: u32, verdict: Verdict) -> Candidate {
        Candidate {
            path: Path::new("/tmp").join(name),
            bytes,
            size_truncated: false,
            age: HOUR * age_hours,
            verdict,
        }
    }

    /// One scripted answer for every PID (issue #461 review, item 3). Real PIDs
    /// cannot express "dead" without a race — the kernel may hand a reaped
    /// number to someone else before the probe runs.
    struct FakeProbe(Option<bool>);

    impl FakeProbe {
        fn dead() -> Self {
            Self(Some(false))
        }
        fn live() -> Self {
            Self(Some(true))
        }
        fn unanswerable() -> Self {
            Self(None)
        }
    }

    impl ProcessProbe for FakeProbe {
        fn is_alive(&self, _pid: i32) -> Option<bool> {
            self.0
        }
    }

    /// A probe that answers per PID, for tests that need a reapable root and a
    /// kept root side by side in one directory.
    struct DeadPids(Vec<i32>);

    impl ProcessProbe for DeadPids {
        fn is_alive(&self, pid: i32) -> Option<bool> {
            Some(!self.0.contains(&pid))
        }
    }

    /// Force a directory's **mtime**, and only its mtime, so a test can put
    /// that one timestamp anywhere on the wall clock — including the future,
    /// which no amount of waiting can produce.
    ///
    /// It is not the root's only timestamp. `FileTimes` reaches mtime and atime;
    /// nothing portable reaches **birth time**, and on a filesystem that records
    /// one — the tmpfs this usually runs on does — the directory keeps a
    /// creation time of "just now" whatever this sets. That is precisely why no
    /// behavioural test can catch a reinstated start-time comparison, and why
    /// the guard is `source_has_no_pid_recycling_machinery` instead.
    fn set_mtime(dir: &Path, when: SystemTime) {
        let handle = std::fs::File::open(dir).expect("open dir");
        handle
            .set_times(std::fs::FileTimes::new().set_modified(when))
            .expect("set mtime");
    }

    /// A file of `len` apparent bytes that occupies (almost) no blocks, so a
    /// test can build an exabyte-scale tree in milliseconds — the same trick
    /// available to any local user with write access to `/tmp`.
    fn sparse_file(path: &Path, len: u64) {
        std::fs::File::create(path)
            .expect("create sparse file")
            .set_len(len)
            .expect("set_len");
    }

    /// A temp dir on a filesystem that will actually accept a `len`-byte sparse
    /// file, or `None` if no candidate will.
    ///
    /// `set_len` is bounded by the filesystem's **maximum file size**, not by
    /// free space, and the two candidates differ by five orders of magnitude:
    /// ext4 stops at 16 TiB and answers `EFBIG`, while tmpfs and APFS go to
    /// ~8 EiB. So the exabyte fixture below is not portable to a `/tmp` that
    /// sits on ext4 — which is exactly CI's Linux runner, while a dev box with a
    /// RAM-backed `/tmp` (CLAUDE.md rule 14) hosts it happily.
    ///
    /// That split stayed invisible until issue #489 put the workspace's tests in
    /// a gate: the test passed on every machine that ran it and had never once
    /// run in CI. `/dev/shm` is tried first because it is a tmpfs on every
    /// normal Linux regardless of what `/tmp` is mounted on; the default temp
    /// dir covers macOS, where APFS is already large enough.
    fn sparse_capable_tempdir(len: u64) -> Option<tempfile::TempDir> {
        let mut bases: Vec<PathBuf> = Vec::new();
        let shm = PathBuf::from("/dev/shm");
        if shm.is_dir() {
            bases.push(shm);
        }
        bases.push(std::env::temp_dir());
        for base in bases {
            let Ok(dir) = tempfile::tempdir_in(&base) else {
                continue;
            };
            let probe = dir.path().join(".sparse-probe");
            let accepted = std::fs::File::create(&probe)
                .and_then(|f| f.set_len(len))
                .is_ok();
            let _ = std::fs::remove_file(&probe);
            if accepted {
                return Some(dir);
            }
        }
        None
    }

    #[test]
    fn owned_prefixes_are_reaped_by_default() {
        assert!(is_owned("dad-tests-1234-AbCdEf", false));
        assert!(is_owned("dot-agent-deck-test-lock-AbCdEf", false));
        // `src/test_temp.rs` dirs (issue #322). Without this they would be
        // unreclaimable: they live under the private `/var/tmp` parent, which
        // `--include-untagged` deliberately never reaches. They carry no PID,
        // so the age rule decides them.
        assert!(is_owned("dad-unit-AbCdEf", false));
    }

    /// The tempfile crate's default prefix belongs to every Rust program on the
    /// machine, so reaping it must stay opt-in — this is the guard against a
    /// prune helper deleting another tool's live temp dir.
    #[test]
    fn untagged_tempfile_prefix_is_opt_in() {
        assert!(!is_owned(".tmpAbCdEf", false));
        assert!(is_owned(".tmpAbCdEf", true));
    }

    #[test]
    fn unrelated_names_are_never_reaped() {
        for name in ["systemd-private-abc", "dad-screenshot.txt", "opencode"] {
            assert!(!is_owned(name, true), "{name} should not be reaped");
        }
    }

    /// A symlink named like an owned dir must not be collected — otherwise the
    /// reaper could be pointed at a tree outside the temp root.
    #[cfg(unix)]
    #[test]
    fn symlinks_named_like_owned_dirs_are_skipped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("real-target");
        std::fs::create_dir(&target).expect("create target");
        std::os::unix::fs::symlink(&target, tmp.path().join("dad-tests-1-lnk"))
            .expect("create symlink");
        let found =
            collect(tmp.path(), &opts(Duration::ZERO), &FakeProbe::dead()).expect("collect");
        assert!(
            found.is_empty(),
            "symlink was collected: {:?}",
            found.iter().map(|c| c.path.clone()).collect::<Vec<_>>()
        );
        assert!(target.exists(), "target must be untouched");
    }

    /// Issue #461, requirement 1: the owning PID is read straight out of the
    /// name, and anything that is not that shape yields no PID at all.
    #[test]
    fn the_owning_pid_is_read_out_of_the_root_name() {
        assert_eq!(parse_pid("dad-tests-12345-AbCdEf"), Some(12345));
        assert_eq!(parse_pid("dad-tests-7-a-b-c"), Some(7));
        for name in [
            ".tmpAbCdEf",
            "dot-agent-deck-test-lock-AbCdEf",
            "dad-tests-",
            "dad-tests--AbCdEf",
            "dad-tests-notapid-AbCdEf",
            "dad-tests-12x-AbCdEf",
            "dad-tests-0-AbCdEf",
            "dad-tests-99999999999999-AbCdEf",
        ] {
            assert_eq!(parse_pid(name), None, "{name} must not yield a PID");
        }
    }

    /// The branches of the decision, with no filesystem in the way. A dead owner
    /// ignores `--older-than` but not its own short floor; a live owner ignores
    /// age entirely; only the fallbacks are decided by `--older-than`.
    #[test]
    fn ownership_decides_first_and_age_only_where_it_cannot() {
        let max_age = HOUR * 6;
        let fresh = HOUR;
        let stale = HOUR * 9;

        for age in [fresh, stale] {
            assert_eq!(
                classify(Owner::Dead, age, max_age, false),
                Verdict {
                    reap: true,
                    reason: Reason::DeadPid
                },
                "a dead owner past the floor is reaped however `--older-than` is set"
            );
            assert_eq!(
                classify(Owner::Live, age, max_age, false),
                Verdict {
                    reap: false,
                    reason: Reason::LivePid
                },
                "a live owner is kept at any age"
            );
        }

        assert_eq!(
            classify(Owner::Unknown, stale, max_age, false),
            Verdict {
                reap: true,
                reason: Reason::UntaggedAge
            }
        );
        assert_eq!(
            classify(Owner::Unknown, fresh, max_age, false),
            Verdict {
                reap: false,
                reason: Reason::UntaggedAge
            }
        );
    }

    /// The orphan window. The owning PID names the *test* process, but a daemon
    /// it spawned `setsid`s out of the group and can outlive it by up to
    /// [`MAX_PINNED_ORPHAN_CAP_SECS`] while still writing under that root — so a
    /// dead owner is NOT reaped instantly. Below the floor the root is kept and
    /// the reason says why; above it, reaping resumes and `--older-than` still
    /// cannot hold it back.
    #[test]
    fn a_dead_owner_is_not_reaped_inside_the_orphan_window() {
        let generous = HOUR * 24;

        for age in [
            Duration::ZERO,
            Duration::from_secs(1),
            DEAD_PID_MIN_AGE - Duration::from_secs(1),
        ] {
            assert_eq!(
                classify(Owner::Dead, age, generous, false),
                Verdict {
                    reap: false,
                    reason: Reason::DeadPid
                },
                "a dead owner younger than the floor must be kept ({age:?})"
            );
        }

        for age in [DEAD_PID_MIN_AGE, DEAD_PID_MIN_AGE + Duration::from_secs(1)] {
            assert_eq!(
                classify(Owner::Dead, age, generous, false),
                Verdict {
                    reap: true,
                    reason: Reason::DeadPid
                },
                "at or past the floor a dead owner is reaped despite --older-than 24h ({age:?})"
            );
        }

        // The floor must stay far above the orphan cap it exists for, and far
        // below the age threshold it is not a substitute for. Issue #679: the
        // first of these used to be a bare `>= 600`, which is what let the floor
        // read as "2x the cap" while the longest cap in the repo was already
        // 900. Pinning it to the derivation instead means a cap raise that
        // forgets the floor cannot pass here either.
        assert_eq!(
            DEAD_PID_MIN_AGE,
            Duration::from_secs(MAX_PINNED_ORPHAN_CAP_SECS) * 2,
            "the floor is 2x the longest orphan cap, not 2x the 300s default"
        );
        assert!(DEAD_PID_MIN_AGE >= Duration::from_secs(MAX_PINNED_ORPHAN_CAP_SECS));
        assert!(DEAD_PID_MIN_AGE < Duration::from_secs(DEFAULT_MAX_AGE_HOURS * 3600));

        // Both keep-side notes name the floor, not `--older-than`, so the report
        // cannot blame the wrong threshold.
        let note = Reason::DeadPid.note(false, generous);
        assert!(note.contains(&human_duration(DEAD_PID_MIN_AGE)), "{note}");
        assert!(!note.contains("24h"), "{note}");
    }

    /// `--ignore-liveness` is the operator's answer to a PID reused across a
    /// reboot: it demotes a live-PID root to the age rule rather than reaping it
    /// outright, and it is never inferred. Default off.
    #[test]
    fn ignore_liveness_demotes_a_live_pid_to_the_age_rule() {
        let max_age = HOUR * 6;

        assert_eq!(
            classify(Owner::Live, HOUR * 9, max_age, true),
            Verdict {
                reap: true,
                reason: Reason::LivenessIgnored
            },
            "past the threshold, an untrusted live PID is reapable"
        );
        assert_eq!(
            classify(Owner::Live, HOUR, max_age, true),
            Verdict {
                reap: false,
                reason: Reason::LivenessIgnored
            },
            "the flag demotes to the age rule — it does not reap unconditionally"
        );
        // Off by default, and it is a keep at any age without it.
        assert!(
            !parse_args(&[])
                .expect("parse")
                .expect("opts")
                .0
                .ignore_liveness
        );
        assert_eq!(
            classify(Owner::Live, HOUR * 9, max_age, false),
            Verdict {
                reap: false,
                reason: Reason::LivePid
            }
        );
        assert!(
            parse_args(&["--ignore-liveness".to_string()])
                .expect("parse")
                .expect("opts")
                .0
                .ignore_liveness
        );
        // A dead owner's floor is unaffected by the flag.
        assert!(!classify(Owner::Dead, Duration::ZERO, max_age, true).reap);
    }

    /// Ownership comes from the PID and from nothing else — there is no
    /// recycled branch to reach, so a live PID has exactly one outcome. A
    /// platform that cannot answer liveness is *not* a keep: it falls back to
    /// the age rule exactly as an untagged name does.
    #[test]
    fn a_live_pid_has_exactly_one_outcome_and_an_unanswerable_one_falls_back() {
        let name = "dad-tests-4242-AbCdEf";
        assert_eq!(owner_of(name, &FakeProbe::live()), Owner::Live);
        assert_eq!(owner_of(name, &FakeProbe::dead()), Owner::Dead);
        assert_eq!(owner_of(name, &FakeProbe::unanswerable()), Owner::Unknown);
        assert_eq!(owner_of(".tmpAbCdEf", &FakeProbe::live()), Owner::Unknown);
    }

    /// Issue #461's headline case: 280 roots whose owners were provably gone
    /// were refused because the oldest was under the six-hour default. A dead
    /// PID must outvote any `--older-than`, however generous — the only thing
    /// that holds it back is its own short orphan-window floor, so the root here
    /// is aged past that and nothing else changes.
    #[cfg(unix)]
    #[test]
    fn a_dead_pid_is_reaped_even_when_younger_than_the_threshold() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("dad-tests-4242-AbCdEf");
        std::fs::create_dir(&dir).expect("create");
        std::fs::write(dir.join("payload"), vec![0u8; 2048]).expect("write");
        // Past the orphan floor, but still far under the 24h threshold below —
        // which is the whole point of the assertion.
        set_mtime(&dir, SystemTime::now() - DEAD_PID_MIN_AGE * 2);

        let found = collect(tmp.path(), &opts(HOUR * 24), &FakeProbe::dead()).expect("collect");
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].verdict,
            Verdict {
                reap: true,
                reason: Reason::DeadPid
            }
        );
        assert!(found[0].age < HOUR * 24, "must be under the threshold");
        assert_eq!(found[0].bytes, 2048);
        assert!(!found[0].size_truncated);
    }

    /// The floor, driven through the real filesystem: a root whose owner just
    /// died is kept, because a `setsid`'d daemon may still be writing under it.
    /// This is the case the old six-hour rule protected by accident and an
    /// unconditional dead-PID reap would have regressed.
    #[test]
    fn a_freshly_dead_root_is_kept_through_collect() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("dad-tests-4242-JustDied");
        std::fs::create_dir(&dir).expect("create");

        // Zero threshold: only the orphan floor can be keeping this.
        let found =
            collect(tmp.path(), &opts(Duration::ZERO), &FakeProbe::dead()).expect("collect");
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].verdict,
            Verdict {
                reap: false,
                reason: Reason::DeadPid
            }
        );

        let applied = sweep(
            &one_root(tmp.path()),
            "scratch",
            &Options {
                max_age: Duration::ZERO,
                apply: true,
                include_untagged: false,
                ignore_liveness: false,
            },
            &FakeProbe::dead(),
        )
        .expect("apply");
        assert_eq!(applied.removed, 0, "{}", applied.report);
        assert!(dir.exists(), "a just-died root must survive --apply");
        assert!(
            applied.report.contains("may still be writing"),
            "the report must explain the orphan window:\n{}",
            applied.report
        );
    }

    /// The other half of the fix, driven by the REAL probe: a suite still
    /// running past the threshold used to be eligible to have its own scratch
    /// space deleted out from under it.
    #[cfg(unix)]
    #[test]
    fn a_live_pid_is_kept_even_when_older_than_the_threshold() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp
            .path()
            .join(format!("dad-tests-{}-AbCdEf", std::process::id()));
        std::fs::create_dir(&dir).expect("create");

        // A zero threshold makes every dir "old enough", so only ownership can
        // be keeping this one.
        let found = collect(tmp.path(), &opts(Duration::ZERO), &SystemProbe).expect("collect");
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].verdict,
            Verdict {
                reap: false,
                reason: Reason::LivePid
            }
        );
        assert!(dir.exists());
    }

    /// A live owner keeps its root at a zero threshold, through the real
    /// deletion path and the real `kill(pid, 0)` probe, **whatever mtime the
    /// root carries** — decades stale or decades in the future. Ancient mtime is
    /// what a long-running suite looks like once a `--older-than 0` sweep comes
    /// past; a future mtime is the shape a forward clock step leaves behind.
    ///
    /// **This is not a guard against the deleted start-time comparison, and must
    /// not be read as one.** [`set_mtime`] moves mtime only: both roots keep a
    /// birth time of "just now", so a restored `meta.created().ok().or(mtime)`
    /// comparison would find the current process older than both, classify both
    /// live, and leave this test green. Nothing a test can build changes that —
    /// see the module docs and `source_has_no_pid_recycling_machinery`, which is
    /// the guard.
    #[cfg(unix)]
    #[test]
    fn a_live_pid_is_kept_whatever_mtime_the_root_carries() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pid = std::process::id();
        let ancient = tmp.path().join(format!("dad-tests-{pid}-ancient"));
        let futuristic = tmp.path().join(format!("dad-tests-{pid}-futuristic"));
        for dir in [&ancient, &futuristic] {
            std::fs::create_dir(dir).expect("create");
            std::fs::write(dir.join("payload"), vec![0u8; 512]).expect("write");
        }
        // An mtime decades before this process could possibly have started…
        set_mtime(&ancient, SystemTime::UNIX_EPOCH + Duration::from_secs(1));
        // …and one decades after it, the shape a forward clock step leaves on an
        // otherwise ordinary root. Only mtime moves; both birth times stay put.
        set_mtime(
            &futuristic,
            SystemTime::now() + Duration::from_secs(50 * 365 * 24 * 3600),
        );

        let applied = sweep(
            &one_root(tmp.path()),
            "scratch",
            &Options {
                max_age: Duration::ZERO,
                apply: true,
                include_untagged: false,
                ignore_liveness: false,
            },
            &SystemProbe,
        )
        .expect("apply");

        assert_eq!(applied.removed, 0, "{}", applied.report);
        assert!(
            ancient.exists(),
            "a live owner's root must survive an ancient mtime"
        );
        assert!(
            futuristic.exists(),
            "a live owner's root must survive a future mtime too"
        );
        assert!(
            !applied.report.contains("reap:"),
            "nothing may be listed for reaping:\n{}",
            applied.report
        );
        assert!(applied.report.contains("live-pid"), "{}", applied.report);

        // The mirror image, with one honest asymmetry. An *ancient* mtime cannot
        // save a dead owner — `--older-than 24h` does not hold it back. But a
        // *future* mtime yields an age of zero (`duration_since` fails, so the
        // age saturates at nothing), which lands under the orphan floor and is
        // therefore KEPT. That is the fail-safe direction, and it is the reason
        // this uses a past mtime where it once used a future one.
        let dead_tmp = tempfile::tempdir().expect("tempdir");
        let dead = dead_tmp.path().join("dad-tests-4242-ancient");
        std::fs::create_dir(&dead).expect("create");
        set_mtime(&dead, SystemTime::UNIX_EPOCH + Duration::from_secs(1));
        let found =
            collect(dead_tmp.path(), &opts(HOUR * 24), &FakeProbe::dead()).expect("collect");
        assert_eq!(
            found[0].verdict,
            Verdict {
                reap: true,
                reason: Reason::DeadPid
            },
            "an ancient mtime cannot save a dead owner from --older-than 24h"
        );

        // And the future-mtime case, spelled out rather than left implicit.
        let future = dead_tmp.path().join("dad-tests-4243-futuristic");
        std::fs::create_dir(&future).expect("create");
        set_mtime(
            &future,
            SystemTime::now() + Duration::from_secs(365 * 24 * 3600),
        );
        let found =
            collect(dead_tmp.path(), &opts(Duration::ZERO), &FakeProbe::dead()).expect("collect");
        let fut = found
            .iter()
            .find(|c| c.path == future)
            .expect("future root collected");
        assert_eq!(
            fut.verdict,
            Verdict {
                reap: false,
                reason: Reason::DeadPid
            },
            "a future mtime reads as age zero, which the orphan floor keeps — the safe direction"
        );
    }

    /// Names carrying no usable PID — the pre-fix lock dirs, untagged `.tmp*`
    /// dirs, and malformed roots — keep the original age behaviour in both
    /// directions.
    #[test]
    fn names_without_a_usable_pid_fall_back_to_the_age_rule() {
        for name in [
            "dot-agent-deck-test-lock-AbCdEf",
            "dad-tests-notapid-AbCdEf",
            "dad-tests--AbCdEf",
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir(tmp.path().join(name)).expect("create");

            let stale =
                collect(tmp.path(), &opts(Duration::ZERO), &FakeProbe::dead()).expect("collect");
            assert_eq!(
                stale[0].verdict,
                Verdict {
                    reap: true,
                    reason: Reason::UntaggedAge
                },
                "{name} past the threshold"
            );

            let fresh = collect(tmp.path(), &opts(HOUR), &FakeProbe::dead()).expect("collect");
            assert_eq!(
                fresh[0].verdict,
                Verdict {
                    reap: false,
                    reason: Reason::UntaggedAge
                },
                "{name} under the threshold"
            );
        }
    }

    /// The invariant that matters most now that `collect` returns kept
    /// candidates too: only the `reap` half is ever handed to `remove_dir_all`.
    /// A test that stops at `collect` cannot see this — it has to drive the
    /// real apply path.
    #[cfg(unix)]
    #[test]
    fn apply_removes_only_the_reap_slice() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let doomed = tmp.path().join("dad-tests-111-AbCdEf");
        let spared = tmp.path().join("dad-tests-222-GhIjKl");
        for dir in [&doomed, &spared] {
            std::fs::create_dir(dir).expect("create");
            std::fs::write(dir.join("payload"), vec![0u8; 1024]).expect("write");
            // Past the orphan floor, so the dead-owner root is genuinely
            // reapable and this test is about the reap/keep SPLIT, not the floor.
            set_mtime(dir, SystemTime::now() - DEAD_PID_MIN_AGE * 2);
        }
        let probe = DeadPids(vec![111]);

        let dry = sweep(
            &one_root(tmp.path()),
            "scratch",
            &opts(Duration::ZERO),
            &probe,
        )
        .expect("dry run");
        assert_eq!(dry.removed, 0, "a dry run must delete nothing");
        assert!(doomed.exists() && spared.exists());
        assert!(dry.report.contains("dry run"), "{}", dry.report);

        let applied = sweep(
            &one_root(tmp.path()),
            "scratch",
            &Options {
                max_age: Duration::ZERO,
                apply: true,
                include_untagged: false,
                ignore_liveness: false,
            },
            &probe,
        )
        .expect("apply");

        assert!(!doomed.exists(), "the dead owner's root must be removed");
        assert!(
            spared.exists(),
            "the live owner's root must survive --apply, even at a zero threshold"
        );
        assert_eq!(applied.removed, 1);
        assert_eq!(applied.freed, 1024);
        assert!(!applied.freed_truncated);
        assert!(applied.failures.is_empty(), "{:?}", applied.failures);
        assert!(applied.report.contains("live-pid"), "{}", applied.report);
        assert!(!applied.report.contains("dry run"), "{}", applied.report);
    }

    /// Issue #461: the old report stated an age fact ("no owned dirs older than
    /// 6h") while the real question was ownership. Every decision must now name
    /// its reason, kept dirs included.
    #[test]
    fn the_report_attributes_every_decision_to_a_reason() {
        let reap = vec![
            candidate(
                "dad-tests-101-AbCdEf",
                2048,
                1,
                Verdict {
                    reap: true,
                    reason: Reason::DeadPid,
                },
            ),
            candidate(
                ".tmpAbCdEf",
                1024,
                9,
                Verdict {
                    reap: true,
                    reason: Reason::UntaggedAge,
                },
            ),
        ];
        let keep = vec![
            candidate(
                "dad-tests-202-AbCdEf",
                4096,
                12,
                Verdict {
                    reap: false,
                    reason: Reason::LivePid,
                },
            ),
            candidate(
                "dot-agent-deck-test-lock-GhIjKl",
                512,
                1,
                Verdict {
                    reap: false,
                    reason: Reason::UntaggedAge,
                },
            ),
        ];

        let text = report("/tmp", &reap, &keep, HOUR * 6);
        for needle in [
            "dead-pid",
            "untagged",
            "live-pid",
            "owning process is gone",
            "owning process is still running",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        assert!(text.contains("reap: 2 dir(s)"), "{text}");
        assert!(text.contains("keep: 2 dir(s)"), "{text}");
        assert!(!text.contains('≥'), "nothing was truncated:\n{text}");
        assert!(
            !text.contains("recycled"),
            "the recycled reason is gone:\n{text}"
        );
    }

    /// Nothing eligible is no longer reported as an age fact: the summary says
    /// which ownership category held each surviving dir back.
    #[test]
    fn an_empty_reap_set_still_says_why_the_survivors_were_kept() {
        let keep = vec![candidate(
            "dad-tests-202-AbCdEf",
            4096,
            12,
            Verdict {
                reap: false,
                reason: Reason::LivePid,
            },
        )];
        let text = report("/tmp", &[], &keep, HOUR * 6);
        assert!(text.contains("nothing to reap in /tmp"), "{text}");
        assert!(text.contains("live-pid"), "{text}");
        assert!(!text.contains("older than 6h"), "{text}");
    }

    /// Survivors are **listed**, not merely counted. A `live-pid` root that is
    /// really a PID reused across a reboot looks identical to a running suite's
    /// root in a bare count, and it is the one class the reaper will not settle
    /// on its own — so its path, size and age have to be on screen for the
    /// operator to judge whether `--ignore-liveness` is warranted.
    #[test]
    fn kept_roots_are_listed_individually_with_size_and_age() {
        let keep = vec![
            candidate(
                "dad-tests-202-Survivor",
                4096,
                12,
                Verdict {
                    reap: false,
                    reason: Reason::LivePid,
                },
            ),
            candidate(
                "dad-tests-303-JustDied",
                512,
                0,
                Verdict {
                    reap: false,
                    reason: Reason::DeadPid,
                },
            ),
        ];
        let text = report("/tmp", &[], &keep, HOUR * 6);

        assert!(
            text.contains("kept:"),
            "no survivor listing header:\n{text}"
        );
        // Each survivor gets its own line, with its own path and size.
        for (name, size) in [
            ("dad-tests-202-Survivor", "4.0 KB"),
            ("dad-tests-303-JustDied", "512 B"),
        ] {
            let line = text
                .lines()
                .find(|l| l.contains(name))
                .unwrap_or_else(|| panic!("{name} was not listed:\n{text}"));
            assert!(line.contains(size), "{name} line lacks its size: {line:?}");
        }
        // The reboot-collision hint and the orphan-window explanation both land.
        assert!(text.contains("--ignore-liveness overrides"), "{text}");
        assert!(text.contains("may still be writing"), "{text}");
        assert!(text.contains("keep: 2 dir(s)"), "{text}");
    }

    /// The survivor listing is capped exactly like the reap listing, so a
    /// machine holding hundreds of live roots cannot bury the summary.
    #[test]
    fn a_long_keep_list_is_truncated_but_fully_counted() {
        let keep: Vec<Candidate> = (0..MAX_LISTED + 3)
            .map(|i| {
                candidate(
                    &format!("dad-tests-{i}-Survivor"),
                    1024,
                    12,
                    Verdict {
                        reap: false,
                        reason: Reason::LivePid,
                    },
                )
            })
            .collect();
        let text = report("/tmp", &[], &keep, HOUR * 6);
        assert_eq!(
            text.lines().filter(|l| l.contains("-Survivor")).count(),
            MAX_LISTED
        );
        assert!(text.contains("… and 3 more"), "{text}");
        assert!(
            text.contains(&format!("keep: {} dir(s)", MAX_LISTED + 3)),
            "the summary must count every survivor:\n{text}"
        );
    }

    /// A leaking machine accumulates hundreds of roots; the per-directory list
    /// is capped so the summary stays visible, and the cap announces itself
    /// rather than silently truncating.
    #[test]
    fn a_long_reap_list_is_truncated_but_fully_counted() {
        let reap: Vec<Candidate> = (0..MAX_LISTED + 5)
            .map(|i| {
                candidate(
                    &format!("dad-tests-{i}-AbCdEf"),
                    1024,
                    1,
                    Verdict {
                        reap: true,
                        reason: Reason::DeadPid,
                    },
                )
            })
            .collect();
        let text = report("/tmp", &reap, &[], HOUR * 6);
        assert_eq!(
            text.lines().filter(|l| l.contains("dad-tests-")).count(),
            MAX_LISTED
        );
        assert!(text.contains("… and 5 more"), "{text}");
        assert!(text.contains("dead-pid"), "{text}");
        assert!(
            text.contains(&format!("{} dir(s)", MAX_LISTED + 5)),
            "the summary must count every candidate, not just the listed ones:\n{text}"
        );
    }

    /// `/tmp` is mode 1777, so the suffix of a `dad-tests-<dead-pid>-*` name is
    /// attacker-controlled text that the default dry run prints straight to a
    /// terminal. It must reach the terminal escaped — an OSC 52 sequence in a
    /// directory name would otherwise rewrite the reader's clipboard.
    #[test]
    fn hostile_path_names_are_escaped_before_printing() {
        let reap = vec![candidate(
            "dad-tests-101-\u{1b}]52;c;aGk=\u{7}\nreap: 999 dir(s)",
            2048,
            1,
            Verdict {
                reap: true,
                reason: Reason::DeadPid,
            },
        )];
        let text = report("/tmp", &reap, &[], HOUR * 6);
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{7}'),
            "control characters reached the terminal:\n{text:?}"
        );
        assert!(text.contains("\\u{1b}]52"), "{text:?}");
        assert!(
            text.lines().filter(|l| l.contains("dad-tests-101")).count() == 1,
            "an embedded newline forged a second line:\n{text:?}"
        );
    }

    /// The size walk now runs on every owned root, kept ones included, so an
    /// enormous or planted tree must not be walked to the end. The size becomes
    /// a lower bound and says so; the verdict is unaffected either way.
    #[test]
    fn an_oversized_tree_stops_walking_and_reports_a_lower_bound() {
        let tmp = tempfile::tempdir().expect("tempdir");
        for i in 0..6 {
            std::fs::write(tmp.path().join(format!("f{i}")), vec![0u8; 100]).expect("write");
        }

        let full = dir_size_bounded(tmp.path(), 100, 8);
        assert_eq!(full.bytes, 600);
        assert!(!full.truncated);

        let capped = dir_size_bounded(tmp.path(), 3, 8);
        assert!(capped.truncated, "the entry budget must stop the walk");
        assert!(capped.bytes < 600, "a truncated walk is a lower bound");

        // Depth is bounded independently of entry count.
        let deep = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&deep).expect("create nested");
        std::fs::write(deep.join("buried"), vec![0u8; 4096]).expect("write");
        let shallow = dir_size_bounded(tmp.path(), 100, 1);
        assert!(shallow.truncated, "the depth budget must stop the descent");
        assert_eq!(shallow.bytes, 600, "nothing below the depth cap is counted");

        // Presentation only: the same tree gets the same verdict either way.
        assert_eq!(
            owner_of("dad-tests-4242-AbCdEf", &FakeProbe::dead()),
            Owner::Dead
        );
    }

    /// Sparse files make apparent size attacker-controlled at zero cost, and
    /// `u64` addition panics on overflow in a debug build — which is how a
    /// plain `cargo xtask clean-e2e-tmp` was aborted with `attempt to add with
    /// overflow` by three planted files, before it could clean anything.
    ///
    /// Every accumulation on the path from one file's length to the printed
    /// total must saturate instead: the per-tree walk, the per-reason group
    /// total, the reap/keep totals, and the `freed` counter after `--apply`.
    ///
    /// Unix-gated only because it ages the roots past the orphan floor with
    /// [`set_mtime`], which opens the directory itself.
    ///
    /// The fixture needs a filesystem whose maximum file size can hold it — see
    /// [`sparse_capable_tempdir`], which is why this does not simply call
    /// `tempfile::tempdir()`.
    #[cfg(unix)]
    #[test]
    fn exabyte_sparse_files_saturate_instead_of_aborting_the_sweep() {
        const HUGE: u64 = 8_000_000_000_000_000_000; // 3 of these overflow u64

        let Some(tmp) = sparse_capable_tempdir(HUGE) else {
            // Loud rather than silent. This is the only test that drives
            // `dir_size_bounded`'s own saturating accumulator with a real
            // overflow, so a vacuous pass would retire that coverage quietly.
            // Observed on macOS, whose runners have no `/dev/shm` and whose
            // temp dir refuses the length; Linux CI reaches it through
            // `/dev/shm` and still covers it. The group, grand and `freed`
            // totals are covered on every platform by
            // `group_and_grand_totals_saturate`, which needs no filesystem.
            println!(
                "SKIP: no filesystem here accepts a {HUGE}-byte sparse file \
                 (tried /dev/shm and the default temp dir), so the size walk's \
                 own saturation is unverified on this host"
            );
            return;
        };
        // The auditor's exact shape: a readable, dead-owner root full of sparse
        // files, reachable by the default dry run.
        for root in ["dad-tests-2147483647-AbCdEf", "dad-tests-2147483646-GhIjKl"] {
            let dir = tmp.path().join(root);
            std::fs::create_dir(&dir).expect("create");
            for i in 0..3 {
                sparse_file(&dir.join(format!("sparse{i}")), HUGE);
            }
            // Aged past the orphan floor so both roots actually reach the
            // deletion path — this test is about arithmetic, not the floor.
            // (Written after the files, which bump the directory's mtime.)
            set_mtime(&dir, SystemTime::now() - DEAD_PID_MIN_AGE * 2);
        }

        // One tree on its own already overflows.
        let one = dir_size_bounded(&tmp.path().join("dad-tests-2147483647-AbCdEf"), 100, 8);
        assert_eq!(one.bytes, u64::MAX, "the walk must saturate, not wrap");
        assert!(!one.truncated, "three entries are well inside the budget");

        // And so do the group totals and the freed counter across two trees.
        let applied = sweep(
            &one_root(tmp.path()),
            "scratch",
            &Options {
                max_age: Duration::ZERO,
                apply: true,
                include_untagged: false,
                ignore_liveness: false,
            },
            &FakeProbe::dead(),
        )
        .expect("apply");
        assert_eq!(applied.removed, 2, "{}", applied.report);
        assert_eq!(applied.freed, u64::MAX);
        assert!(applied.failures.is_empty(), "{:?}", applied.failures);
        assert!(applied.report.contains("dead-pid"), "{}", applied.report);
    }

    /// The same overflow reached through the pure reporting path, where the
    /// group and grand totals are summed independently of the walk.
    #[test]
    fn group_and_grand_totals_saturate() {
        let reap = vec![
            candidate(
                "dad-tests-101-AbCdEf",
                u64::MAX,
                1,
                Verdict {
                    reap: true,
                    reason: Reason::DeadPid,
                },
            ),
            candidate(
                "dad-tests-102-GhIjKl",
                u64::MAX,
                1,
                Verdict {
                    reap: true,
                    reason: Reason::DeadPid,
                },
            ),
        ];
        let text = report("/tmp", &reap, &[], HOUR * 6);
        assert!(text.contains("reap: 2 dir(s)"), "{text}");
        assert!(text.contains("dead-pid"), "{text}");
    }

    /// `--older-than` takes hours from the command line and multiplies by 3600.
    /// A value near `u64::MAX` used to panic the command outright in a debug
    /// build rather than parse into an absurdly distant threshold.
    #[test]
    fn an_enormous_older_than_saturates_rather_than_panicking() {
        let args = vec!["--older-than".to_string(), u64::MAX.to_string()];
        let (parsed, _roots) = parse_args(&args).expect("parse").expect("options");
        assert_eq!(parsed.max_age, Duration::from_secs(u64::MAX));
    }

    /// The sizing walk resolves every queued directory by path, so a symlink
    /// swapped into that path after it was observed to be a directory would be
    /// followed by `read_dir`. This drives that state directly — the walk is
    /// handed a path that is a symlink by the time it opens it — and the
    /// re-check must refuse to descend, leaving the size at zero rather than
    /// counting a tree outside the candidate.
    ///
    /// It is a narrowing, not a fix: a swap undone again inside the window
    /// still slips through, which is why the guarantee is documented as bounded
    /// rather than absolute.
    #[cfg(unix)]
    #[test]
    fn a_directory_replaced_by_a_symlink_is_not_descended() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).expect("create outside");
        std::fs::write(outside.join("payload"), vec![0u8; 4096]).expect("write");

        let root = tmp.path().join("root");
        std::fs::create_dir(&root).expect("create root");
        std::fs::write(root.join("own"), vec![0u8; 100]).expect("write");
        assert_eq!(dir_size_bounded(&root, 100, 8).bytes, 100);

        std::fs::remove_dir_all(&root).expect("remove root");
        std::os::unix::fs::symlink(&outside, &root).expect("symlink");
        let size = dir_size_bounded(&root, 100, 8);
        assert_eq!(
            size.bytes, 0,
            "the walk descended a path that had become a symlink"
        );
        assert!(!size.truncated);
        assert!(
            outside.join("payload").exists(),
            "nothing outside is touched"
        );
    }

    /// A truncated size is marked in the per-directory line, in the group
    /// totals, and explained once at the end.
    #[test]
    fn a_truncated_size_is_marked_as_a_lower_bound() {
        let mut reap = vec![candidate(
            "dad-tests-101-AbCdEf",
            5 * 1024 * 1024,
            1,
            Verdict {
                reap: true,
                reason: Reason::DeadPid,
            },
        )];
        reap[0].size_truncated = true;
        let text = report("/tmp", &reap, &[], HOUR * 6);
        assert!(text.contains("≥ 5.0 MB"), "{text}");
        assert!(text.contains("reap: 1 dir(s), ≥ 5.0 MB"), "{text}");
        assert!(text.contains("lower bounds"), "{text}");
    }

    /// This module's own source, scanned by the reinstatement guard below.
    const SRC: &str = include_str!("clean_tmp.rs");

    /// Tokens that only the deleted PID-recycling machinery brings back, matched
    /// **case-insensitively** (hence the lower-case spellings) against the
    /// source with comments and literals removed by [`code_only`].
    ///
    /// `recycled` covers `Owner::Recycled`, `Reason::RecycledAge`, and any
    /// freshly invented `recycled_*` binding. The rest name the `/proc` parsing
    /// surface the two failed implementations were built on.
    const FORBIDDEN: &[&str] = &[
        "recycled",
        ".created(",
        "sysconf",
        "_sc_clk_tck",
        "starttime",
        "process_start_time",
        "boot_time",
    ];

    fn is_ident_char(ch: char) -> bool {
        ch.is_alphanumeric() || ch == '_'
    }

    /// Whether the character before `i` continues an identifier, so a bare `r`
    /// is only read as a raw-string sigil where one can legally start. A `b`
    /// immediately before it is the byte-string prefix, not identifier text.
    fn prev_is_ident(c: &[char], i: usize) -> bool {
        match i.checked_sub(1).map(|p| c[p]) {
            None => false,
            Some('b') => prev_is_ident(c, i - 1),
            Some(p) => is_ident_char(p),
        }
    }

    /// Index just past a raw string starting at `i` (`r"…"`, `r#"…"#`, …), or
    /// `None` if `i` does not start one.
    fn raw_string_end(c: &[char], i: usize) -> Option<usize> {
        let mut j = i + 1;
        let mut hashes = 0usize;
        while c.get(j) == Some(&'#') {
            hashes += 1;
            j += 1;
        }
        if c.get(j) != Some(&'"') {
            return None;
        }
        j += 1;
        while j < c.len() {
            if c[j] == '"' && c[j + 1..].iter().take(hashes).all(|&h| h == '#') {
                return Some((j + 1 + hashes).min(c.len()));
            }
            j += 1;
        }
        Some(c.len())
    }

    /// Index just past a char literal starting at `i`. `None` for a lifetime or
    /// a loop label — this file has `'walk:`, and swallowing everything up to
    /// the next apostrophe would blind the scan to the code in between.
    fn char_literal_end(c: &[char], i: usize) -> Option<usize> {
        if c.get(i + 1) == Some(&'\\') {
            // An escape of any length: `'\n'`, `'\''`, `'\u{1b}'`.
            let mut j = i + 3;
            while j < c.len() && c[j] != '\'' {
                j += 1;
            }
            return (j < c.len()).then_some(j + 1);
        }
        (c.get(i + 2) == Some(&'\'')).then_some(i + 3)
    }

    /// The source with comments, string literals and char literals blanked out,
    /// so a token scan sees code and only code.
    ///
    /// Both halves are load-bearing. **Comments** have to go because this module
    /// documents the deleted branch at length — the words the scan forbids are
    /// all over its own explanation of why the code is gone, and a naive
    /// substring scan would fail on that explanation, which would be its own
    /// kind of dishonesty. **Literals** have to go because [`FORBIDDEN`] is
    /// itself a list of string literals living in the file being scanned.
    ///
    /// This is not a duplicate of the crate's `strip_rust_comments`, which does
    /// only the first half by design: its callers index the stripped text back
    /// against raw line numbers, so it preserves literals verbatim and walks
    /// bytes rather than chars (which mangles this file's `≥` and `…` into
    /// mojibake — harmless there, wrong here). This walks chars and removes
    /// literals; neither contract can serve the other.
    fn code_only(src: &str) -> String {
        let c: Vec<char> = src.chars().collect();
        let mut out = String::with_capacity(src.len());
        let mut i = 0;
        while i < c.len() {
            let ch = c[i];
            if ch == '/' && c.get(i + 1) == Some(&'/') {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            } else if ch == '/' && c.get(i + 1) == Some(&'*') {
                // Rust block comments nest, so depth is tracked rather than
                // stopping at the first `*/`.
                let mut depth = 1usize;
                i += 2;
                while i < c.len() && depth > 0 {
                    if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                        depth += 1;
                        i += 2;
                    } else if c[i] == '*' && c.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                out.push(' ');
            } else if ch == 'r' && !prev_is_ident(&c, i) {
                match raw_string_end(&c, i) {
                    Some(end) => {
                        i = end;
                        out.push(' ');
                    }
                    // A plain identifier that happens to start with `r`.
                    None => {
                        out.push(ch);
                        i += 1;
                    }
                }
            } else if ch == '"' {
                i += 1;
                while i < c.len() && c[i] != '"' {
                    i += if c[i] == '\\' { 2 } else { 1 };
                }
                i += 1;
                out.push(' ');
            } else if ch == '\'' {
                match char_literal_end(&c, i) {
                    Some(end) => {
                        i = end;
                        out.push(' ');
                    }
                    None => {
                        out.push(ch);
                        i += 1;
                    }
                }
            } else {
                out.push(ch);
                i += 1;
            }
        }
        out
    }

    /// The scanner sees code and nothing else: comments of all three kinds go,
    /// literals go, and — the case this file actually contains — a loop label is
    /// not mistaken for a char literal, which would have swallowed every line
    /// between two apostrophes.
    #[test]
    fn the_source_scan_strips_comments_and_literals_but_not_labels() {
        let sample = concat!(
            "// line boot_time\n",
            "/// doc recycled\n",
            "//! inner sysconf\n",
            "/* block /* nested starttime */ still_comment */\n",
            "let s = \"string sysconf // not a comment\";\n",
            "let r = r#\"raw _SC_CLK_TCK\"#;\n",
            "let dash = '-'; let esc = '\\u{1b}'; let quote = '\\'';\n",
            "'walk: loop { keep_me(); break 'walk; }\n",
        );
        let code = code_only(sample);
        for gone in [
            "boot_time",
            "recycled",
            "sysconf",
            "starttime",
            "still_comment",
            "_SC_CLK_TCK",
            "not a comment",
        ] {
            assert!(!code.contains(gone), "{gone:?} survived:\n{code}");
        }
        assert!(code.contains("let s ="), "code was eaten:\n{code}");
        assert!(
            code.contains("keep_me()"),
            "the loop label was read as a char literal, swallowing the code \
             between the two apostrophes:\n{code}"
        );
        assert!(code.contains("'walk: loop"), "{code}");
    }

    /// **The** guard against the recycled-PID branch coming back (issue #461),
    /// and the only mechanism that can be one.
    ///
    /// No behavioural test can do this job. Tripping a reinstated comparison
    /// needs a root whose timestamp predates its owner's start by more than the
    /// margin, and a directory's birth time cannot be backdated by any portable
    /// API — a test reaches mtime and nothing else, so the gap it can build is
    /// milliseconds, far inside the five-minute margin the deleted code used and
    /// inside any plausible replacement. Restore that code verbatim and every
    /// other test in this file still passes. So the protection is this scan, the
    /// signature check below, and code review.
    #[test]
    fn source_has_no_pid_recycling_machinery() {
        let code = code_only(SRC).to_lowercase();
        for token in FORBIDDEN {
            assert!(
                !code.contains(token),
                "`{token}` is back in clean_tmp.rs.\n\
                 The recycled-PID branch was deleted deliberately (issue #461), not lost: \
                 ordering a process start against a directory timestamp has to bridge the \
                 wall clock, and a forward clock step biases a LIVE root towards looking \
                 recycled — the deletion-unsafe direction. Two implementations were built \
                 and both deleted live e2e roots.\n\
                 Read the `Why there is no recycled-PID branch` section at the top of this \
                 file and docs/develop/e2e-temp-dirs.md before touching this test. Do not \
                 reinstate the branch in any form, including a report-only annotation."
            );
        }

        // Proof the stripping is load-bearing rather than decorative: the module
        // explains the deleted branch at length, so the RAW source is full of
        // the very tokens the scan forbids. A naive substring scan would fail on
        // the documentation of why the code is gone — which is why the tokens
        // are not simply dropped from the list instead.
        let raw = SRC.to_lowercase();
        assert!(
            raw.matches("recycled").count() >= 5 && raw.contains(".created("),
            "the module no longer explains why the branch is gone, so this test \
             no longer demonstrates that it strips comments before scanning"
        );
    }

    /// The cheap complement to the source scan, and deliberately **not**
    /// redundant with it: this one is enforced by the compiler rather than at
    /// runtime. `owner_of` takes a name and a probe and no timestamp, which is
    /// the shape that makes a start-time comparison impossible to express;
    /// reinstating one means widening the signature, and widening the signature
    /// stops this line from compiling. Do not tidy it away as a test that
    /// asserts nothing — the assertion is the type.
    #[test]
    fn owner_of_takes_a_name_and_a_probe_and_no_timestamp() {
        let shape: fn(&str, &dyn ProcessProbe) -> Owner = owner_of;
        assert_eq!(
            shape("dad-tests-4242-AbCdEf", &FakeProbe::live()),
            Owner::Live
        );
    }

    // ---- issue #322: which roots are scanned, and proving each one safe ----

    /// The harness puts its roots in a private `/var/tmp/dad-e2e-<uid>` parent,
    /// so a reaper that only looked at the system temp dir would report
    /// "nothing to reap" while GBs sat under `/var/tmp`. The system temp dir
    /// stays in the set because that is where every pre-#322 leftover is.
    #[test]
    fn standard_roots_cover_the_private_parent_and_the_historical_temp_dir() {
        let roots = standard_roots();
        let paths: Vec<&Path> = roots.iter().map(|r| r.path.as_path()).collect();
        #[cfg(unix)]
        assert!(
            paths.contains(&private_parent().as_path()),
            "private parent missing from {paths:?}",
        );
        assert!(
            paths.contains(&std::env::temp_dir().as_path()),
            "system temp dir missing from {paths:?}",
        );
    }

    /// A scratch directory that can stand in for `/var/tmp`.
    ///
    /// Hardened to 0o700 because [`vet_root`] judges the *holder* of a private
    /// root as well as the root itself, and `tempfile` creates at the umask
    /// default — 0o775 under the common `umask 002`, which is precisely the
    /// "another local user could swap the directory below it" shape the rule
    /// refuses. Without this the fixtures below would be rejected for the wrong
    /// reason and prove nothing about the shapes they plant.
    fn scratch_anchor() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700))
                .expect("harden the anchor");
        }
        tmp
    }

    /// Build macOS's own shape in miniature: a real `private/var/tmp`, reached
    /// through a `var` symlink, so one directory has two spellings that share no
    /// lexical prefix past `anchor`. Returns `(through_the_link, real)`.
    ///
    /// This is the platform's actual layout, not a contrivance — on macOS `/var`
    /// is a symlink to `private/var`, so `/var/tmp` and `/private/var/tmp` are
    /// one directory. Every macOS-shaped test below is built on it. `None` off
    /// Unix, where there is nothing to build it from.
    fn macos_var_alias(anchor: &Path) -> Option<(PathBuf, PathBuf)> {
        #[cfg(unix)]
        {
            let real = anchor.join("private/var/tmp");
            std::fs::create_dir_all(&real).expect("stand-in /private/var/tmp");
            std::os::unix::fs::symlink("private/var", anchor.join("var")).expect("plant /var");
            Some((anchor.join("var/tmp"), real))
        }
        #[cfg(not(unix))]
        {
            let _ = anchor;
            None
        }
    }

    /// The reading the whole platform rests on, verified rather than assumed:
    /// `symlink_metadata` does not follow only the **final** component, so
    /// `symlink_metadata("/var/tmp")` traverses macOS's `/var -> private/var`
    /// link and stats the real `/private/var/tmp` — `drwxrwxrwt root:wheel`.
    ///
    /// If that reading were wrong the shared-holder check would see a symlink,
    /// and `cargo xtask clean-e2e-tmp` would print `REFUSED to scan` on every
    /// Mac and reap nothing. Fail-safe, and useless. The reaper's own tests do
    /// not run on macOS CI at all (issue #470 — `cargo nextest run` there
    /// carries no `--workspace`), so the shape is pinned here on Linux.
    #[cfg(unix)]
    #[test]
    fn the_shared_holder_is_judged_through_a_symlinked_ancestor_as_macos_needs() {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;
        let tmp = scratch_anchor();
        // SAFETY: `geteuid` takes no arguments and touches no memory we own.
        let euid = unsafe { libc::geteuid() };
        let (through_link, real) = macos_var_alias(tmp.path()).expect("unix");
        // 1777 is the mode macOS ships: world-writable, and sticky, which is
        // exactly what makes it acceptable.
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o1777)).expect("chmod");

        let meta = std::fs::symlink_metadata(&through_link).expect("lstat through the link");
        assert!(
            !meta.file_type().is_symlink(),
            "lstat must traverse the ancestor link and stat the real directory",
        );
        assert!(
            meta.is_dir(),
            "{} is not a directory",
            through_link.display()
        );
        let mode = meta.permissions().mode() & 0o7777;
        assert_eq!(mode, 0o1777, "{} is 0o{mode:o}", through_link.display());
        assert_eq!(
            meta.ino(),
            std::fs::symlink_metadata(&real)
                .expect("lstat the real directory")
                .ino(),
            "the two spellings must be one inode",
        );

        // So the verdict is handed a real sticky directory, and accepts it.
        assert_eq!(
            shared_parent_verdict(&through_link, false, true, meta.uid(), mode, euid),
            None,
        );
        // And with the ownership macOS actually has, which no unprivileged test
        // can build: root:wheel, at a typical macOS UID.
        assert_eq!(
            shared_parent_verdict(Path::new("/private/var/tmp"), false, true, 0, 0o1777, 501),
            None,
        );
    }

    /// The whole macOS path end to end: a private parent below a symlinked
    /// ancestor is accepted, scanned at the **resolved** spelling, and its
    /// children are still matched by name.
    ///
    /// On macOS the two halves disagree about spelling by construction — the
    /// harness holds `/var/tmp/dad-e2e-<uid>`, `vet_root` resolves to
    /// `/private/var/tmp/dad-e2e-<uid>`. What has to hold is that every decision
    /// is made by directory *identity* and by the **final** component's name,
    /// neither of which resolution moves.
    #[cfg(unix)]
    #[test]
    fn a_private_parent_below_a_symlinked_ancestor_is_scanned_at_its_resolved_path() {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;
        let tmp = scratch_anchor();
        // SAFETY: `geteuid` takes no arguments and touches no memory we own.
        let euid = unsafe { libc::geteuid() };
        let (through_link, real) = macos_var_alias(tmp.path()).expect("unix");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o1777)).expect("chmod");

        let parent = through_link.join(format!("dad-e2e-{euid}"));
        std::fs::create_dir(&parent).expect("create the private parent");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let child = parent.join("dad-tests-1-macos");
        std::fs::create_dir(&child).expect("plant a harness root");
        std::fs::write(child.join("payload"), vec![0u8; 1024]).expect("write");

        let root = ScanRoot {
            path: parent.clone(),
            untagged_ok: false,
            required: false,
            private: true,
        };
        let scan = match vet_root(&root) {
            RootVerdict::Scan(scan) => scan,
            RootVerdict::Absent => panic!("{} was reported absent", parent.display()),
            RootVerdict::Refused(why) => panic!("macOS's own shape was refused: {why}"),
        };
        assert_eq!(scan, parent.canonicalize().expect("canonicalize"));
        assert_ne!(
            scan, parent,
            "the fixture must really diverge, as macOS does",
        );
        let named = std::fs::metadata(&parent).expect("stat by name");
        let resolved = std::fs::metadata(&scan).expect("stat resolved");
        assert_eq!(
            (named.dev(), named.ino()),
            (resolved.dev(), resolved.ino()),
            "the two spellings must be one directory",
        );

        // Resolution rewrites ancestors, never the final component, so the
        // owned-prefix match is untouched by it.
        let found = collect(&scan, &opts(Duration::ZERO), &FakeProbe::dead()).expect("collect");
        assert_eq!(
            found.len(),
            1,
            "found {:?}",
            found.iter().map(|c| c.path.clone()).collect::<Vec<_>>(),
        );
        assert_eq!(found[0].path, scan.join("dad-tests-1-macos"));
        assert_eq!(found[0].bytes, 1024);
    }

    /// De-duplication at the macOS spelling divergence. `/var/tmp/dad-e2e-501`
    /// and `/private/var/tmp/dad-e2e-501` share no lexical prefix past the
    /// anchor, so a textual comparison would walk one directory twice — doubling
    /// the dry-run totals and, under `--apply`, failing the second
    /// `remove_dir_all` with `NotFound`.
    #[cfg(unix)]
    #[test]
    fn two_spellings_across_a_symlinked_ancestor_dedup_to_one_root() {
        let tmp = scratch_anchor();
        let (through_link, real) = macos_var_alias(tmp.path()).expect("unix");
        let root = |path: PathBuf| ScanRoot {
            path,
            untagged_ok: false,
            required: true,
            private: false,
        };
        let deduped = dedup_roots(vec![root(through_link.clone()), root(real)]);
        assert_eq!(
            deduped.len(),
            1,
            "left {:?}",
            deduped.iter().map(|r| r.path.clone()).collect::<Vec<_>>(),
        );
        assert_eq!(
            deduped[0].path, through_link,
            "the first spelling must survive",
        );
    }

    /// The blocker this closes, at the level that decides it: the *harness*
    /// verifies the private parent before writing under it, but this command —
    /// the one that deletes — verified nothing at all, so a `/var/tmp/dad-e2e-
    /// <victim-uid>` another local user created first would have been scanned
    /// and emptied under `--apply`.
    ///
    /// The foreign case is driven through the pure verdict because `chown` is
    /// privileged; every other shape is built on disk below.
    #[cfg(unix)]
    #[test]
    fn a_foreign_or_loose_private_parent_is_never_scanned() {
        let path = Path::new("/var/tmp/dad-e2e-1000");
        let why = |v: Option<String>| v.unwrap_or_default();

        // Ours, a real directory, owner-only: the one shape that passes.
        assert_eq!(
            private_root_verdict(path, false, true, 1000, 0o700, 1000),
            None,
        );

        // Someone else got the predictable name first.
        let foreign = why(private_root_verdict(path, false, true, 1001, 0o700, 1000));
        assert!(foreign.contains("owned by uid 1001"), "{foreign}");
        assert!(foreign.contains("not this user's"), "{foreign}");

        // Ours, but open enough that anything under it could be anyone's.
        for mode in [0o777, 0o770, 0o707, 0o750] {
            let loose = why(private_root_verdict(path, false, true, 1000, mode, 1000));
            assert!(
                loose.contains("not owner-only"),
                "0o{mode:o} must be refused: {loose}",
            );
        }

        // Redirection: refused, and the message has to say it was not followed.
        let linked = why(private_root_verdict(path, true, true, 1000, 0o777, 1000));
        assert!(linked.contains("is a symlink"), "{linked}");
        assert!(linked.contains("rather than followed"), "{linked}");

        // A FIFO, socket or plain file at the name.
        let fifo = why(private_root_verdict(path, false, false, 1000, 0o700, 1000));
        assert!(fifo.contains("is not a directory"), "{fifo}");
    }

    /// The same rule against real entries on disk, since every shape except the
    /// foreign owner is buildable unprivileged. `vet_root` must come back
    /// `Refused` for each — never `Scan`, which is what would let `--apply`
    /// reach `remove_dir_all`.
    #[cfg(unix)]
    #[test]
    fn vetting_refuses_every_unsafe_private_parent_shape_on_disk() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = scratch_anchor();
        // SAFETY: `geteuid` takes no arguments and touches no memory we own.
        let euid = unsafe { libc::geteuid() };
        let private = |path: PathBuf| ScanRoot {
            path,
            untagged_ok: false,
            required: false,
            private: true,
        };
        let refusal = |root: &ScanRoot| match vet_root(root) {
            RootVerdict::Refused(why) => why,
            RootVerdict::Scan(p) => {
                panic!("{} was accepted as {}", root.path.display(), p.display())
            }
            RootVerdict::Absent => panic!("{} was reported absent", root.path.display()),
        };

        // A loose 0o777 parent — the shape a foreign one would also have, and
        // the one that makes "ours by construction" false.
        let loose = tmp.path().join("loose");
        std::fs::create_dir(&loose).expect("create loose");
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o777)).expect("chmod");
        std::fs::create_dir(loose.join("dad-tests-1-x")).expect("plant a child");
        let why = refusal(&private(loose.clone()));
        assert!(why.contains("not owner-only"), "{why}");
        assert!(
            loose.join("dad-tests-1-x").is_dir(),
            "a refused root must be left completely alone",
        );

        // A symlink at the parent name, pointed at a directory that DOES hold a
        // matching child — the redirection the finding is about.
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).expect("create elsewhere");
        std::fs::create_dir(elsewhere.join("dad-tests-2-y")).expect("plant a child");
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&elsewhere, &link).expect("symlink");
        let why = refusal(&private(link.clone()));
        assert!(why.contains("is a symlink"), "{why}");
        assert!(
            elsewhere.join("dad-tests-2-y").is_dir(),
            "the reaper followed the link",
        );

        // A dangling symlink is still a symlink, not an absence.
        let dangling = tmp.path().join("dangling");
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), &dangling).expect("symlink");
        let why = refusal(&private(dangling));
        assert!(why.contains("is a symlink"), "{why}");

        // A FIFO at the parent name: `read_dir` on one is not something to try.
        let fifo = tmp.path().join("fifo");
        let c_fifo = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).expect("cstring");
        // SAFETY: a NUL-terminated path the call only reads.
        assert_eq!(unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) }, 0, "mkfifo");
        let why = refusal(&private(fifo));
        assert!(why.contains("is not a directory"), "{why}");

        // And the control: a real, owner-only directory of ours is accepted, so
        // the assertions above are refusals of the shape, not of the fixture.
        let good = tmp.path().join("good");
        std::fs::create_dir(&good).expect("create good");
        std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        match vet_root(&private(good.clone())) {
            RootVerdict::Scan(scan) => assert_eq!(
                scan,
                good.canonicalize().expect("canonicalize"),
                "the scanned path must be the resolved one",
            ),
            RootVerdict::Absent => panic!("a real directory reported absent"),
            RootVerdict::Refused(why) => {
                panic!("an owner-only dir of uid {euid} was refused: {why}")
            }
        }
    }

    /// A parent that is simply not there is normal — a machine that has never
    /// run the suite has no private parent yet — and must stay silent rather
    /// than becoming a refusal.
    #[test]
    fn an_absent_standard_root_is_not_a_refusal() {
        let tmp = scratch_anchor();
        let root = ScanRoot {
            path: tmp.path().join("never-created"),
            untagged_ok: false,
            required: false,
            private: true,
        };
        assert!(
            matches!(vet_root(&root), RootVerdict::Absent),
            "an absent root must be Absent, not Refused",
        );
    }

    /// The holder of the private parent has to be trustworthy too: `/var/tmp`
    /// is world-writable, and only the sticky bit stops another user renaming
    /// our parent out from under the scan.
    #[cfg(unix)]
    #[test]
    fn the_shared_holder_of_the_private_parent_must_be_sticky_and_root_owned() {
        let path = Path::new("/var/tmp");
        let why = |v: Option<String>| v.unwrap_or_default();

        // The real-world shape: root-owned, 1777.
        assert_eq!(
            shared_parent_verdict(path, false, true, 0, 0o1777, 1000),
            None
        );
        // Ours is fine too — a scratch stand-in in the tests below is exactly that.
        assert_eq!(
            shared_parent_verdict(path, false, true, 1000, 0o700, 1000),
            None
        );

        let foreign = why(shared_parent_verdict(path, false, true, 1001, 0o1777, 1000));
        assert!(foreign.contains("owned by uid 1001"), "{foreign}");

        // 0777 without the sticky bit: any local user can rename our parent.
        let unsticky = why(shared_parent_verdict(path, false, true, 0, 0o777, 1000));
        assert!(unsticky.contains("without the sticky bit"), "{unsticky}");

        let linked = why(shared_parent_verdict(path, true, true, 0, 0o1777, 1000));
        assert!(linked.contains("is a symlink"), "{linked}");
    }

    /// `/var/tmp` is a Unix path. On Windows it is root-relative on the current
    /// drive and the harness never writes there, so `--apply` must not be able
    /// to delete from it.
    #[cfg(not(unix))]
    #[test]
    fn the_var_tmp_rung_does_not_exist_off_unix() {
        assert!(
            !standard_roots()
                .iter()
                .any(|r| r.path.starts_with("/var/tmp")),
            "/var/tmp must not be scanned on a non-Unix platform",
        );
    }

    /// Issue #322: `--include-untagged` reaps by the `tempfile` crate's default
    /// prefix, so it stays confined to the historical system temp root. Letting
    /// it follow the widened root set would put unrelated tooling's persistent
    /// tempdirs in range.
    #[test]
    fn include_untagged_is_confined_to_the_historical_temp_dir() {
        for root in standard_roots() {
            let is_system_temp = root.path == std::env::temp_dir();
            assert_eq!(
                root.untagged_ok,
                is_system_temp,
                "{} has the wrong --include-untagged scope",
                root.path.display(),
            );
        }
    }

    /// Two spellings of ONE directory must be walked once. Lexical de-duping
    /// misses this: under `--apply` the second `remove_dir_all` returns
    /// `NotFound` and a successful cleanup gets reported as a failure.
    #[cfg(unix)]
    #[test]
    fn aliased_roots_are_deduplicated_by_the_directory_they_resolve_to() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).expect("create real");
        let alias = tmp.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).expect("create symlink");
        let deduped = dedup_roots(vec![
            ScanRoot {
                path: real.clone(),
                untagged_ok: false,
                required: true,
                private: false,
            },
            ScanRoot {
                path: alias,
                untagged_ok: false,
                required: true,
                private: false,
            },
            ScanRoot {
                path: real.join("."),
                untagged_ok: false,
                required: true,
                private: false,
            },
        ]);
        assert_eq!(
            deduped.len(),
            1,
            "left {:?}",
            deduped.iter().map(|r| r.path.clone()).collect::<Vec<_>>(),
        );
        assert_eq!(deduped[0].path, real, "the first spelling must survive");
    }

    /// Placement and deletion are separate decisions: a `DAD_E2E_TMPDIR` that
    /// moved the harness is never scanned automatically, and the user is told
    /// how to opt in rather than left wondering why nothing was found.
    ///
    /// The last case is macOS's: the variable and the root being scanned name
    /// one directory in two spellings. The hint compares by the directory, not
    /// by how it is written, so a Mac is not told to `--root` something already
    /// in the set. This stays one test rather than two because it is the only
    /// one that mutates `DAD_E2E_TMPDIR` — a second would race it outside
    /// nextest's one-process-per-test.
    #[test]
    fn the_env_override_is_not_scanned_but_is_named() {
        let tmp = scratch_anchor();
        let alias = macos_var_alias(tmp.path());
        // SAFETY: nextest runs one test per process, so this process is
        // single-threaded here; the variable is restored before returning.
        let prev = std::env::var_os(TEMP_BASE_ENV);
        unsafe { std::env::set_var(TEMP_BASE_ENV, "/somewhere/else/dad-e2e") };
        let roots = standard_roots();
        let scanned_override = roots.iter().any(|r| r.path.ends_with("dad-e2e"));
        let hint = override_hint(&roots);
        let hint_when_named = override_hint(&[ScanRoot {
            path: PathBuf::from("/somewhere/else/dad-e2e"),
            untagged_ok: true,
            required: true,
            private: false,
        }]);
        let hint_when_aliased = alias.as_ref().map(|(through_link, real)| {
            unsafe { std::env::set_var(TEMP_BASE_ENV, through_link) };
            override_hint(&[ScanRoot {
                path: real.clone(),
                untagged_ok: true,
                required: true,
                private: false,
            }])
        });
        match prev {
            Some(v) => unsafe { std::env::set_var(TEMP_BASE_ENV, v) },
            None => unsafe { std::env::remove_var(TEMP_BASE_ENV) },
        }
        assert!(!scanned_override, "the override must not be scanned");
        let hint = hint.expect("an unscanned override must be named");
        assert!(hint.contains("/somewhere/else/dad-e2e"), "{hint}");
        assert!(hint.contains("--root"), "{hint}");
        assert!(
            hint_when_named.is_none(),
            "no hint once the override is passed as --root: {hint_when_named:?}",
        );
        if let Some(aliased) = hint_when_aliased {
            assert!(
                aliased.is_none(),
                "an override that is another spelling of a scanned root must not \
                 be hinted at: {aliased:?}",
            );
        }
    }

    /// Two absolute paths, spelled the way *this* platform means it — i.e. two
    /// paths that satisfy [`parse_args`]'s `is_absolute()` check on the host.
    ///
    /// `is_absolute()` is platform-specific: a leading `/` is absolute on Unix
    /// but **not** on Windows, which requires a drive prefix. So `/scratch/one`
    /// is root-relative there, `parse_args` rightly refused it, and the test
    /// below panicked in its own setup instead of exercising the replacement
    /// rule — a Windows-only failure (issue #511). Like the sparse-file fixture
    /// above, that only became visible when issue #489 put the workspace's tests
    /// in a gate on all three platforms. Nothing touches the filesystem; these
    /// directories need not exist.
    #[cfg(not(windows))]
    const SCRATCH_ROOTS: [&str; 2] = ["/scratch/one", "/scratch/two"];
    #[cfg(windows)]
    const SCRATCH_ROOTS: [&str; 2] = [r"C:\scratch\one", r"C:\scratch\two"];

    /// `--root` replaces the standard set rather than adding to it, so a
    /// deliberate scan of one directory cannot quietly also delete from
    /// `/var/tmp` or the system temp dir.
    #[test]
    fn explicit_roots_replace_the_standard_set() {
        let (_opts, roots) = parse_args(&[
            "--root".to_string(),
            SCRATCH_ROOTS[0].to_string(),
            "--root".to_string(),
            SCRATCH_ROOTS[1].to_string(),
        ])
        .expect("parse")
        .expect("options");
        assert_eq!(
            roots,
            vec![
                PathBuf::from(SCRATCH_ROOTS[0]),
                PathBuf::from(SCRATCH_ROOTS[1])
            ],
        );
        assert!(
            parse_args(&[])
                .expect("parse")
                .expect("options")
                .1
                .is_empty(),
            "no --root means the standard set",
        );
    }

    /// Naming the private parent with `--root` must not buy weaker treatment
    /// than letting it be discovered as a standard root.
    ///
    /// The two spellings are one directory, so they must get one security
    /// posture. Before this, `--root` set `private: false` — which skipped the
    /// ownership, `mode & 0o077` and sticky-holder checks entirely and let
    /// `canonicalize` **follow** a symlink the private arm refuses outright —
    /// and flipped `untagged_ok` on at the one location `usage()` says
    /// `--include-untagged` must never reach.
    #[cfg(unix)]
    #[test]
    fn naming_the_private_parent_with_root_keeps_its_private_treatment() {
        let private = private_parent();
        let roots = explicit_scan_roots(std::slice::from_ref(&private));
        assert_eq!(roots.len(), 1);
        assert!(
            roots[0].private,
            "{} named by hand must still be judged as the private parent",
            private.display(),
        );
        assert!(
            !roots[0].untagged_ok,
            "--include-untagged must never reach the private parent, however it \
             was spelled",
        );

        // An unrelated hand-named directory is untouched by this: it stays an
        // ordinary explicit root, or the flag would become unusable.
        let other = explicit_scan_roots(&[PathBuf::from("/scratch/elsewhere")]);
        assert!(!other[0].private);
        assert!(other[0].untagged_ok);
    }

    /// A relative `--root` would resolve against whatever directory the command
    /// happened to be run from — refused rather than guessed at.
    #[test]
    fn a_relative_root_is_refused() {
        assert!(parse_args(&["--root".to_string(), "scratch".to_string()]).is_err());
        assert!(parse_args(&["--root".to_string()]).is_err());
    }
    /// A root whose owner is still running is never reaped, so a reap cannot
    /// race a suite that is currently running.
    ///
    /// Issue #461 moved the guarantee: [`collect`] no longer filters young roots
    /// out, because the report has to explain survivors, so what used to be an
    /// empty result is now a collected root carrying a keep verdict.
    #[test]
    fn recent_dirs_are_left_alone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(tmp.path().join("dad-tests-1-fresh")).expect("create");
        let found = collect(tmp.path(), &opts(HOUR), &FakeProbe::live()).expect("collect");
        assert_eq!(found.len(), 1, "the root must still be seen and explained");
        assert!(
            !found[0].verdict.reap,
            "a live owner's root must not be reaped"
        );
        assert_eq!(found[0].verdict.reason, Reason::LivePid);
    }

    #[test]
    fn stale_owned_dirs_are_collected_with_their_size() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stale = tmp.path().join("dad-tests-1-stale");
        std::fs::create_dir(&stale).expect("create");
        std::fs::write(stale.join("payload"), vec![0u8; 2048]).expect("write");
        let found =
            collect(tmp.path(), &opts(Duration::ZERO), &FakeProbe::dead()).expect("collect");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].bytes, 2048);
    }

    /// The residue actually left on disk after a full e2e run is not a harness
    /// root: the exit sweep removed the real 0o700 one, and an agent process
    /// that outlived the test re-created the path with `mkdir -p`, landing it
    /// at the umask default (0o775 under the common `umask 002`). Reaping keys
    /// on the name and never on the mode — a `0o700`-only filter added here
    /// would make exactly the residue worth reclaiming unreclaimable.
    ///
    /// Aged past [`DEAD_PID_MIN_AGE`] so it asserts what its name says: the
    /// orphan floor added in issue #461 keeps a *fresh* dead-owner root.
    #[cfg(unix)]
    #[test]
    fn an_orphan_recreated_root_at_the_umask_default_is_still_reaped() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let skeleton = tmp.path().join("dad-tests-99999-Zz0AbC");
        std::fs::create_dir(&skeleton).expect("create");
        std::fs::write(skeleton.join("payload"), vec![0u8; 4096]).expect("write");
        std::fs::set_permissions(&skeleton, std::fs::Permissions::from_mode(0o775))
            .expect("chmod 0o775");
        let mode = std::fs::symlink_metadata(&skeleton)
            .expect("stat skeleton")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o775, "the fixture must reproduce the observed mode");
        set_mtime(&skeleton, SystemTime::now() - DEAD_PID_MIN_AGE - HOUR);

        let found =
            collect(tmp.path(), &opts(Duration::ZERO), &FakeProbe::dead()).expect("collect");
        assert_eq!(found.len(), 1, "a 0o775 orphan skeleton must still be seen");
        assert_eq!(found[0].path, skeleton);
        assert_eq!(found[0].bytes, 4096);
        assert!(
            found[0].verdict.reap,
            "a long-dead owner's root must be reaped"
        );
        assert_eq!(found[0].verdict.reason, Reason::DeadPid);
    }
}
