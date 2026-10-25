//! Worktrees: which one a directory is in, the pseudonymous id the collector
//! sends for it, and how much disk it holds.
//!
//! Several runs normally share a worktree, so disk is reported per worktree,
//! never per run. A path is never sent unless the user opts in: the id is a
//! SHA-256 of the canonical path, which `dot-agent-deck-collector worktree-id`
//! computes for a path the user names.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The git toplevel of `dir`: the nearest directory, `dir` itself included,
/// holding an entry named `.git` (a directory for a main checkout, a file for
/// a linked worktree). `None` when no ancestor has one.
pub fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|candidate| fs::symlink_metadata(candidate.join(".git")).is_ok())
        .map(Path::to_path_buf)
}

/// `deck.worktree.id`: the lowercase hex of the first 16 bytes of SHA-256
/// over the path's bytes. The path is expected canonical
/// ([`canonical_toplevel`]), so one worktree has one id.
pub fn worktree_id(canonical: &Path) -> String {
    let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// The canonical git toplevel of `path`, which must exist.
pub fn canonical_toplevel(path: &Path) -> io::Result<PathBuf> {
    let canonical = fs::canonicalize(path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    let dir = if canonical.is_dir() {
        canonical.as_path()
    } else {
        canonical.parent().unwrap_or(&canonical)
    };
    git_toplevel(dir).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} is inside no git worktree", path.display()),
        )
    })
}

/// The disk `root` holds: `st_blocks × 512` (allocated bytes, which is what
/// fills a disk) over `root` and everything under it, directories included,
/// with each inode counted once. The walk does not follow symlinks, does not
/// enter a directory on another filesystem, skips what it cannot read, and skips a
/// subdirectory that holds its own `.git` (a nested repository or worktree,
/// reported on its own when a run uses it). Fails only when `root` itself
/// cannot be read.
pub fn disk_usage(root: &Path) -> io::Result<u64> {
    let meta = fs::symlink_metadata(root)?;
    let device = meta.dev();
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    seen.insert((meta.dev(), meta.ino()));
    let mut total = meta.blocks() * 512;
    if !meta.is_dir() {
        return Ok(total);
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            // Only a directory can be another filesystem's mount point. A
            // file's device is not compared: on overlayfs it is the
            // underlying filesystem's, not the overlay's its directory reports.
            if meta.is_dir()
                && (meta.dev() != device || fs::symlink_metadata(path.join(".git")).is_ok())
            {
                continue;
            }
            if !seen.insert((meta.dev(), meta.ino())) {
                continue;
            }
            total += meta.blocks() * 512;
            if meta.is_dir() {
                stack.push(path);
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage_of(path: &Path) -> u64 {
        fs::symlink_metadata(path).unwrap().blocks() * 512
    }

    /// Write `len` incompressible bytes and flush them, so the file's
    /// allocated size is settled before anything counts it, and a
    /// compressing filesystem cannot shrink it to almost nothing.
    fn write_allocated(path: &Path, len: usize) {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let bytes: Vec<u8> = (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let mut file = fs::File::create(path).unwrap();
        std::io::Write::write_all(&mut file, &bytes).unwrap();
        file.sync_all().unwrap();
    }

    #[test]
    fn toplevel_is_the_nearest_directory_with_a_git_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        fs::create_dir_all(main.join(".git")).unwrap();
        fs::create_dir_all(main.join("src/deep")).unwrap();
        let linked = tmp.path().join("linked");
        fs::create_dir_all(linked.join("sub")).unwrap();
        fs::write(
            linked.join(".git"),
            "gitdir: ../main/.git/worktrees/linked\n",
        )
        .unwrap();

        assert_eq!(git_toplevel(&main.join("src/deep")), Some(main.clone()));
        assert_eq!(git_toplevel(&main), Some(main.clone()));
        assert_eq!(git_toplevel(&linked.join("sub")), Some(linked.clone()));
        let outside = tmp.path().join("plain");
        fs::create_dir_all(&outside).unwrap();
        // The temp directory's own ancestors hold no `.git` on a sane box;
        // whatever they hold, the answer is not inside `plain`.
        assert!(git_toplevel(&outside).is_none_or(|t| !t.starts_with(&outside)));
    }

    #[test]
    fn worktree_id_is_a_stable_pseudonym() {
        let id = worktree_id(Path::new("/home/u/code/repo"));
        assert_eq!(id.len(), 32);
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_eq!(id, worktree_id(Path::new("/home/u/code/repo")));
        assert_ne!(id, worktree_id(Path::new("/home/u/code/repo2")));
        // The first 16 bytes of SHA-256("/a"), as `printf /a | sha256sum` prints them.
        assert_eq!(
            worktree_id(Path::new("/a")),
            "6a50dc8584134c7de537c0052ff6d236"
        );
    }

    #[test]
    fn canonical_toplevel_resolves_symlinks_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/lib.rs"), "x").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&repo, &link).unwrap();
        let expected = fs::canonicalize(&repo).unwrap();
        assert_eq!(canonical_toplevel(&link.join("src")).unwrap(), expected);
        assert_eq!(
            canonical_toplevel(&repo.join("src/lib.rs")).unwrap(),
            expected
        );
        assert!(canonical_toplevel(&tmp.path().join("missing")).is_err());
    }

    #[test]
    fn usage_counts_allocated_bytes_once_per_inode_and_skips_nested_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("wt");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        let big = root.join("target/debug/big.bin");
        write_allocated(&big, 256 * 1024);
        fs::hard_link(&big, root.join("target/debug/again.bin")).unwrap();
        std::os::unix::fs::symlink("/", root.join("escape")).unwrap();
        let nested = root.join("nested");
        fs::create_dir_all(nested.join(".git")).unwrap();
        write_allocated(&nested.join("huge.bin"), 512 * 1024);

        let total = disk_usage(&root).unwrap();
        let expected: u64 = [
            root.clone(),
            root.join(".git"),
            root.join("target"),
            root.join("target/debug"),
            big.clone(),
            root.join("escape"),
        ]
        .iter()
        .map(|p| usage_of(p))
        .sum();
        assert_eq!(total, expected);
        assert!(total >= 256 * 1024, "{total}");
        assert!(
            total < 256 * 1024 + 512 * 1024,
            "{total}: nested repo or hard link counted"
        );
        assert_eq!(disk_usage(&nested).unwrap(), {
            let n: u64 = [nested.clone(), nested.join(".git"), nested.join("huge.bin")]
                .iter()
                .map(|p| usage_of(p))
                .sum();
            n
        });
    }

    #[test]
    fn usage_of_a_missing_root_fails() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(disk_usage(&tmp.path().join("gone")).is_err());
    }
}
