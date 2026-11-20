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
    let mut found = Vec::new();
    let mut read = 0usize;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        if Instant::now() >= deadline {
            return Err(format!("the look inside {} ran out of time", dir.display()));
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
