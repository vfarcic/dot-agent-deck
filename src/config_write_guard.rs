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
//!   its environment — every integration-test binary under this repo's test
//!   aliases, and every child that did not clear its environment. Both `NEXTEST`
//!   and `NEXTEST_RUN_ID` must be non-empty: nextest sets the pair for every
//!   test it runs, while a stray `NEXTEST=1` left in a user's shell is one
//!   variable, and arming on it alone would refuse that user's real config
//!   writes (PRD #1487 review S4). When
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
//!
//! # What it is not
//!
//! Best-effort protection against **accidental** misconfiguration — a test
//! that forgot to isolate `HOME` — and not a sandbox (PRD #1487 audit A4). The
//! check judges the path when it is called; the writer then creates, renames
//! and removes by pathname. A fixture that swaps a directory on the path for a
//! symlink *between* the check and the write — a concurrent ancestor swap — is
//! outside what it guarantees. Nothing in the deck does that, and a test that
//! did would be attacking its own fixture.
//!
//! A refusal is logged at `warn!` with the destination and the reason, so a
//! write that did not happen is never silent.

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
    check_config_write(dest).inspect_err(|e| {
        tracing::warn!(
            destination = %dest.display(),
            reason = %e,
            "agent config write refused by test containment"
        );
    })
}

/// Whether [`ensure_config_write_allowed`] would let `dest` be written, without
/// its log line. For a side effect that is only worth taking when the write it
/// serves can happen — `agent_hook_config::lock_config`'s sidecar — and whose
/// absence is not itself a refusal to report.
pub(crate) fn config_write_allowed(dest: &Path) -> bool {
    check_config_write(dest).is_ok()
}

/// [`ensure_config_write_allowed`] without the log line.
fn check_config_write(dest: &Path) -> io::Result<()> {
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
    let roots = owned_roots(&candidates);
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

/// The candidates that name an existing absolute directory, canonicalized. A
/// root that resolves to anything else — a file, say — contributes nothing:
/// containment under a file would admit a write to the file itself (PRD #1487,
/// Qodo 4202781000).
fn owned_roots(candidates: &[PathBuf]) -> Vec<PathBuf> {
    candidates
        .iter()
        .filter(|root| root.is_absolute())
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .filter(|root| root.is_dir())
        .collect()
}

/// Whether this process is a test: the lib's own unit-test binary, or a
/// process `cargo nextest` started (directly or as an ancestor whose
/// environment was inherited). nextest sets `NEXTEST` and `NEXTEST_RUN_ID` for
/// every test it runs, and nextest is what every test alias in
/// `.cargo/config.toml` runs. Both are required (review S4): `NEXTEST` alone is
/// a generic-looking name a user's shell can carry for unrelated reasons.
fn running_under_test_runner() -> bool {
    cfg!(test) || nextest_armed(|name| std::env::var_os(name))
}

/// The nextest half of [`running_under_test_runner`], over an environment
/// lookup so it can be tested without touching the process environment.
fn nextest_armed(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    let set = |name: &str| var(name).is_some_and(|value| !value.is_empty());
    set("NEXTEST") && set("NEXTEST_RUN_ID")
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

/// How many times [`resolve_for_containment`] resolves an entry again when it
/// was missing and then present — another writer's rename landing between the
/// two looks — before it refuses the path.
const RESOLVE_RACE_RETRIES: u32 = 20;

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
    let mut raced = 0;
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
                match std::fs::symlink_metadata(existing) {
                    // A dangling symlink. A writer would follow or replace it;
                    // neither is judged here.
                    Ok(meta) if meta.file_type().is_symlink() => {
                        return Err(refuse("an entry on the path exists but does not resolve"));
                    }
                    // Not a symlink, so it appeared between the two calls: a
                    // concurrent writer's rename landed there, as eight
                    // `codex_trust_008` writers did on a Windows runner.
                    // Resolve it again, a bounded number of times.
                    Ok(_) if raced < RESOLVE_RACE_RETRIES => {
                        raced += 1;
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    Ok(_) => {
                        return Err(refuse("an entry on the path exists but does not resolve"));
                    }
                    Err(_) => {}
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

    /// Scenario: a root that names a file, a missing path or a relative path
    /// contributes no owned root; only an existing absolute directory does.
    #[test]
    fn only_existing_absolute_directories_are_roots() {
        let fixture = crate::test_temp::tempdir().unwrap();
        let dir = std::fs::canonicalize(fixture.path()).unwrap();
        let file = dir.join("config.toml");
        std::fs::write(&file, "").unwrap();
        std::os::unix::fs::symlink(&file, dir.join("file-link")).unwrap();
        assert!(owned_roots(std::slice::from_ref(&file)).is_empty());
        assert!(owned_roots(&[dir.join("file-link")]).is_empty());
        assert!(owned_roots(&[dir.join("missing")]).is_empty());
        assert!(owned_roots(&[PathBuf::from("relative")]).is_empty());
        assert_eq!(owned_roots(&[file, dir.clone()]), vec![dir]);
    }

    /// Scenario: a stray `NEXTEST` alone does not arm containment; nextest's
    /// own pair does (PRD #1487 review S4).
    #[test]
    fn nextest_arms_only_with_its_run_id() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| std::ffi::OsString::from(v))
            }
        };
        assert!(!nextest_armed(env(&[("NEXTEST", "1")])));
        assert!(!nextest_armed(env(&[
            ("NEXTEST", "1"),
            ("NEXTEST_RUN_ID", "")
        ])));
        assert!(!nextest_armed(env(&[("NEXTEST_RUN_ID", "r")])));
        assert!(nextest_armed(env(&[
            ("NEXTEST", "1"),
            ("NEXTEST_RUN_ID", "r")
        ])));
    }
}
