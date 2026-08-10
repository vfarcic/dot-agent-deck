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
//! - `dot-agent-deck-test-lock-*` — the pre-fix lock dirs. Also ours; still
//!   present in bulk on machines that ran the suite before the leak was fixed.
//! - `.tmp*` — **not** reaped unless `--include-untagged` is passed. That is
//!   the `tempfile` crate's *default* prefix, so it belongs to every Rust
//!   program on the machine, not just this suite. Globbing it blindly can
//!   delete a live temp dir owned by something else entirely.
//!
//! Dry-run is the default; `--apply` is required to remove anything.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

/// Directory-name prefixes this repo owns outright and may reap by default.
const OWNED_PREFIXES: &[&str] = &["dad-tests-", "dot-agent-deck-test-lock-"];

/// The `tempfile` crate's default prefix — shared with every other Rust
/// program, so it is opt-in only.
const UNTAGGED_PREFIX: &str = ".tmp";

const DEFAULT_MAX_AGE_HOURS: u64 = 6;

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
    let opts = match parse_args(args) {
        Ok(Some(opts)) => opts,
        Ok(None) => return ExitCode::SUCCESS, // --help
        Err(msg) => {
            eprintln!("xtask clean-e2e-tmp: {msg}");
            usage();
            return ExitCode::from(2);
        }
    };

    let temp_root = std::env::temp_dir();
    let candidates = match collect(&temp_root, &opts) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "xtask clean-e2e-tmp: cannot read {}: {e}",
                temp_root.display()
            );
            return ExitCode::FAILURE;
        }
    };

    if candidates.is_empty() {
        println!(
            "nothing to reap in {} (no owned dirs older than {})",
            temp_root.display(),
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
        "{} dir(s), {} total, older than {} in {}",
        candidates.len(),
        human_bytes(total),
        human_duration(opts.max_age),
        temp_root.display(),
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

fn parse_args(args: &[String]) -> Result<Option<Options>, String> {
    let mut opts = Options {
        max_age: Duration::from_secs(DEFAULT_MAX_AGE_HOURS * 3600),
        apply: false,
        include_untagged: false,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--apply" => opts.apply = true,
            "--include-untagged" => opts.include_untagged = true,
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
    Ok(Some(opts))
}

fn usage() {
    println!(
        "usage: cargo xtask clean-e2e-tmp [--older-than <hours>] [--apply] [--include-untagged]"
    );
    println!();
    println!("Reaps stale e2e harness temp dirs left by SIGKILLed test processes.");
    println!("Dry-run by default; --apply is required to delete.");
    println!();
    println!("  --older-than <hours>  age threshold (default: {DEFAULT_MAX_AGE_HOURS})");
    println!("  --apply               actually remove the directories");
    println!("  --include-untagged    ALSO reap `{UNTAGGED_PREFIX}*` dirs. These use the");
    println!("                        tempfile crate's DEFAULT prefix and are shared with");
    println!("                        every Rust program on this machine — only use this");
    println!("                        when no other Rust build or tool is running.");
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
    out.sort_by_key(|entry| std::cmp::Reverse(entry.bytes));
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
}
