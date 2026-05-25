use std::process::Command;

fn main() {
    // Derive version from git tags (e.g. "v0.7.1" -> "0.7.1", "v0.25.0-alpha.0" -> "0.25.0-alpha.0").
    // Falls back to CARGO_PKG_VERSION when not in a git repo or no tags exist.
    let version = git_version().unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=DAD_VERSION={version}");

    // PRD #103 M1.0: emit a finer-grained build identifier alongside DAD_VERSION.
    // Shape: `<DAD_VERSION>-g<short-sha>[-dirty]`, falling back to
    // `<DAD_VERSION>-unknown` when git metadata is unavailable.
    let build_id = compose_build_id(&version);
    println!("cargo:rustc-env=DAD_BUILD_ID={build_id}");

    // Re-run if HEAD changes (new commit, branch switch, detached-HEAD move).
    // `.git/HEAD` alone is necessary but not sufficient on a normal branch —
    // the file contents `ref: refs/heads/<branch>` don't change when commits
    // land on that branch. PRD #103 M1.0 prescribes also watching the
    // resolved ref file, the index (dirty/clean transitions), and
    // packed-refs (post-`git gc` fallback).
    //
    // In a git worktree, `.git` is a *file* containing
    // `gitdir: <main-repo>/.git/worktrees/<name>`, not a directory. The
    // literal `.git/HEAD` / `.git/index` paths then point at nothing and
    // `cargo:rerun-if-changed` silently no-ops, which means cached
    // `DAD_BUILD_ID` doesn't invalidate on commit (violates PRD invariant
    // 6). Resolve each path through `git rev-parse --git-path <name>` so
    // we watch the real file under `.git/worktrees/<name>/...` (or the
    // shared `commondir` for packed-refs). Fall back to the literal path
    // only if `git rev-parse` fails — matches the existing degrade-to-
    // unknown discipline elsewhere in this file.
    emit_rerun_if_changed_git_path("HEAD");
    emit_rerun_if_changed_git_path("index");
    emit_rerun_if_changed_git_path("packed-refs");
    if let Some(ref_path) = parse_head_ref_path() {
        emit_rerun_if_changed_git_path(&ref_path);
    }
}

/// Emit `cargo:rerun-if-changed` for a git-internal path resolved via
/// `git rev-parse --git-path <relative>`. In a worktree this returns the
/// real path under `.git/worktrees/<name>/...` (for HEAD/index) or the
/// shared `commondir` for `packed-refs` and `refs/heads/<branch>`.
///
/// Falls back to the literal `.git/<relative>` path if `git rev-parse`
/// fails (no git, shallow tarball, ...). Cargo's `rerun-if-changed`
/// silently tolerates non-existent paths, so the fallback is harmless
/// outside a real repo.
fn emit_rerun_if_changed_git_path(relative: &str) {
    let resolved = Command::new("git")
        .args(["rev-parse", "--git-path", relative])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!(".git/{relative}"));
    println!("cargo:rerun-if-changed={resolved}");
}

fn git_version() -> Option<String> {
    let output = Command::new("git")
        .args(["describe", "--tags", "--abbrev=0"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let tag = String::from_utf8(output.stdout).ok()?.trim().to_string();
    let stripped = tag
        .strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .unwrap_or(&tag);

    // SemVer check: digits.digits.digits, optionally followed by a `-<prerelease>`
    // suffix (alphanumeric + dots/dashes per the SemVer grammar). The pre-release
    // suffix is accepted opaquely — we only validate the X.Y.Z core.
    let core = stripped.split_once('-').map(|(c, _)| c).unwrap_or(stripped);
    let core_parts: Vec<&str> = core.split('.').collect();
    if core_parts.len() == 3 && core_parts.iter().all(|p| p.parse::<u64>().is_ok()) {
        Some(stripped.to_string())
    } else {
        None
    }
}

/// Compose `<version>-g<short-sha>[-dirty]`, or `<version>-unknown` when
/// git is not available. Mirrors the fallback discipline of `git_version`:
/// any git failure degrades to the `-unknown` sentinel rather than aborting
/// the build, so tarball / shallow-clone builds still produce a usable
/// `DAD_BUILD_ID`.
fn compose_build_id(version: &str) -> String {
    let Some(short_sha) = git_short_sha() else {
        return format!("{version}-unknown");
    };
    let dirty_suffix = if git_is_dirty() { "-dirty" } else { "" };
    format!("{version}-g{short_sha}{dirty_suffix}")
}

fn git_short_sha() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}

fn git_is_dirty() -> bool {
    let Ok(output) = Command::new("git").args(["status", "--porcelain"]).output() else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    !output.stdout.is_empty()
}

/// Resolve the symbolic ref path HEAD points at (e.g. `refs/heads/main`)
/// via `git symbolic-ref -q HEAD`. Returns `None` on detached HEAD (the
/// existing HEAD watch already covers that case) or when git isn't
/// available.
///
/// We can't read `.git/HEAD` directly: in a worktree, `.git` is a *file*
/// containing `gitdir: ...`, not a directory, so the literal path
/// points at nothing. `git symbolic-ref` handles both layouts uniformly
/// and prints just the ref path (or fails silently with a non-zero exit
/// when HEAD is detached).
fn parse_head_ref_path() -> Option<String> {
    let output = Command::new("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let ref_path = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if ref_path.is_empty() {
        None
    } else {
        Some(ref_path)
    }
}
