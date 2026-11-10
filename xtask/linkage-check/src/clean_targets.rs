//! `cargo xtask clean-targets` — reclaim the cargo `target/` directories of
//! idle worktrees (issue #2107).
//!
//! Every dispatch worktree builds its own `target/`, and nothing removed one
//! until the worktree itself was removed. A long-lived worktree's tree grows
//! with every feature set and toolchain it is built under: one reached 319 GB
//! on the shared Linux box, and on a laptop an idle finished unit's `target/`
//! was routinely 15–145 GB. This subcommand deletes such a tree when nothing is
//! using it, so the next build in that worktree starts cold instead.
//!
//! # What it may delete, and nothing else
//!
//! For each worktree `git worktree list --porcelain` names (the main checkout
//! included), the one path `<worktree>/target`, and only when it is a real
//! directory (not a symlink) carrying a file cargo writes at a build
//! directory's root: `CACHEDIR.TAG` with the standard signature, or
//! `.rustc_info.json`. The second is needed because cargo writes the tag only
//! when it creates the directory itself, and a `target/` that something else
//! created first (measured: one whose first entry was `target/nextest`) never
//! gets one; that tree held 56 GB. A worktree's sources, branch and uncommitted
//! work are never in scope. Bare, prunable and locked worktrees are skipped.
//!
//! # When a tree counts as in use
//!
//! A tree is kept, whatever its size, when any of these holds:
//!
//! - a process this user can see has a file under it open, mapped or as its
//!   executable — a running build, a test binary, the deck's daemon started
//!   from `target/debug`, cargo's own `.cargo-lock`. `rust-analyzer` is the
//!   exception: it keeps proc-macro libraries mapped for its whole session, so
//!   counting it would keep every tree an editor has open forever. The `cargo
//!   check` it runs is a build like any other and does count;
//! - a build tool (`cargo`, `rustc`, a linker, …) is running with its working
//!   directory anywhere in the worktree, so a build that has not yet opened
//!   anything under `target/` is still seen;
//! - anything under the tree was modified less than `--min-idle` ago (default
//!   30 minutes), so a build that just finished, or one between two steps, is
//!   left alone;
//! - its open files could not be checked at all (`lsof` missing or failing).
//!
//! The checkout this runs from is therefore never reclaimed: its own binary is
//! running from its `target/`.
//!
//! # The delete is a rename first
//!
//! The process check is repeated immediately before each delete, and the tree
//! is then **renamed** out of the worktree (to a hidden sibling of the
//! worktree, `.dad-clean-targets-…`, on the same filesystem) before it is
//! removed. A build that starts after the rename gets a fresh, empty `target/`
//! instead of a half-deleted one. A sibling and not a name inside the worktree,
//! because `.gitignore` ignores `/target` only, so a renamed tree inside the
//! worktree would show up as untracked files to a `git add -A` while it is
//! being removed. A parked tree left behind by an interrupted run is found and
//! removed by the next run.
//!
//! The window between the recheck and the rename is not closed: a build that
//! starts inside it can lose its tree. It is the time two syscalls take, and
//! the build then fails rather than producing a wrong result.
//!
//! Dry run by default; `--apply` deletes. Operational page:
//! `docs/develop/target-cleanup.md`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, SystemTime};

/// cargo's `CACHEDIR.TAG` signature (<https://bford.info/cachedir/>).
const CACHEDIR_SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55";

/// Prefix of a tree parked for removal, beside the worktree it came from.
const PARK_PREFIX: &str = ".dad-clean-targets-";

const DEFAULT_MIN_IDLE_MINUTES: u64 = 30;

/// Processes that, running with their working directory inside a worktree,
/// mean a build is in progress there even before it opens anything under
/// `target/`. Matched against the command name `lsof` reports.
const BUILD_TOOLS: &[&str] = &[
    "cargo",
    "rustc",
    "rustdoc",
    "clippy-driver",
    "cargo-clippy",
    "cargo-nextest",
    "cargo-mutants",
    "cargo-xtask",
    "rust-lld",
    "ld",
    "ld64",
    "cc",
    "clang",
    "gcc",
    "sccache",
];

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Opts {
    pub apply: bool,
    pub min_idle: Duration,
    pub min_size_bytes: u64,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            apply: false,
            min_idle: Duration::from_secs(DEFAULT_MIN_IDLE_MINUTES * 60),
            min_size_bytes: 0,
        }
    }
}

/// One `lsof` record: a process and one path it holds.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Holder {
    pub pid: u32,
    pub command: String,
    pub path: PathBuf,
    /// The path is the process's working directory, not an open file.
    pub is_cwd: bool,
}

/// What was measured about one tree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Scan {
    pub bytes: u64,
    pub newest: SystemTime,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Verdict {
    Reclaim { bytes: u64, idle: Duration },
    Skip(String),
}

/// The worktree paths in `git worktree list --porcelain` output, without the
/// bare, prunable and locked ones.
pub(crate) fn parse_worktree_porcelain(s: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for block in s.split("\n\n") {
        let mut path = None;
        let mut excluded = false;
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if line == "bare"
                || line == "prunable"
                || line.starts_with("prunable ")
                || line == "locked"
                || line.starts_with("locked ")
            {
                excluded = true;
            }
        }
        if let (Some(p), false) = (path, excluded) {
            out.push(p);
        }
    }
    out
}

/// Parse `lsof -F pcfn` output into one [`Holder`] per named file.
pub(crate) fn parse_lsof(s: &str) -> Vec<Holder> {
    let mut out = Vec::new();
    let (mut pid, mut command, mut fd) = (0u32, String::new(), String::new());
    for line in s.lines() {
        let Some(tag) = line.chars().next() else {
            continue;
        };
        let rest = &line[tag.len_utf8()..];
        match tag {
            'p' => {
                pid = rest.parse().unwrap_or(0);
                command.clear();
                fd.clear();
            }
            'c' => command = rest.to_string(),
            'f' => fd = rest.to_string(),
            'n' if pid != 0 => out.push(Holder {
                pid,
                command: command.clone(),
                path: PathBuf::from(rest),
                is_cwd: fd == "cwd",
            }),
            _ => {}
        }
    }
    out
}

fn is_build_tool(command: &str) -> bool {
    let base = command.rsplit('/').next().unwrap_or(command);
    BUILD_TOOLS.contains(&base)
}

/// Why `target` (inside `worktree`) is in use, or `None` when no holder in the
/// snapshot uses it. Both paths must already be canonical, as `lsof` reports
/// canonical paths.
pub(crate) fn blocker(worktree: &Path, target: &Path, holders: &[Holder]) -> Option<String> {
    for h in holders {
        if h.path.starts_with(target) && !h.command.starts_with("rust-analyzer") {
            return Some(format!(
                "in use: {} (pid {}) holds {}",
                h.command,
                h.pid,
                h.path.display()
            ));
        }
        if h.is_cwd && h.path.starts_with(worktree) && is_build_tool(&h.command) {
            return Some(format!(
                "build running: {} (pid {}) in {}",
                h.command,
                h.pid,
                h.path.display()
            ));
        }
    }
    None
}

/// `dir` is a real directory carrying cargo's `CACHEDIR.TAG` or its
/// `.rustc_info.json` (see the module docs for why either).
pub(crate) fn is_cargo_target(dir: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    if !meta.is_dir() {
        return false;
    }
    let tagged = std::fs::read_to_string(dir.join("CACHEDIR.TAG"))
        .map(|s| s.starts_with(CACHEDIR_SIGNATURE))
        .unwrap_or(false);
    tagged
        || std::fs::symlink_metadata(dir.join(".rustc_info.json"))
            .map(|m| m.is_file())
            .unwrap_or(false)
}

/// Disk usage and newest modification time of everything under `dir`, the
/// directory itself included. Symlinks are measured, never followed. An entry
/// that disappears during the walk is ignored.
pub(crate) fn scan(dir: &Path) -> std::io::Result<Scan> {
    let meta = std::fs::symlink_metadata(dir)?;
    let mut acc = Scan {
        bytes: disk_bytes(&meta),
        newest: meta.modified()?,
    };
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let meta = match std::fs::symlink_metadata(entry.path()) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            acc.bytes += disk_bytes(&meta);
            if let Ok(m) = meta.modified()
                && m > acc.newest
            {
                acc.newest = m;
            }
            if meta.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok(acc)
}

#[cfg(unix)]
fn disk_bytes(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.blocks() * 512
}

#[cfg(not(unix))]
fn disk_bytes(meta: &std::fs::Metadata) -> u64 {
    meta.len()
}

/// The idle and size policy, on an unblocked tree.
pub(crate) fn decide(scan: &Scan, now: SystemTime, opts: &Opts) -> Verdict {
    // A modification time in the future reads as "just written": keep it.
    let idle = now.duration_since(scan.newest).unwrap_or(Duration::ZERO);
    if idle < opts.min_idle {
        return Verdict::Skip(format!(
            "written {} ago (--min-idle {})",
            human_duration(idle),
            human_duration(opts.min_idle)
        ));
    }
    if scan.bytes < opts.min_size_bytes {
        return Verdict::Skip(format!(
            "{} is under --min-size {}",
            human_bytes(scan.bytes),
            human_bytes(opts.min_size_bytes)
        ));
    }
    Verdict::Reclaim {
        bytes: scan.bytes,
        idle,
    }
}

/// Everything decided about one worktree's `target/`. `holders` is `None` when
/// the open-file snapshot could not be taken. Returns `None` when the worktree
/// has no `target/` at all.
pub(crate) fn assess(
    worktree: &Path,
    holders: Option<&[Holder]>,
    now: SystemTime,
    opts: &Opts,
) -> Option<(PathBuf, Verdict)> {
    let worktree = worktree
        .canonicalize()
        .unwrap_or_else(|_| worktree.to_path_buf());
    let target = worktree.join("target");
    if std::fs::symlink_metadata(&target).is_err() {
        return None;
    }
    if !is_cargo_target(&target) {
        return Some((
            target,
            Verdict::Skip(
                "not a cargo build directory (no CACHEDIR.TAG or .rustc_info.json, or a symlink)"
                    .into(),
            ),
        ));
    }
    let Some(holders) = holders else {
        return Some((
            target,
            Verdict::Skip("open files could not be checked".into()),
        ));
    };
    if let Some(why) = blocker(&worktree, &target, holders) {
        return Some((target, Verdict::Skip(why)));
    }
    let verdict = match scan(&target) {
        Ok(s) => decide(&s, now, opts),
        Err(e) => Verdict::Skip(format!("could not be measured: {e}")),
    };
    Some((target, verdict))
}

/// The parking path for `target` of `worktree`: a hidden sibling of the
/// worktree, so it is on the same filesystem and outside the work tree.
pub(crate) fn park_path(worktree: &Path, tag: &str) -> Option<PathBuf> {
    let name = worktree.file_name()?.to_string_lossy().into_owned();
    let mut file: OsString = PARK_PREFIX.into();
    file.push(format!("{name}-{tag}"));
    Some(worktree.parent()?.join(file))
}

/// Trees parked by an earlier run that did not finish removing them: hidden
/// siblings of the worktrees that carry the prefix and cargo's tag.
pub(crate) fn parked_leftovers(worktrees: &[PathBuf]) -> Vec<PathBuf> {
    let mut parents: Vec<PathBuf> = worktrees
        .iter()
        .filter_map(|w| w.parent().map(Path::to_path_buf))
        .collect();
    parents.sort();
    parents.dedup();
    let mut out = Vec::new();
    for p in parents {
        let Ok(entries) = std::fs::read_dir(&p) else {
            continue;
        };
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with(PARK_PREFIX)
                && is_cargo_target(&e.path())
            {
                out.push(e.path());
            }
        }
    }
    out.sort();
    out
}

/// Rename `target` to `parked`, then remove it.
pub(crate) fn reclaim(target: &Path, parked: &Path) -> std::io::Result<()> {
    std::fs::rename(target, parked)?;
    std::fs::remove_dir_all(parked)
}

fn human_bytes(b: u64) -> String {
    const GIB: f64 = (1u64 << 30) as f64;
    const MIB: f64 = (1u64 << 20) as f64;
    if b as f64 >= GIB {
        format!("{:.1} GiB", b as f64 / GIB)
    } else {
        format!("{:.0} MiB", b as f64 / MIB)
    }
}

fn human_duration(d: Duration) -> String {
    let m = d.as_secs() / 60;
    if m >= 120 {
        format!("{}h", m / 60)
    } else {
        format!("{m}m")
    }
}

fn usage() {
    println!(
        "usage: cargo xtask clean-targets [--apply] [--min-idle <minutes>] [--min-size <GiB>]"
    );
    println!();
    println!("Reclaims the cargo target/ directory of every worktree of this repository");
    println!("(the main checkout included) that nothing is using. Dry run by default;");
    println!("--apply is required to delete.");
    println!();
    println!("Only <worktree>/target is ever deleted, and only when it is a real directory");
    println!("carrying cargo's CACHEDIR.TAG or .rustc_info.json. A tree is kept when a");
    println!("process has a file under it open (rust-analyzer excepted), when a build tool");
    println!("runs inside the worktree, when anything under it changed within --min-idle,");
    println!("or when open files cannot be checked (it needs lsof). The checkout you run");
    println!("this from is always kept, since its own binary runs from its target/.");
    println!();
    println!(
        "  --min-idle <minutes>  keep trees modified more recently (default: {DEFAULT_MIN_IDLE_MINUTES})"
    );
    println!("  --min-size <GiB>      keep trees smaller than this (default: 0, any size)");
    println!("  --apply               actually delete");
    println!();
    println!("docs/develop/target-cleanup.md has when the skills run it, and with what.");
}

fn parse_args(args: &[String]) -> Result<Option<Opts>, String> {
    let mut opts = Opts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--apply" => opts.apply = true,
            "--min-idle" => {
                let v = it.next().ok_or("--min-idle needs a number of minutes")?;
                let m: u64 = v
                    .parse()
                    .map_err(|_| format!("--min-idle: not a whole number of minutes: {v}"))?;
                opts.min_idle = Duration::from_secs(m * 60);
            }
            "--min-size" => {
                let v = it.next().ok_or("--min-size needs a number of GiB")?;
                let g: u64 = v
                    .parse()
                    .map_err(|_| format!("--min-size: not a whole number of GiB: {v}"))?;
                opts.min_size_bytes = g << 30;
            }
            "-h" | "--help" => {
                usage();
                return Ok(None);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Some(opts))
}

/// `git worktree list --porcelain` for `root`, with the variables that would
/// point git at another repository cleared (the hazard linkage-check rule 13
/// guards in `src/`).
fn list_worktrees(root: &Path) -> Result<Vec<PathBuf>, String> {
    let out = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(parse_worktree_porcelain(&String::from_utf8_lossy(
        &out.stdout,
    )))
}

/// One snapshot of every file this user's processes hold, or `None` when it
/// cannot be taken. `+c 0` asks for the full command name, which
/// `rust-analyzer-proc-macro-srv` needs.
fn holders_snapshot() -> Option<Vec<Holder>> {
    let out = Command::new("lsof")
        .args(["-n", "-P", "-w", "+c", "0", "-F", "pcfn"])
        .output()
        .ok()?;
    let holders = parse_lsof(&String::from_utf8_lossy(&out.stdout));
    // lsof exits 1 for warnings it could not suppress, so judge by the output.
    if holders.is_empty() {
        None
    } else {
        Some(holders)
    }
}

pub fn run(root: &Path, args: &[String]) -> ExitCode {
    let opts = match parse_args(args) {
        Ok(Some(o)) => o,
        Ok(None) => return ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("xtask clean-targets: {msg}");
            usage();
            return ExitCode::from(2);
        }
    };
    let worktrees = match list_worktrees(root) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("xtask clean-targets: {e}");
            return ExitCode::FAILURE;
        }
    };
    let snapshot = holders_snapshot();
    if snapshot.is_none() {
        eprintln!("xtask clean-targets: could not list open files with `lsof`; every tree is kept");
    }
    let now = SystemTime::now();

    let mut planned: Vec<(PathBuf, PathBuf, u64)> = Vec::new(); // worktree, target, bytes
    for wt in &worktrees {
        let Some((target, verdict)) = assess(wt, snapshot.as_deref(), now, &opts) else {
            continue;
        };
        match verdict {
            Verdict::Reclaim { bytes, idle } => {
                println!(
                    "reclaim {} ({}, idle {})",
                    target.display(),
                    human_bytes(bytes),
                    human_duration(idle)
                );
                let wt = target.parent().map(Path::to_path_buf).unwrap_or_default();
                planned.push((wt, target, bytes));
            }
            Verdict::Skip(why) => println!("keep    {}: {why}", target.display()),
        }
    }
    let leftovers = parked_leftovers(&worktrees);
    for l in &leftovers {
        println!("remove  {} (left parked by an earlier run)", l.display());
    }

    let total: u64 = planned.iter().map(|p| p.2).sum();
    if !opts.apply {
        println!(
            "dry run: {} target dir(s), {} reclaimable, {} parked leftover(s). Pass --apply to delete.",
            planned.len(),
            human_bytes(total),
            leftovers.len()
        );
        return ExitCode::SUCCESS;
    }

    let mut failed = false;
    let mut freed = 0u64;
    let tag = format!(
        "{}-{}",
        std::process::id(),
        now.duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    for (wt, target, bytes) in planned {
        // The snapshot above can be minutes old after the walks; take a new one
        // for each delete, and keep the tree if it cannot be taken.
        let Some(fresh) = holders_snapshot() else {
            println!(
                "kept    {}: open files could not be rechecked",
                target.display()
            );
            failed = true;
            continue;
        };
        if let Some(why) = blocker(&wt, &target, &fresh) {
            println!("kept    {}: {why}", target.display());
            continue;
        }
        let Some(parked) = park_path(&wt, &tag) else {
            println!(
                "kept    {}: no parent directory to park it in",
                target.display()
            );
            continue;
        };
        match reclaim(&target, &parked) {
            Ok(()) => {
                freed += bytes;
                println!("deleted {} ({})", target.display(), human_bytes(bytes));
            }
            Err(e) => {
                eprintln!("failed  {}: {e}", target.display());
                failed = true;
            }
        }
    }
    if !leftovers.is_empty() {
        let fresh = holders_snapshot();
        for l in leftovers {
            let held = match &fresh {
                Some(h) => h.iter().any(|h| h.path.starts_with(&l)),
                None => true,
            };
            if held {
                println!(
                    "kept    {}: in use, or open files could not be checked",
                    l.display()
                );
                continue;
            }
            match std::fs::remove_dir_all(&l) {
                Ok(()) => println!("deleted {}", l.display()),
                Err(e) => {
                    eprintln!("failed  {}: {e}", l.display());
                    failed = true;
                }
            }
        }
    }
    println!("reclaimed {}", human_bytes(freed));
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const NOW_SECS: u64 = 2_000_000_000;

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(NOW_SECS)
    }

    fn ago(mins: u64) -> SystemTime {
        now() - Duration::from_secs(mins * 60)
    }

    /// Set the mtime of `p` and every entry under it to `t`.
    fn touch_all(p: &Path, t: SystemTime) {
        if fs::symlink_metadata(p).unwrap().is_dir() {
            for e in fs::read_dir(p).unwrap() {
                touch_all(&e.unwrap().path(), t);
            }
            fs::File::open(p).unwrap().set_modified(t).unwrap();
        } else {
            fs::OpenOptions::new()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
    }

    /// A worktree with sources, an uncommitted file, a `.git` file and a cargo
    /// `target/` whose every entry was last modified at `t`.
    fn worktree(parent: &Path, name: &str, t: SystemTime) -> PathBuf {
        let wt = parent.join(name);
        fs::create_dir_all(wt.join("src")).unwrap();
        fs::write(wt.join("src/lib.rs"), "fn x() {}").unwrap();
        fs::write(wt.join("uncommitted.txt"), "work").unwrap();
        fs::write(wt.join(".git"), "gitdir: /elsewhere").unwrap();
        let deps = wt.join("target/debug/deps");
        fs::create_dir_all(&deps).unwrap();
        fs::write(
            wt.join("target/CACHEDIR.TAG"),
            format!("{CACHEDIR_SIGNATURE}\n"),
        )
        .unwrap();
        fs::write(deps.join("libfoo.rlib"), vec![0u8; 64 * 1024]).unwrap();
        touch_all(&wt.join("target"), t);
        wt.canonicalize().unwrap()
    }

    fn holder(pid: u32, command: &str, path: &Path, is_cwd: bool) -> Holder {
        Holder {
            pid,
            command: command.into(),
            path: path.to_path_buf(),
            is_cwd,
        }
    }

    #[test]
    fn porcelain_keeps_ordinary_worktrees_and_drops_bare_prunable_locked() {
        let s = "worktree /r/main\nHEAD abc\nbranch refs/heads/main\n\n\
                 worktree /r/repo-dispatch a\nHEAD def\ndetached\n\n\
                 worktree /r/bare\nbare\n\n\
                 worktree /r/gone\nHEAD 1\nprunable gitdir file points to non-existent location\n\n\
                 worktree /r/locked\nHEAD 2\nlocked on usb\n\n\
                 worktree /r/locked2\nHEAD 3\nlocked\n";
        assert_eq!(
            parse_worktree_porcelain(s),
            vec![
                PathBuf::from("/r/main"),
                PathBuf::from("/r/repo-dispatch a")
            ]
        );
    }

    #[test]
    fn lsof_records_carry_pid_command_and_cwd() {
        let s = "p12\ncrustc\nfcwd\nn/w/a\nftxt\nn/w/a/target/debug/x\np13\ncrust-analyzer-proc-macro-srv\nfmem\nn/w/a/target/debug/deps/libm.dylib\n";
        assert_eq!(
            parse_lsof(s),
            vec![
                holder(12, "rustc", Path::new("/w/a"), true),
                holder(12, "rustc", Path::new("/w/a/target/debug/x"), false),
                holder(
                    13,
                    "rust-analyzer-proc-macro-srv",
                    Path::new("/w/a/target/debug/deps/libm.dylib"),
                    false
                ),
            ]
        );
    }

    #[test]
    fn an_open_file_under_target_blocks_unless_rust_analyzer_holds_it() {
        let wt = Path::new("/w/a");
        let t = wt.join("target");
        let file = t.join("debug/dot-agent-deck");
        assert!(blocker(wt, &t, &[holder(1, "dot-agent-deck", &file, false)]).is_some());
        assert!(blocker(wt, &t, &[holder(1, "rust-analyzer", &file, false)]).is_none());
        assert!(
            blocker(
                wt,
                &t,
                &[holder(1, "rust-analyzer-proc-macro-srv", &file, false)]
            )
            .is_none()
        );
    }

    #[test]
    fn a_build_tool_running_in_the_worktree_blocks_but_a_shell_does_not() {
        let wt = Path::new("/w/a");
        let t = wt.join("target");
        let sub = wt.join("xtask/docs");
        assert!(blocker(wt, &t, &[holder(1, "cargo", &sub, true)]).is_some());
        assert!(blocker(wt, &t, &[holder(1, "rustc", wt, true)]).is_some());
        assert!(blocker(wt, &t, &[holder(1, "zsh", wt, true)]).is_none());
        assert!(blocker(wt, &t, &[holder(1, "claude", &sub, true)]).is_none());
    }

    #[test]
    fn a_sibling_worktree_sharing_a_name_prefix_does_not_block() {
        let wt = Path::new("/w/repo-dispatch-1");
        let t = wt.join("target");
        let other = Path::new("/w/repo-dispatch-10");
        assert!(blocker(wt, &t, &[holder(1, "cargo", other, true)]).is_none());
        assert!(
            blocker(
                wt,
                &t,
                &[holder(1, "x", &other.join("target/debug/x"), false)]
            )
            .is_none()
        );
    }

    #[test]
    fn only_a_tagged_real_directory_is_a_cargo_target() {
        let d = tempfile::tempdir().unwrap();
        let tagged = d.path().join("tagged");
        fs::create_dir(&tagged).unwrap();
        fs::write(tagged.join("CACHEDIR.TAG"), CACHEDIR_SIGNATURE).unwrap();
        assert!(is_cargo_target(&tagged));

        let untagged = d.path().join("untagged");
        fs::create_dir(&untagged).unwrap();
        assert!(!is_cargo_target(&untagged));

        let wrong = d.path().join("wrong");
        fs::create_dir(&wrong).unwrap();
        fs::write(wrong.join("CACHEDIR.TAG"), "Signature: something else").unwrap();
        assert!(!is_cargo_target(&wrong));

        // Cargo's other root file, for a tree cargo did not create.
        let info = d.path().join("info");
        fs::create_dir(&info).unwrap();
        fs::write(info.join(".rustc_info.json"), "{}").unwrap();
        assert!(is_cargo_target(&info));

        #[cfg(unix)]
        {
            let link = d.path().join("link");
            std::os::unix::fs::symlink(&tagged, &link).unwrap();
            assert!(!is_cargo_target(&link));
        }
    }

    #[test]
    fn an_idle_tree_is_reclaimed_and_a_recent_write_anywhere_keeps_it() {
        let d = tempfile::tempdir().unwrap();
        let wt = worktree(d.path(), "repo-dispatch-1", ago(120));
        let (target, v) = assess(&wt, Some(&[]), now(), &Opts::default()).unwrap();
        assert_eq!(target, wt.join("target"));
        assert!(matches!(v, Verdict::Reclaim { .. }), "{v:?}");

        // A write deep in the tree, without touching any directory above it.
        let deep = wt.join("target/debug/deps/libfoo.rlib");
        fs::OpenOptions::new()
            .write(true)
            .open(&deep)
            .unwrap()
            .set_modified(ago(5))
            .unwrap();
        let (_, v) = assess(&wt, Some(&[]), now(), &Opts::default()).unwrap();
        assert!(
            matches!(v, Verdict::Skip(ref s) if s.contains("written")),
            "{v:?}"
        );
    }

    #[test]
    fn min_size_keeps_a_small_tree() {
        let d = tempfile::tempdir().unwrap();
        let wt = worktree(d.path(), "w", ago(120));
        let opts = Opts {
            min_size_bytes: 1 << 30,
            ..Opts::default()
        };
        let (_, v) = assess(&wt, Some(&[]), now(), &opts).unwrap();
        assert!(
            matches!(v, Verdict::Skip(ref s) if s.contains("--min-size")),
            "{v:?}"
        );
    }

    #[test]
    fn an_unchecked_or_held_tree_is_kept() {
        let d = tempfile::tempdir().unwrap();
        let wt = worktree(d.path(), "w", ago(120));
        let (_, v) = assess(&wt, None, now(), &Opts::default()).unwrap();
        assert!(matches!(v, Verdict::Skip(_)), "{v:?}");

        let h = [holder(9, "cargo", &wt, true)];
        let (_, v) = assess(&wt, Some(&h), now(), &Opts::default()).unwrap();
        assert!(
            matches!(v, Verdict::Skip(ref s) if s.contains("build running")),
            "{v:?}"
        );
    }

    #[test]
    fn a_worktree_without_target_or_with_a_foreign_target_is_left_alone() {
        let d = tempfile::tempdir().unwrap();
        let bare = d.path().join("no-target");
        fs::create_dir(&bare).unwrap();
        assert!(assess(&bare, Some(&[]), now(), &Opts::default()).is_none());

        let foreign = d.path().join("foreign");
        fs::create_dir_all(foreign.join("target")).unwrap();
        let (_, v) = assess(&foreign, Some(&[]), now(), &Opts::default()).unwrap();
        assert!(matches!(v, Verdict::Skip(_)), "{v:?}");
    }

    #[test]
    fn a_future_mtime_counts_as_just_written() {
        let s = Scan {
            bytes: 1,
            newest: now() + Duration::from_secs(3600),
        };
        assert!(matches!(
            decide(&s, now(), &Opts::default()),
            Verdict::Skip(_)
        ));
    }

    #[test]
    fn reclaim_removes_only_target_and_leaves_no_parked_tree() {
        let d = tempfile::tempdir().unwrap();
        let wt = worktree(d.path(), "repo-dispatch-1", ago(120));
        let parked = park_path(&wt, "t1").unwrap();
        assert_eq!(parked.parent(), wt.parent());
        reclaim(&wt.join("target"), &parked).unwrap();

        assert!(!wt.join("target").exists());
        assert!(!parked.exists());
        assert_eq!(
            fs::read_to_string(wt.join("src/lib.rs")).unwrap(),
            "fn x() {}"
        );
        assert_eq!(
            fs::read_to_string(wt.join("uncommitted.txt")).unwrap(),
            "work"
        );
        assert!(wt.join(".git").exists());
    }

    #[test]
    fn a_parked_tree_left_by_an_interrupted_run_is_found_and_nothing_else_is() {
        let d = tempfile::tempdir().unwrap();
        let wt = worktree(d.path(), "repo-dispatch-1", ago(120));
        let parked = park_path(&wt, "t2").unwrap();
        fs::rename(wt.join("target"), &parked).unwrap();
        // Same prefix, but no cargo tag: not ours.
        fs::create_dir(d.path().join(format!("{PARK_PREFIX}other"))).unwrap();
        // A sibling worktree's own target/ is not a leftover.
        worktree(d.path(), "repo-dispatch-2", ago(120));

        assert_eq!(parked_leftovers(std::slice::from_ref(&wt)), vec![parked]);
    }

    #[test]
    fn args_parse_and_default_to_a_dry_run() {
        let a = |v: &[&str]| parse_args(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(a(&[]).unwrap().unwrap(), Opts::default());
        assert!(!Opts::default().apply);
        let o = a(&["--apply", "--min-idle", "60", "--min-size", "50"])
            .unwrap()
            .unwrap();
        assert!(o.apply);
        assert_eq!(o.min_idle, Duration::from_secs(3600));
        assert_eq!(o.min_size_bytes, 50 << 30);
        assert!(a(&["--min-idle", "x"]).is_err());
        assert!(a(&["--bogus"]).is_err());
    }
}
