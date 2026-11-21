//! PRD #1790 stage 2 (final review, auditor 1): the Git working trees of ANY
//! repository that lie inside a directory a removal would delete.
//!
//! `git worktree list` names the worktrees of one repository, so a linked
//! worktree of another repository — or a plain clone — placed inside a
//! dispatched worktree is invisible to it, and under a git-ignored directory
//! it is invisible to `git status` too. `git worktree remove` then deletes
//! it, its uncommitted files included. This looks at the directory itself:
//! a `.git` entry, file or directory, anywhere below the top level marks the
//! directory holding it as a working tree.
//!
//! The walk does not follow symbolic links, so it never leaves the directory
//! and a link cannot make it loop; a link is not something the removal would
//! follow either. It is bounded by an entry count and a deadline, and an
//! answer it could not finish — a bound reached, a directory it could not
//! read — is an `Err`, which every caller reads as a reason to keep.
//!
//! A directory below the top level is looked at for a `.git` entry before
//! its other entries are read, so a nested tree is named without its own top
//! level counting against the bound.
//!
//! The deadline is checked before each directory is opened, after each entry
//! is read and before an answer is returned, so a walk that passes it never
//! answers. It is not a deadline on a single filesystem call: a `read_dir` or
//! entry read that blocks in the kernel is not interrupted, and the walk
//! notices the time only once that call returns. The async wrapper stops
//! waiting at the deadline either way; the walk on the blocking pool ends at
//! its next check.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The most directory entries one walk reads before it gives up.
pub const MAX_SCAN_ENTRIES: usize = 1_000_000;

/// How long one walk may take before it gives up, for a caller with no
/// deadline of its own.
pub const SCAN_TIMEOUT: Duration = Duration::from_secs(30);

/// The working trees inside `dir`, below its top level, each named as `dir`
/// joined with its path relative to `dir`; at most `max_found` are answered.
/// `Err` when the walk read more than `max_entries` entries, passed
/// `deadline`, or could not read a directory.
pub fn git_trees_inside(
    dir: &Path,
    max_entries: usize,
    max_found: usize,
    deadline: Instant,
) -> Result<Vec<PathBuf>, String> {
    walk_with(dir, max_entries, max_found, deadline, Instant::now)
}

/// [`git_trees_inside`], reading the time from `now`, so a test can move the
/// clock while one directory is being read.
fn walk_with(
    dir: &Path,
    max_entries: usize,
    max_found: usize,
    deadline: Instant,
    mut now: impl FnMut() -> Instant,
) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    let mut read = 0usize;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        if now() >= deadline {
            return Err(format!("the look inside {} ran out of time", dir.display()));
        }
        // Below the top level, a `.git` entry makes the directory a tree of
        // its own: name it without reading its other entries (PR #2431
        // review). Only a `.git` that is there settles it; on any other
        // answer the directory is read, which finds a `.git` the look could
        // not reach, or fails as it always did.
        if current != dir && std::fs::symlink_metadata(current.join(".git")).is_ok() {
            found.push(current);
            if found.len() >= max_found {
                break;
            }
            continue;
        }
        let entries = match std::fs::read_dir(&current) {
            Ok(entries) => entries,
            // Gone since it was seen, or the tree itself is gone: nothing
            // left in it to lose.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("could not read {}: {e}", current.display())),
        };
        let mut subdirs = Vec::new();
        let mut holds_git = false;
        for entry in entries {
            let entry = entry.map_err(|e| format!("could not read {}: {e}", current.display()))?;
            read += 1;
            if now() >= deadline {
                return Err(format!("the look inside {} ran out of time", dir.display()));
            }
            if read > max_entries {
                return Err(format!(
                    "{} holds more than {max_entries} entries to look through",
                    dir.display()
                ));
            }
            if entry.file_name() == ".git" {
                holds_git = true;
                continue;
            }
            // `DirEntry::file_type` does not follow a symbolic link.
            let kind = entry
                .file_type()
                .map_err(|e| format!("could not read {}: {e}", entry.path().display()))?;
            if kind.is_dir() {
                subdirs.push(entry.path());
            }
        }
        if holds_git && current != dir {
            // Everything below it is the nested tree's; one name covers it.
            found.push(current);
            if found.len() >= max_found {
                break;
            }
            continue;
        }
        pending.extend(subdirs);
    }
    if now() >= deadline {
        return Err(format!("the look inside {} ran out of time", dir.display()));
    }
    Ok(found)
}

/// [`git_trees_inside`] on the blocking pool, giving up at `deadline`.
pub async fn git_trees_inside_async(
    dir: &Path,
    max_found: usize,
    deadline: Instant,
) -> Result<Vec<PathBuf>, String> {
    let dir = dir.to_path_buf();
    let Some(wait) = deadline.checked_duration_since(Instant::now()) else {
        return Err("out of time".to_string());
    };
    let walk = tokio::task::spawn_blocking(move || {
        git_trees_inside(&dir, MAX_SCAN_ENTRIES, max_found, deadline)
    });
    match tokio::time::timeout(wait, walk).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(e)) => Err(format!("the walk failed: {e}")),
        Err(_) => Err("out of time".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    fn walk(dir: &Path) -> Result<Vec<PathBuf>, String> {
        git_trees_inside(dir, MAX_SCAN_ENTRIES, 16, soon())
    }

    /// A `.git` file (a linked worktree's) and a `.git` directory (a clone's)
    /// below the top level are each found, named under the walked path; the
    /// top level's own `.git` is not, and a tree found is not walked into.
    #[test]
    fn finds_every_working_tree_below_the_top_level_but_not_its_own() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let top = tmp.path();
        std::fs::write(top.join(".git"), "gitdir: /elsewhere\n").unwrap();
        let linked = top.join(".worktrees").join("child");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(linked.join(".git"), "gitdir: /other/.git/worktrees/child\n").unwrap();
        let clone = top.join("vendor").join("deep").join("clone");
        std::fs::create_dir_all(clone.join(".git").join("objects")).unwrap();
        let inner = clone.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join(".git"), "gitdir: x\n").unwrap();
        std::fs::create_dir_all(top.join("src")).unwrap();
        std::fs::write(top.join("src").join("main.rs"), "").unwrap();

        let mut found = walk(top).expect("answered");
        found.sort();
        assert_eq!(found, vec![linked, clone]);
    }

    /// Nothing inside: an empty answer, not an error.
    #[test]
    fn a_tree_with_nothing_inside_answers_empty() {
        let tmp = crate::test_temp::tempdir().unwrap();
        std::fs::write(tmp.path().join(".git"), "gitdir: x\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("a").join("b")).unwrap();
        assert_eq!(walk(tmp.path()), Ok(Vec::new()));
    }

    /// A symbolic link to a directory holding a working tree is not followed.
    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_is_not_followed() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(outside.join("tree")).unwrap();
        std::fs::write(outside.join("tree").join(".git"), "gitdir: x\n").unwrap();
        let top = tmp.path().join("top");
        std::fs::create_dir_all(&top).unwrap();
        std::os::unix::fs::symlink(&outside, top.join("link")).unwrap();
        std::os::unix::fs::symlink(outside.join("tree").join(".git"), top.join("not-a-tree"))
            .unwrap_or(());
        assert_eq!(walk(&top), Ok(Vec::new()));
    }

    /// PR #2431 round 4 (Qodo): a directory below the top level holding a
    /// symbolic link named `.git`, dangling or not, is named as a working
    /// tree, so a removal keeps the tree that holds it. The walk did this
    /// before `e0be7194` too (it marked any entry named `.git`, whatever its
    /// type); keeping is the safe answer, since the link may lead to a real
    /// repository's metadata.
    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_named_git_names_its_directory_as_a_tree() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let outside = tmp.path().join("outside-repo");
        std::fs::create_dir_all(outside.join(".git")).unwrap();
        let top = tmp.path().join("top");
        let linked = top.join("ignored").join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        std::os::unix::fs::symlink(outside.join(".git"), linked.join(".git")).unwrap();
        let dangling = top.join("ignored").join("dangling");
        std::fs::create_dir_all(&dangling).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), dangling.join(".git")).unwrap();

        let mut found = walk(&top).expect("answered");
        found.sort();
        assert_eq!(found, vec![dangling, linked]);
    }

    /// The entry bound and the deadline each end the walk with an `Err`.
    #[test]
    fn a_bound_reached_is_an_error() {
        let tmp = crate::test_temp::tempdir().unwrap();
        for n in 0..20 {
            std::fs::create_dir_all(tmp.path().join(format!("d{n}"))).unwrap();
        }
        assert!(git_trees_inside(tmp.path(), 5, 16, soon()).is_err());
        assert!(git_trees_inside(tmp.path(), MAX_SCAN_ENTRIES, 16, Instant::now()).is_err());
        assert_eq!(
            git_trees_inside(tmp.path(), MAX_SCAN_ENTRIES, 16, soon()),
            Ok(Vec::new())
        );
    }

    /// Verification finding D: the deadline passing while one directory's
    /// entries are being read ends the walk with an `Err`, rather than an
    /// answer finished late.
    #[test]
    fn a_deadline_passed_inside_one_directory_is_an_error() {
        let tmp = crate::test_temp::tempdir().unwrap();
        for n in 0..8 {
            std::fs::write(tmp.path().join(format!("f{n}")), "").unwrap();
        }
        let start = Instant::now();
        let deadline = start + Duration::from_secs(30);
        let mut calls = 0usize;
        // The first reading of the clock is in time; every later one is past
        // the deadline, as if the directory took that long to read.
        let clock = || {
            calls += 1;
            if calls == 1 {
                start
            } else {
                deadline + Duration::from_secs(1)
            }
        };
        let answer = walk_with(tmp.path(), MAX_SCAN_ENTRIES, 16, deadline, clock);
        assert!(answer.is_err(), "{answer:?}");
    }

    /// A directory that cannot be read may hold a working tree: `Err`.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions. Permissions do not bind for
        // root.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let tmp = crate::test_temp::tempdir().unwrap();
        let blocked = tmp.path().join("blocked");
        std::fs::create_dir_all(&blocked).unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let answer = walk(tmp.path());
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(answer.is_err(), "{answer:?}");
    }

    /// PR #2431 review (Qodo): a directory below the top level is checked
    /// for `.git` before its entries are read, so a nested tree whose own top
    /// level holds more entries than the bound is found, not given up on, and
    /// its entries are not counted against the walk's bound.
    #[test]
    fn a_nested_tree_is_found_before_its_entries_are_read() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let nested = tmp.path().join("vendor").join("big-clone");
        std::fs::create_dir_all(nested.join(".git")).unwrap();
        for n in 0..20 {
            std::fs::write(nested.join(format!("f{n}")), "").unwrap();
        }
        assert_eq!(
            git_trees_inside(tmp.path(), 10, 16, soon()),
            Ok(vec![nested])
        );
    }

    /// At most `max_found` are answered; one is enough to keep a tree.
    #[test]
    fn at_most_max_found_are_answered() {
        let tmp = crate::test_temp::tempdir().unwrap();
        for n in 0..5 {
            let tree = tmp.path().join(format!("t{n}"));
            std::fs::create_dir_all(&tree).unwrap();
            std::fs::write(tree.join(".git"), "gitdir: x\n").unwrap();
        }
        assert_eq!(
            git_trees_inside(tmp.path(), MAX_SCAN_ENTRIES, 2, soon())
                .unwrap()
                .len(),
            2
        );
    }
}
