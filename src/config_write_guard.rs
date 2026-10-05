//! Test-write containment for every agent-config writer (PRD #1487).
//!
//! The deck writes into configuration files that belong to other programs:
//! Codex's `hooks.json` and `config.toml`, Claude Code's `settings.json`,
//! Devin's `config.json`, the OpenCode plugin and the Pi extension. In
//! production those live under the user's real home, which is the point. In a
//! test they must not: on 2026-10-03 fast-tier fixtures that spawned Codex
//! stand-ins without isolating `HOME` let the wrapper's automatic install
//! rewrite the operator's real `~/.codex/hooks.json`, and Codex then held every
//! start on its hook-review screen.
//!
//! [`ensure_config_write_allowed`] is the one check every writer calls before
//! it creates a directory, a temp file or a backup, renames over a
//! destination, or deletes one. It is inert for a user; in a test process it
//! refuses any destination that does not resolve inside an owned root.
//!
//! # When it is armed
//!
//! - **Explicitly**, by a non-empty [`MARKER_ENV`]. The owned root(s) are then
//!   [`ROOT_ENV`] and nothing else: an unset or empty root refuses every
//!   write. This is the contract a fixture spawning a child with a cleared
//!   environment hands that child.
//! - **Automatically in a test process**: in the lib target's own unit tests
//!   (`cfg(test)`), and in any process `cargo nextest` started or that inherited
//!   its `NEXTEST` variable — every integration-test binary under this repo's
//!   test aliases, and every child that did not clear its environment. When
//!   [`ROOT_ENV`] is unset there, the roots default to the places tests make
//!   scratch directories ([`default_test_roots`]); the user's home is not one
//!   of them.
//!
//! A child started with a cleared environment inherits neither signal, which is
//! why the e2e harness pins both variables into the environments it builds.
//!
//! # How a destination is judged
//!
//! The destination is made absolute, then its longest existing ancestor is
//! canonicalized — resolving every symlink and `..` the way the kernel will
//! when the writer opens it — and the components that do not exist yet are
//! appended. A missing component that is a `..`, or an entry that exists but
//! cannot be resolved (a dangling symlink), is refused rather than guessed at.
//! The result must lie under one canonicalized root.

use std::io;
use std::path::{Component, Path, PathBuf};

/// Arms containment explicitly. Any non-empty value.
pub const MARKER_ENV: &str = "DOT_AGENT_DECK_TEST_CONFIG_WRITE";

/// The owned root(s) an armed writer may write under, separated like `PATH`.
pub const ROOT_ENV: &str = "DOT_AGENT_DECK_TEST_CONFIG_ROOT";

/// Refuse `dest` unless containment is unarmed or `dest` resolves inside an
/// owned root. Call it before the first side effect of a write — a
/// `create_dir_all`, a temp file, a backup, a rename or a removal.
pub(crate) fn ensure_config_write_allowed(dest: &Path) -> io::Result<()> {
    let Some(roots) = armed_roots()? else {
        return Ok(());
    };
    let resolved = resolve_for_containment(dest)?;
    if roots.iter().any(|root| resolved.starts_with(root)) {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "refusing to write agent config at {} (resolves to {}): this is a test process and \
             the path is outside its owned root ({}). Point HOME / CODEX_HOME / XDG_CONFIG_HOME \
             / PI_CODING_AGENT_DIR at a directory under {ROOT_ENV}.",
            dest.display(),
            resolved.display(),
            roots
                .iter()
                .map(|root| root.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ))
}

/// `None` when containment is not armed; otherwise the canonical owned roots,
/// or an error when there is none to write under.
fn armed_roots() -> io::Result<Option<Vec<PathBuf>>> {
    let explicit = std::env::var_os(MARKER_ENV).is_some_and(|value| !value.is_empty());
    if !explicit && !running_under_test_runner() {
        return Ok(None);
    }
    let candidates: Vec<PathBuf> = match std::env::var_os(ROOT_ENV) {
        Some(value) => std::env::split_paths(&value)
            .filter(|root| !root.as_os_str().is_empty())
            .collect(),
        None if explicit => Vec::new(),
        None => default_test_roots(),
    };
    let roots: Vec<PathBuf> = candidates
        .iter()
        .filter(|root| root.is_absolute())
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect();
    if roots.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing to write agent config: this is a test process ({MARKER_ENV} or the \
                 test runner armed containment) and {ROOT_ENV} names no existing absolute \
                 directory to write under"
            ),
        ));
    }
    Ok(Some(roots))
}

/// Whether this process is a test: the lib's own unit-test binary, or a
/// process `cargo nextest` started (directly or as an ancestor whose
/// environment was inherited). `NEXTEST` is set by nextest for every test it
/// runs, and nextest is what every test alias in `.cargo/config.toml` runs.
fn running_under_test_runner() -> bool {
    cfg!(test) || std::env::var_os("NEXTEST").is_some_and(|value| !value.is_empty())
}

/// Where tests make scratch directories, used as the owned roots when a test
/// process is armed without [`ROOT_ENV`]: the harness's and `test_temp`'s
/// private base `/var/tmp/dad-e2e-<uid>`, a base moved with `DAD_E2E_TMPDIR`,
/// and the OS temp dir (the harness's last rung, and where a bare
/// `tempfile::tempdir()` lands).
pub fn default_test_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    #[cfg(unix)]
    roots.push(
        PathBuf::from("/var/tmp")
            .join(format!("dad-e2e-{}", crate::platform::paths::current_uid())),
    );
    if let Some(moved) = std::env::var_os("DAD_E2E_TMPDIR").filter(|v| !v.is_empty()) {
        roots.push(PathBuf::from(moved));
    }
    roots.push(std::env::temp_dir());
    roots
}

/// Resolve `dest` the way the writer's own open will: canonicalize the longest
/// existing ancestor, then append what does not exist yet.
fn resolve_for_containment(dest: &Path) -> io::Result<PathBuf> {
    let refuse = |why: &str| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing to write agent config at {}: {why}",
                dest.display()
            ),
        )
    };
    let absolute = std::path::absolute(dest)?;
    let mut existing = absolute.as_path();
    let mut missing: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        match std::fs::canonicalize(existing) {
            Ok(canonical) => {
                let mut resolved = canonical;
                for name in missing.iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Something is there but does not resolve: a dangling symlink.
                // A writer would follow or replace it; neither is judged here.
                if std::fs::symlink_metadata(existing).is_ok() {
                    return Err(refuse("an entry on the path exists but does not resolve"));
                }
                match existing.components().next_back() {
                    Some(Component::Normal(name)) => missing.push(name),
                    _ => return Err(refuse("a missing component is `..` or `.`")),
                }
                existing = existing
                    .parent()
                    .ok_or_else(|| refuse("no ancestor of the path exists"))?;
            }
            Err(e) => return Err(refuse(&format!("cannot resolve the path: {e}"))),
        }
    }
}

// Unix-only: the one test needs a symlink.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The lib's unit tests are armed by `cfg(test)` alone, and resolve a path
    /// through a symlink and `..` the way the kernel does.
    #[test]
    fn resolve_follows_symlinks_and_parent_components() {
        let fixture = crate::test_temp::tempdir().unwrap();
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        std::os::unix::fs::symlink(root.join("outside"), root.join("a/link")).unwrap();
        assert_eq!(
            resolve_for_containment(&root.join("a/b/../link/new/file")).unwrap(),
            root.join("outside/new/file")
        );
        assert!(resolve_for_containment(&root.join("a/missing/../file")).is_err());
        std::os::unix::fs::symlink(root.join("gone"), root.join("dangling")).unwrap();
        assert!(resolve_for_containment(&root.join("dangling/file")).is_err());
        assert!(running_under_test_runner());
    }
}
