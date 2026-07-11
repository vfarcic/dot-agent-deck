//! Bundling + materialization of the Pi orchestrator extension, and the pure
//! decision core for `dot-agent-deck orchestrator setup` (PRD #201, M3.1 + M3.2).
//!
//! Design Decision #1: we bundle the *glue* and detect the *engine*. The Pi
//! runtime is a detected PATH dependency (like `claude`/`opencode`); the one
//! thing that gives us deterministic control — our TypeScript orchestrator
//! extension — is compiled into this binary via [`include_str!`] and
//! materialized on demand into Pi's global extension directory.
//!
//! Keeping the embedded strings pointed at the real `pi-extension/src/` sources
//! (not a fork under `src/`) means a future edit to the extension flows into the
//! binary on the next `cargo build` with no copy step to forget.

use std::path::{Path, PathBuf};

/// The one command that installs Pi (verified). Printed verbatim when `pi` is
/// not on PATH so the user can copy-paste it. Kept as a constant so the CLI
/// wrapper and the `run_setup` core agree on the exact string.
pub const PI_INSTALL_HINT: &str = "npm install -g @earendil-works/pi-coding-agent";

/// The subdirectory name the extension is materialized into under Pi's
/// extensions dir. Pi auto-discovers `<extensions>/<name>/index.ts` (the
/// "subdirectory" layout documented for Pi 0.80.6), so materializing into this
/// dir is what *enables* the extension — there is no separate enable step.
pub const EXTENSION_DIR_NAME: &str = "dot-agent-deck";

/// The embedded extension files, as `(filename, contents)` pairs.
///
/// Only the two files Pi needs to load the extension are embedded:
///   - `index.ts`        — the Pi-API glue (default-export factory), and
///   - `orchestrator.ts` — the pure logic it imports via `./orchestrator.ts`.
///
/// A `package.json` is intentionally **not** embedded: the subdirectory
/// `index.ts` discovery layout does not require one, and the extension's only
/// runtime import (`typebox`) plus its type-only `@earendil-works/pi-coding-agent`
/// import are resolved from Pi's own installation when jiti loads the extension.
///
/// `include_str!` paths are relative to this source file (`src/`), so they
/// reference the real `pi-extension/` sources and stay in sync on rebuild.
pub const EXTENSION_FILES: &[(&str, &str)] = &[
    ("index.ts", include_str!("../pi-extension/src/index.ts")),
    (
        "orchestrator.ts",
        include_str!("../pi-extension/src/orchestrator.ts"),
    ),
];

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
}

/// The global Pi extensions dir the bundled extension is materialized into:
/// `~/.pi/agent/extensions/dot-agent-deck`. This is Pi's global auto-discovery
/// location, so anything written here is loaded by every `pi` session.
pub fn default_extension_dir() -> PathBuf {
    home_dir()
        .join(".pi")
        .join("agent")
        .join("extensions")
        .join(EXTENSION_DIR_NAME)
}

/// Materialize the bundled extension into `target_dir` in the layout Pi
/// discovers: `<target_dir>/index.ts` + `<target_dir>/orchestrator.ts`.
///
/// Creates `target_dir` (and parents) if needed and (over)writes each embedded
/// file, so re-running is idempotent and refreshes a stale copy. Returns the
/// list of paths written, in embed order. Hermetic — pass any directory
/// (a temp dir in tests, the real `~/.pi/...` in the CLI); it never reads the
/// environment.
pub fn materialize(target_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(target_dir)?;
    let mut written = Vec::with_capacity(EXTENSION_FILES.len());
    for (name, contents) in EXTENSION_FILES {
        let path = target_dir.join(name);
        std::fs::write(&path, contents)?;
        written.push(path);
    }
    Ok(written)
}

/// The outcome of the pure `orchestrator setup` core, independent of the real
/// PATH and real `~/.pi`. The thin CLI wrapper turns this into stdout/stderr +
/// an exit code.
#[derive(Debug)]
pub struct SetupReport {
    /// `true` → the extension was materialized (CLI exits zero); `false` → `pi`
    /// was absent and nothing was written (CLI exits non-zero).
    pub success: bool,
    /// Human-facing text: printed to stdout on success, stderr on failure. On
    /// the absent path it contains [`PI_INSTALL_HINT`] verbatim on its own line.
    pub message: String,
    /// Absolute paths materialized (empty when `pi` is absent).
    pub written: Vec<PathBuf>,
}

/// Pure decision core for `dot-agent-deck orchestrator setup`.
///
/// Takes an explicit `pi_present` flag and a `target_dir` so both branches are
/// testable without touching the real PATH or the real `~/.pi`. The CLI wrapper
/// supplies [`pi_on_path`] + [`default_extension_dir`].
///
///  - `pi` absent  → no filesystem writes; a failure report whose message names
///    the exact install command ([`PI_INSTALL_HINT`]).
///  - `pi` present → materialize the extension into `target_dir` and report
///    success, naming the files written.
pub fn run_setup(pi_present: bool, target_dir: &Path) -> std::io::Result<SetupReport> {
    if !pi_present {
        return Ok(SetupReport {
            success: false,
            message: format!(
                "pi is not installed or not on PATH — cannot enable the orchestrator extension.\n\
                 Install pi, then re-run `dot-agent-deck orchestrator setup`:\n\
                 {PI_INSTALL_HINT}"
            ),
            written: Vec::new(),
        });
    }

    let written = materialize(target_dir)?;
    let files = written
        .iter()
        .map(|p| format!("  {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(SetupReport {
        success: true,
        message: format!(
            "Enabled the dot-agent-deck orchestrator extension for Pi.\n\
             Materialized the bundled extension into {dir}:\n\
             {files}\n\
             Pi auto-discovers it there. Set `command = \"pi\"` for your orchestrator role \
             in .dot-agent-deck.toml to use it.",
            dir = target_dir.display()
        ),
        written,
    })
}

/// Whether a `pi` executable is discoverable on `PATH`. Used by the CLI wrapper
/// only; the pure [`run_setup`] core takes the boolean directly so tests never
/// depend on the machine's real PATH.
pub fn pi_on_path() -> bool {
    match std::env::var_os("PATH") {
        Some(path) => path_contains_binary(&path, "pi"),
        None => false,
    }
}

/// Scan a `PATH`-shaped value for a regular, *executable* file named `name`.
/// Pure over its `path` argument (no environment read) so it is testable without
/// mutating the process-global `PATH` — see the note on global-state races under
/// CI's shared `cargo test` process. Follows symlinks (Pi's launcher is
/// typically a symlink into its npm install) and requires a regular file, so a
/// directory of that name in a `PATH` entry does not count.
fn path_contains_binary(path: &std::ffi::OsStr, name: &str) -> bool {
    std::env::split_paths(path).any(|dir| is_executable_file(&dir.join(name)))
}

/// Whether `candidate` is a regular file that is *also* executable.
///
/// `is_file()` alone is not enough: a regular but non-executable file named `pi`
/// on `PATH` would make `orchestrator setup` falsely report success, only for
/// the Pi pane to fail to spawn later (Greptile #2). On Unix we additionally
/// require at least one exec bit (`mode & 0o111 != 0`); a non-executable
/// candidate is treated as "pi not usable", so the setup core takes the
/// not-present branch (prints the install hint, exits non-zero). On non-Unix
/// targets there is no cheap exec-bit check, so a regular file is accepted.
/// `metadata()` follows symlinks, matching `is_file()`'s symlink behavior.
fn is_executable_file(candidate: &Path) -> bool {
    if !candidate.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(candidate) {
            Ok(meta) => meta.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- M3.1 (test-plan row 11): materialize into a temp dir -------------

    /// Row 11: `materialize` writes exactly the embedded files into the target
    /// dir, byte-for-byte equal to the compiled-in strings.
    #[test]
    fn materialize_writes_embedded_files_to_target() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("dot-agent-deck");

        let written = materialize(&target).unwrap();

        // One path per embedded file, in embed order.
        assert_eq!(written.len(), EXTENSION_FILES.len());
        for (name, contents) in EXTENSION_FILES {
            let path = target.join(name);
            assert!(
                written.contains(&path),
                "expected {} in the written list",
                path.display()
            );
            let on_disk = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("expected materialized {}: {e}", path.display()));
            assert_eq!(
                &on_disk, contents,
                "{} must match the embedded source byte-for-byte",
                name
            );
        }
    }

    /// The materialized layout is exactly the two files Pi's subdirectory
    /// discovery needs: `index.ts` (the loaded entry point) and its
    /// `orchestrator.ts` sibling. Pins the layout the docs describe.
    #[test]
    fn materialize_layout_is_index_plus_orchestrator() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("ext");

        materialize(&target).unwrap();

        assert!(
            target.join("index.ts").is_file(),
            "index.ts is the entry point"
        );
        assert!(
            target.join("orchestrator.ts").is_file(),
            "orchestrator.ts is imported by index.ts via ./orchestrator.ts"
        );
        // The subdirectory layout needs no package.json.
        assert!(!target.join("package.json").exists());
    }

    /// The embedded `index.ts` really is Pi's default-export factory that
    /// imports the sibling `./orchestrator.ts` — so the two-file layout is
    /// self-consistent (guards against embedding the wrong sources).
    #[test]
    fn embedded_index_imports_sibling_orchestrator() {
        let index = EXTENSION_FILES
            .iter()
            .find(|(n, _)| *n == "index.ts")
            .map(|(_, c)| *c)
            .expect("index.ts must be embedded");
        assert!(index.contains("export default function"));
        assert!(index.contains("./orchestrator.ts"));
    }

    /// Re-running materialization overwrites in place (idempotent refresh),
    /// never erroring on an existing dir/file.
    #[test]
    fn materialize_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("ext");

        materialize(&target).unwrap();
        // Corrupt a file, then re-materialize: it must be restored.
        std::fs::write(target.join("index.ts"), "STALE").unwrap();
        let second = materialize(&target).unwrap();

        assert_eq!(second.len(), EXTENSION_FILES.len());
        let restored = std::fs::read_to_string(target.join("index.ts")).unwrap();
        assert_ne!(
            restored, "STALE",
            "re-materialize must overwrite a stale file"
        );
        assert!(restored.contains("export default function"));
    }

    // ---- M3.2 (test-plan row 12): setup core, both branches --------------

    /// Row 12 (a): `pi` PRESENT → materializes into the given temp dir and
    /// reports success (exit zero), naming the files it wrote.
    #[test]
    fn setup_present_materializes_and_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join(".pi/agent/extensions/dot-agent-deck");

        let report = run_setup(true, &target).unwrap();

        assert!(report.success, "present pi must succeed (exit zero)");
        assert_eq!(report.written.len(), EXTENSION_FILES.len());
        // The files really landed on disk in the discovery layout.
        assert!(target.join("index.ts").is_file());
        assert!(target.join("orchestrator.ts").is_file());
        // The success message names the target dir and the entry file.
        assert!(report.message.contains(&target.display().to_string()));
        assert!(report.message.contains("index.ts"));
        // No install hint on the success path.
        assert!(!report.message.contains(PI_INSTALL_HINT));
    }

    /// Row 12 (b): `pi` ABSENT → emits the exact install-hint command, writes
    /// nothing, and signals failure (exit non-zero).
    #[test]
    fn setup_absent_emits_exact_hint_and_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join(".pi/agent/extensions/dot-agent-deck");

        let report = run_setup(false, &target).unwrap();

        assert!(!report.success, "absent pi must fail (exit non-zero)");
        assert!(report.written.is_empty(), "absent pi must not materialize");
        // Nothing was written — the discovery dir does not exist.
        assert!(!target.exists(), "absent pi must not touch the target dir");
        // The exact install command appears verbatim on its own line.
        assert_eq!(
            PI_INSTALL_HINT,
            "npm install -g @earendil-works/pi-coding-agent"
        );
        assert!(
            report.message.lines().any(|l| l == PI_INSTALL_HINT),
            "message must contain the exact install hint on its own line; got:\n{}",
            report.message
        );
    }

    #[test]
    fn default_extension_dir_is_pi_global_layout() {
        // Pin the real target path shape without touching the real HOME's files.
        let dir = default_extension_dir();
        assert!(dir.ends_with("dot-agent-deck"));
        assert!(dir.to_string_lossy().contains(".pi/agent/extensions"));
    }

    #[test]
    fn path_contains_binary_finds_and_misses() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let faux = bin_dir.join("faux-pi");
        std::fs::write(&faux, b"#!/bin/sh\n").unwrap();
        set_mode(&faux, 0o755);
        // A directory that shares the name must NOT count as the binary.
        std::fs::create_dir_all(bin_dir.join("faux-dir")).unwrap();

        // Build a PATH-shaped value pointing only at our temp bin dir — no
        // process-global env mutation, so this is safe under shared-process runs.
        let path = std::env::join_paths([bin_dir.as_path()]).unwrap();

        assert!(
            path_contains_binary(&path, "faux-pi"),
            "a regular executable file on PATH must be detected"
        );
        assert!(
            !path_contains_binary(&path, "faux-dir"),
            "a directory sharing the name must not count"
        );
        assert!(
            !path_contains_binary(&path, "definitely-not-a-real-binary-xyz"),
            "an absent name must not be detected"
        );
    }

    /// Greptile #2 regression: a present-but-NON-EXECUTABLE `pi` on PATH must
    /// NOT be treated as usable. `path_contains_binary` rejects it (it is a
    /// regular file but has no exec bit), so the setup core takes the
    /// not-present branch and does NOT report success — otherwise setup would
    /// falsely succeed and the Pi pane would later fail to spawn.
    #[cfg(unix)]
    #[test]
    fn setup_non_executable_pi_does_not_succeed() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        // A regular file named `pi` with its exec bits explicitly cleared, so
        // the outcome is deterministic regardless of the process umask.
        let pi = bin_dir.join("pi");
        std::fs::write(&pi, b"#!/bin/sh\necho pi\n").unwrap();
        set_mode(&pi, 0o644);
        assert!(pi.is_file(), "the candidate is a regular file");

        let path = std::env::join_paths([bin_dir.as_path()]).unwrap();
        let pi_present = path_contains_binary(&path, "pi");
        assert!(
            !pi_present,
            "a non-executable `pi` must not count as present"
        );

        // Fed into the setup core, the not-present branch runs: no success, no
        // writes, and the target dir is untouched.
        let target = tmp.path().join(".pi/agent/extensions/dot-agent-deck");
        let report = run_setup(pi_present, &target).unwrap();
        assert!(
            !report.success,
            "a non-executable pi must not report setup success"
        );
        assert!(report.written.is_empty(), "nothing must be materialized");
        assert!(!target.exists(), "the target dir must not be created");
    }

    /// Set the permission bits on `path` (Unix only). Used by the PATH-scan
    /// tests to make a candidate executable or explicitly non-executable
    /// independent of the process umask.
    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(mode);
        std::fs::set_permissions(path, perms).unwrap();
    }

    /// No-op on non-Unix targets, where `is_executable_file` accepts any regular
    /// file and exec bits are not consulted.
    #[cfg(not(unix))]
    fn set_mode(_path: &Path, _mode: u32) {}
}
