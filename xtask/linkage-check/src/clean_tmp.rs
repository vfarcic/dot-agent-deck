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

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

/// Directory-name prefixes this repo owns outright and may reap by default.
const OWNED_PREFIXES: &[&str] = &["dad-tests-", "dad-unit-", "dot-agent-deck-test-lock-"];

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
}

struct Candidate {
    path: PathBuf,
    bytes: u64,
    age: Duration,
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

    let mut candidates = Vec::new();
    for root in &vetted {
        // Per-root view: `collect` reads `include_untagged` straight off
        // `Options`, and the flag is confined to one root, so it is masked here
        // rather than inside `collect` — which issue #461 is rewriting and
        // which this branch leaves byte-for-byte untouched.
        let scoped = Options {
            max_age: opts.max_age,
            apply: opts.apply,
            include_untagged: opts.include_untagged && root.untagged_ok,
        };
        // `root.scan`, not `root.shown`: the resolved path vetting produced, so
        // the tree walked here is the tree vetting judged.
        match collect(&root.scan, &scoped) {
            Ok(mut found) => candidates.append(&mut found),
            // Vetting already stat'ed it, so a `NotFound` here is a race with
            // something else removing the root — nothing left to reap.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                eprintln!(
                    "xtask clean-e2e-tmp: cannot read {}: {e}",
                    root.shown.display()
                );
                return ExitCode::FAILURE;
            }
        }
    }
    // `collect` sorts within one root; re-sort so the biggest offender is first
    // across all of them.
    candidates.sort_by_key(|c| std::cmp::Reverse(c.bytes));

    if candidates.is_empty() {
        println!(
            "nothing to reap in {searched} (no owned dirs older than {})",
            human_duration(opts.max_age),
        );
        return ExitCode::SUCCESS;
    }

    let total: u64 = candidates.iter().map(|c| c.bytes).sum();
    for c in &candidates {
        println!(
            "  {:>9}  {:>6} old  {}",
            human_bytes(c.bytes),
            human_duration(c.age),
            c.path.display(),
        );
    }
    println!(
        "{} dir(s), {} total, older than {} in {searched}",
        candidates.len(),
        human_bytes(total),
        human_duration(opts.max_age),
    );

    if !opts.apply {
        println!();
        println!("dry run — nothing removed. Re-run with --apply to delete.");
        return ExitCode::SUCCESS;
    }

    let mut removed = 0usize;
    let mut freed = 0u64;
    let mut failures = 0usize;
    for c in &candidates {
        match std::fs::remove_dir_all(&c.path) {
            Ok(()) => {
                removed += 1;
                freed += c.bytes;
            }
            Err(e) => {
                failures += 1;
                eprintln!("  failed to remove {}: {e}", c.path.display());
            }
        }
    }
    println!("removed {removed} dir(s), freed {}", human_bytes(freed));
    if failures > 0 {
        eprintln!("{failures} dir(s) could not be removed");
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
    };
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--apply" => opts.apply = true,
            "--include-untagged" => opts.include_untagged = true,
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
                opts.max_age = Duration::from_secs(hours * 3600);
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
         [--include-untagged] [--root <dir>]..."
    );
    println!();
    println!("Reaps stale e2e harness temp dirs left by SIGKILLed test processes.");
    println!("Dry-run by default; --apply is required to delete.");
    println!();
    println!("Scans the standard roots — the harness's private /var/tmp/dad-e2e-<uid>");
    println!("parent and the system temp dir. It cannot infer another worktree's");
    println!("leftovers, so run it where the run that leaked them ran.");
    println!();
    println!("  --older-than <hours>  age threshold (default: {DEFAULT_MAX_AGE_HOURS})");
    println!("  --apply               actually remove the directories");
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

fn collect(temp_root: &Path, opts: &Options) -> std::io::Result<Vec<Candidate>> {
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
        let age = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .unwrap_or_default();
        if age < opts.max_age {
            continue;
        }
        out.push(Candidate {
            bytes: dir_size(&path),
            path,
            age,
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

/// Recursive apparent size. Never follows symlinks, so a link out of the tree
/// contributes its own size and nothing more.
fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
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

fn human_duration(d: Duration) -> String {
    let hours = d.as_secs() / 3600;
    if hours < 48 {
        format!("{hours}h")
    } else {
        format!("{}d", hours / 24)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_prefixes_are_reaped_by_default() {
        assert!(is_owned("dad-tests-1234-AbCdEf", false));
        assert!(is_owned("dot-agent-deck-test-lock-AbCdEf", false));
        // `src/test_temp.rs` dirs. Without this they would be unreclaimable:
        // they live under the private `/var/tmp` parent, which
        // `--include-untagged` deliberately never reaches.
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
        let opts = Options {
            max_age: Duration::ZERO,
            apply: false,
            include_untagged: false,
        };
        let found = collect(tmp.path(), &opts).expect("collect");
        assert!(
            found.is_empty(),
            "symlink was collected: {:?}",
            found.iter().map(|c| c.path.clone()).collect::<Vec<_>>()
        );
        assert!(target.exists(), "target must be untouched");
    }

    /// Dirs younger than the threshold are left alone, so a reap cannot race a
    /// suite that is currently running.
    #[test]
    fn recent_dirs_are_left_alone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(tmp.path().join("dad-tests-1-fresh")).expect("create");
        let opts = Options {
            max_age: Duration::from_secs(3600),
            apply: false,
            include_untagged: false,
        };
        assert!(collect(tmp.path(), &opts).expect("collect").is_empty());
    }

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
        let opts = Options {
            max_age: Duration::ZERO,
            apply: false,
            include_untagged: false,
        };
        let found = collect(&scan, &opts).expect("collect");
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

    /// Two absolute paths that satisfy [`parse_args`]'s `is_absolute()` check on
    /// the host platform. `is_absolute()` is platform-specific — a leading `/`
    /// is absolute on Unix but NOT on Windows, which requires a drive prefix —
    /// so a Unix-only literal here fails the very validation the test is
    /// exercising, on Windows only (issue #511). Nothing touches the
    /// filesystem; these directories need not exist.
    #[cfg(windows)]
    const SCRATCH_ONE: &str = r"C:\scratch\one";
    #[cfg(windows)]
    const SCRATCH_TWO: &str = r"C:\scratch\two";
    #[cfg(not(windows))]
    const SCRATCH_ONE: &str = "/scratch/one";
    #[cfg(not(windows))]
    const SCRATCH_TWO: &str = "/scratch/two";

    /// `--root` replaces the standard set rather than adding to it, so a
    /// deliberate scan of one directory cannot quietly also delete from
    /// `/var/tmp` or the system temp dir.
    #[test]
    fn explicit_roots_replace_the_standard_set() {
        let (_opts, roots) = parse_args(&[
            "--root".to_string(),
            SCRATCH_ONE.to_string(),
            "--root".to_string(),
            SCRATCH_TWO.to_string(),
        ])
        .expect("parse")
        .expect("options");
        assert_eq!(
            roots,
            vec![PathBuf::from(SCRATCH_ONE), PathBuf::from(SCRATCH_TWO)],
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

    #[test]
    fn stale_owned_dirs_are_collected_with_their_size() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stale = tmp.path().join("dad-tests-1-stale");
        std::fs::create_dir(&stale).expect("create");
        std::fs::write(stale.join("payload"), vec![0u8; 2048]).expect("write");
        let opts = Options {
            max_age: Duration::ZERO,
            apply: false,
            include_untagged: false,
        };
        let found = collect(tmp.path(), &opts).expect("collect");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].bytes, 2048);
    }

    /// The residue actually left on disk after a full e2e run is not a harness
    /// root: the exit sweep removed the real 0o700 one, and an agent process
    /// that outlived the test re-created the path with `mkdir -p`, landing it
    /// at the umask default (0o775 under the common `umask 002`). Reaping keys
    /// on the name and never on the mode — a `0o700`-only filter added here
    /// would make exactly the residue worth reclaiming unreclaimable.
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

        let opts = Options {
            max_age: Duration::ZERO,
            apply: false,
            include_untagged: false,
        };
        let found = collect(tmp.path(), &opts).expect("collect");
        assert_eq!(found.len(), 1, "a 0o775 orphan skeleton must still be seen");
        assert_eq!(found[0].path, skeleton);
        assert_eq!(found[0].bytes, 4096);
    }
}
