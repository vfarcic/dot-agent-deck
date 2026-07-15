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

/// Pi's own env var (`PI_CODING_AGENT_DIR`) that relocates its agent directory —
/// the dir that holds `extensions/`. Pi resolves it in `getAgentDir()`:
/// `$PI_CODING_AGENT_DIR` (tilde-expanded) if set, else `~/.pi/agent`.
/// dot-agent-deck mirrors that EXACT precedence so the bundled extension is
/// always materialized where pi will actually look for it:
///   - **production correctness** — a user who relocates pi via this var still
///     gets a status-tracked pane (materializing into `~/.pi` would miss); and
///   - **test isolation** — tests set it to a throwaway dir, so the suite never
///     writes the developer's real `~/.pi`.
pub const ENV_PI_AGENT_DIR: &str = "PI_CODING_AGENT_DIR";

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

/// Pi's agent directory under a given HOME: `<home>/.pi/agent` (the default
/// branch of pi's `getAgentDir()`, used when [`ENV_PI_AGENT_DIR`] is unset).
fn agent_dir_under_home(home: &Path) -> PathBuf {
    home.join(".pi").join("agent")
}

/// The `extensions/dot-agent-deck` subpath pi discovers inside an agent dir. The
/// single place the `<agent-dir>/extensions/<name>` layout is encoded, so the
/// HOME-based and `PI_CODING_AGENT_DIR`-based paths can't diverge.
fn extension_dir_in(agent_dir: &Path) -> PathBuf {
    agent_dir.join("extensions").join(EXTENSION_DIR_NAME)
}

/// Build the extension target dir under a given HOME:
/// `<home>/.pi/agent/extensions/dot-agent-deck`. The `PI_CODING_AGENT_DIR`-unset
/// layout; kept for the HOME-based resolvers and tests.
pub fn extension_dir_under(home: &Path) -> PathBuf {
    extension_dir_in(&agent_dir_under_home(home))
}

/// Expand a leading `~` / `~/` in a `PI_CODING_AGENT_DIR` value against `home`,
/// matching pi's `expandTildePath`. A bare `~` becomes `home`; `~/x` becomes
/// `home/x`; everything else (absolute or relative) passes through unchanged.
/// With no HOME to expand against, a `~`-prefixed value is returned verbatim
/// (pi would do the same fallback).
fn expand_tilde(value: &str, home: Option<&str>) -> PathBuf {
    match (value, home) {
        ("~", Some(h)) => PathBuf::from(h),
        (v, Some(h)) if v.starts_with("~/") => Path::new(h).join(&v[2..]),
        _ => PathBuf::from(value),
    }
}

/// Pure core mirroring pi's `getAgentDir()`: `$PI_CODING_AGENT_DIR`
/// (tilde-expanded against `home`) if set & non-empty, else `<home>/.pi/agent`.
/// `None` only when NEITHER the override nor `home` yields a non-empty value.
/// Pure over its inputs so every branch is testable without mutating
/// process-global env.
fn resolve_agent_dir_inner(
    agent_dir_override: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    if let Some(dir) = agent_dir_override.filter(|v| !v.is_empty()) {
        return Some(expand_tilde(dir, home));
    }
    home.filter(|v| !v.is_empty())
        .map(|h| agent_dir_under_home(Path::new(h)))
}

/// The Pi agent dir the EXPLICIT `orchestrator setup` path targets, resolved from
/// the process env: `$PI_CODING_AGENT_DIR` if set & non-empty, else
/// `<HOME>/.pi/agent`. `None` only when neither yields a non-empty value — the
/// CLI then errors rather than guess a location pi will never load.
fn agent_dir_strict() -> Option<PathBuf> {
    resolve_agent_dir_inner(
        std::env::var(ENV_PI_AGENT_DIR).ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// The Pi extensions dir the bundled extension is materialized into by the
/// EXPLICIT `orchestrator setup` path: `<agent-dir>/extensions/dot-agent-deck`,
/// where the agent dir is `$PI_CODING_AGENT_DIR` if set else `~/.pi/agent`.
///
/// Returns `None` when neither `PI_CODING_AGENT_DIR` nor HOME is set/non-empty:
/// the CLI must NOT guess a `/tmp` (unset) or relative (`./`, empty) location Pi
/// will never load — it errors instead. Matches the auto path's SKIP
/// (see [`auto_materialize`]); both refuse to guess.
pub fn default_extension_dir() -> Option<PathBuf> {
    agent_dir_strict().map(|dir| extension_dir_in(&dir))
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

// ---------------------------------------------------------------------------
// PRD #201: daemon-startup auto-materialize
// ---------------------------------------------------------------------------
//
// Pi should need NO manual setup — parity with claude (hooks auto-install,
// `hooks_manage::auto_install`) and opencode (plugin auto-install,
// `opencode_manage::auto_install`), which install ONCE at startup. The bundled
// extension is materialized once when the daemon starts serving (the
// `dot-agent-deck daemon serve` entry, which both the lazy-spawned daemon and a
// headless serve go through), NOT on every agent spawn.
//
// Why not per-spawn: the deck must not care HOW pi is launched, so we cannot key
// off the spawn command's basename (a wrapper like `devbox run pi-big` would be
// missed). Materializing per-spawn regardless of command instead meant every
// unrelated agent start (claude, a shell, a fast test) rewrote `~/.pi` whenever
// pi happened to be on PATH. Doing it once at daemon startup is command-agnostic
// AND touches Pi's dir only once per daemon.
//
// Location mirrors pi's own `getAgentDir()`: `$PI_CODING_AGENT_DIR` if set, else
// `<HOME>/.pi/agent`, + `/extensions/dot-agent-deck` (see [`ENV_PI_AGENT_DIR`]).
// Guarded (only when pi is present), idempotent (overwrite), and unset-safe
// (SKIP, never a `/tmp` guess).

/// Look up `key` in the env overlay first (the value that wins in the child's
/// environment), then the process env. `None` if absent in both.
fn lookup_env(env: &[(String, String)], key: &str) -> Option<String> {
    env.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var(key).ok())
}

/// Resolve the Pi agent dir the daemon should materialize the extension into,
/// mirroring pi's `getAgentDir()`: `$PI_CODING_AGENT_DIR` (tilde-expanded) if set
/// & non-empty, else `<HOME>/.pi/agent`. `None` only when neither the override
/// nor HOME yields a non-empty value (then auto-materialize SKIPs rather than
/// guess). Consults the env overlay before the process env for both keys so a
/// daemon/test that overrides them is judged on the effective environment.
fn resolve_agent_dir_from_env(env: &[(String, String)]) -> Option<PathBuf> {
    resolve_agent_dir_inner(
        lookup_env(env, ENV_PI_AGENT_DIR).as_deref(),
        lookup_env(env, "HOME").as_deref(),
    )
}

/// Whether `pi` is discoverable on the PATH the spawned child will actually
/// use: a `PATH` entry in the spawn env overlay if present, else the process
/// `PATH`. The seam resolves pi-presence from the child's effective env (not
/// merely the daemon's own PATH) so a caller that overrides `PATH` in
/// `opts.env` — as the real-pi e2e tests do — is judged against the same PATH
/// the child inherits.
fn pi_present_for_env(env: &[(String, String)]) -> bool {
    let path = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| std::ffi::OsString::from(v))
        .or_else(|| std::env::var_os("PATH"));
    match path {
        Some(p) => path_contains_binary(&p, "pi"),
        None => false,
    }
}

/// Pure decision core for the spawn-time auto-materialize (mirrors
/// [`run_setup`]'s testable shape). Given whether pi is present and the
/// resolved target dir (`None` = HOME unset), returns the paths written, or
/// `None` when it SKIPPED. No env / PATH reads, so the fast-tier test exercises
/// every branch — present/absent, HOME-set/unset, idempotent re-run — without
/// mutating process-global state.
///
///  - `!pi_present`        → SKIP (pi absent — nothing to enable).
///  - `target_dir == None` → SKIP (HOME unset — do NOT write to a `/tmp` guess).
///  - otherwise            → (over)write the bundled files into `target_dir`
///    ([`materialize`] is idempotent), returning the paths written.
pub fn auto_materialize_core(
    pi_present: bool,
    target_dir: Option<&Path>,
) -> std::io::Result<Option<Vec<PathBuf>>> {
    if !pi_present {
        return Ok(None);
    }
    match target_dir {
        Some(target) => Ok(Some(materialize(target)?)),
        None => Ok(None),
    }
}

/// Daemon-startup entry: silently (over)materialize the bundled Pi orchestrator
/// extension so a pi agent needs ZERO manual setup. Called ONCE from the
/// `dot-agent-deck daemon serve` entry (covering both the lazy-spawned daemon and
/// a headless serve) — NOT per spawn and NOT gated on any command, so it works
/// whether pi is launched as `pi`, an absolute path, or a wrapper (`devbox run …`,
/// `run_agent.sh`). `env` is an optional overlay consulted before the process env
/// for `PI_CODING_AGENT_DIR` / `HOME` / `PATH`; the daemon passes `&[]` to use its
/// own environment. The self-guard is pi-presence ([`pi_present_for_env`]), so a
/// daemon on a machine without pi is a cheap no-op. Idempotent (overwrite) and
/// unset-safe (SKIP, no `/tmp` write). Best-effort — a write failure is logged,
/// never fatal (matching `hooks_manage::auto_install`).
pub fn auto_materialize(env: &[(String, String)]) {
    let target = resolve_agent_dir_from_env(env).map(|dir| extension_dir_in(&dir));
    match auto_materialize_core(pi_present_for_env(env), target.as_deref()) {
        Ok(Some(written)) => tracing::info!(
            count = written.len(),
            "auto-materialized the Pi orchestrator extension into the Pi agent dir"
        ),
        Ok(None) => tracing::debug!(
            "auto-materialize: skipped the Pi extension (pi absent, or PI_CODING_AGENT_DIR/HOME unset)"
        ),
        Err(e) => {
            tracing::warn!("auto-materialize: failed to write the Pi orchestrator extension: {e}")
        }
    }
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

    // ---- PRD #201: spawn-time auto-materialize --------------------------

    /// The auto path materializes into a CLEAN target HOME when pi is present:
    /// starting from a directory with NO pre-existing extension, the two
    /// bundled files land in the Pi discovery layout, byte-for-byte equal to
    /// the compiled-in strings. The clean start is essential — otherwise a
    /// materialize would be an indistinguishable no-op.
    #[test]
    fn auto_materialize_core_materializes_into_clean_home_when_pi_present() {
        let tmp = tempfile::tempdir().unwrap();
        let target = extension_dir_under(tmp.path());
        // CLEAN: nothing there yet.
        assert!(!target.exists(), "target must start without the extension");

        let written = auto_materialize_core(true, Some(&target))
            .unwrap()
            .expect("pi present + HOME resolved must materialize");

        assert_eq!(written.len(), EXTENSION_FILES.len());
        assert!(target.join("index.ts").is_file());
        assert!(target.join("orchestrator.ts").is_file());
        for (name, contents) in EXTENSION_FILES {
            let on_disk = std::fs::read_to_string(target.join(name)).unwrap();
            assert_eq!(&on_disk, contents, "{name} must match the embedded source");
        }
    }

    /// pi ABSENT → SKIP: nothing is written and the target dir is never even
    /// created (the guard mirrors `hooks_manage::auto_install`'s
    /// "only if the agent is detected").
    #[test]
    fn auto_materialize_core_skips_when_pi_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let target = extension_dir_under(tmp.path());

        let result = auto_materialize_core(false, Some(&target)).unwrap();

        assert!(result.is_none(), "pi absent must SKIP (return None)");
        assert!(!target.exists(), "pi absent must not create the target dir");
    }

    /// HOME unset (`target_dir == None`) → SKIP with NO write anywhere — in
    /// particular NOT to a predictable `/tmp` path. This is the auto-path
    /// answer to Greptile's `orchestrator_ext.rs:49` `/tmp` fallback concern:
    /// the resolver returns `None` (never a `/tmp` guess) and the core skips.
    #[test]
    fn auto_materialize_core_skips_and_writes_nothing_when_home_unset() {
        // pi is "present", but the target could not be resolved (HOME unset), so
        // the core takes NO `target` and therefore cannot write anywhere — in
        // particular it never falls back to a `/tmp` path. The companion
        // `resolve_agent_dir_inner_override_and_home_precedence` proves the
        // resolver yields `None` (not a `/tmp` guess) when neither is set.
        let result = auto_materialize_core(true, None).unwrap();
        assert!(result.is_none(), "HOME unset must SKIP (return None)");
    }

    /// Re-running the auto path over an already-materialized (or stale) target
    /// overwrites in place — idempotent refresh, never erroring on an existing
    /// dir/file.
    #[test]
    fn auto_materialize_core_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let target = extension_dir_under(tmp.path());

        auto_materialize_core(true, Some(&target)).unwrap();
        // Corrupt a materialized file, then re-run: it must be restored.
        std::fs::write(target.join("index.ts"), "STALE").unwrap();
        let second = auto_materialize_core(true, Some(&target))
            .unwrap()
            .expect("second run must materialize");

        assert_eq!(second.len(), EXTENSION_FILES.len());
        let restored = std::fs::read_to_string(target.join("index.ts")).unwrap();
        assert_ne!(restored, "STALE", "re-run must overwrite a stale file");
        assert!(restored.contains("export default function"));
    }

    /// The pure agent-dir core mirrors pi's `getAgentDir()`: the
    /// `PI_CODING_AGENT_DIR` override wins over HOME (tilde-expanded against it);
    /// with no override it is `<HOME>/.pi/agent`; with neither it is `None`
    /// (never a `/tmp` guess); empty values are ignored on both.
    #[test]
    fn resolve_agent_dir_inner_override_and_home_precedence() {
        // Override wins over HOME (absolute passes through).
        assert_eq!(
            resolve_agent_dir_inner(Some("/custom/agent"), Some("/home/alice")),
            Some(PathBuf::from("/custom/agent"))
        );
        // Override is tilde-expanded against HOME.
        assert_eq!(
            resolve_agent_dir_inner(Some("~/pi-agent"), Some("/home/alice")),
            Some(PathBuf::from("/home/alice/pi-agent"))
        );
        // No override → <HOME>/.pi/agent.
        assert_eq!(
            resolve_agent_dir_inner(None, Some("/home/alice")),
            Some(PathBuf::from("/home/alice/.pi/agent"))
        );
        // Neither set → None (the SKIP path; NO `/tmp` fallback).
        assert_eq!(resolve_agent_dir_inner(None, None), None);
        // Empty override is ignored → falls back to <HOME>/.pi/agent.
        assert_eq!(
            resolve_agent_dir_inner(Some(""), Some("/home/alice")),
            Some(PathBuf::from("/home/alice/.pi/agent"))
        );
        // Empty HOME with no override → None.
        assert_eq!(resolve_agent_dir_inner(None, Some("")), None);
    }

    /// `expand_tilde` expands ONLY a leading `~` / `~/` against HOME (matching
    /// pi's `expandTildePath`); absolute and relative paths, a mid-path `~`, and
    /// (with no HOME) a `~`-prefixed value all pass through unchanged.
    #[test]
    fn expand_tilde_expands_leading_home_only() {
        assert_eq!(expand_tilde("~", Some("/h")), PathBuf::from("/h"));
        assert_eq!(expand_tilde("~/a/b", Some("/h")), PathBuf::from("/h/a/b"));
        assert_eq!(expand_tilde("/abs/x", Some("/h")), PathBuf::from("/abs/x"));
        assert_eq!(expand_tilde("rel/x", Some("/h")), PathBuf::from("rel/x"));
        assert_eq!(expand_tilde("/x/~/y", Some("/h")), PathBuf::from("/x/~/y"));
        // No HOME → a `~` value is returned verbatim (pi's fallback).
        assert_eq!(expand_tilde("~/a", None), PathBuf::from("~/a"));
    }

    /// The env-based resolver honors `PI_CODING_AGENT_DIR` from the overlay (the
    /// value the child gets), winning over HOME, and maps to
    /// `<agent-dir>/extensions/dot-agent-deck`. Hermetic: the override is in the
    /// overlay, so the process env is never consulted.
    #[test]
    fn resolve_agent_dir_from_env_honors_pi_agent_dir_override() {
        let overlay = vec![
            (ENV_PI_AGENT_DIR.to_string(), "/iso/agent".to_string()),
            ("HOME".to_string(), "/should/not/win".to_string()),
        ];
        let dir = resolve_agent_dir_from_env(&overlay).expect("override yields a dir");
        assert_eq!(dir, PathBuf::from("/iso/agent"));
        assert_eq!(
            extension_dir_in(&dir),
            PathBuf::from("/iso/agent/extensions/dot-agent-deck")
        );
    }

    /// `pi_present_for_env` judges pi-presence against the PATH override in the
    /// spawn env overlay when present (so a caller — like the real-pi e2e
    /// tests — that puts pi on a PATH the daemon process itself lacks is
    /// detected correctly).
    #[cfg(unix)]
    #[test]
    fn pi_present_for_env_honors_env_path_override() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let pi = bin_dir.join("pi");
        std::fs::write(&pi, b"#!/bin/sh\n").unwrap();
        set_mode(&pi, 0o755);

        // An overlay PATH pointing only at our temp bin dir detects pi even
        // though the process PATH almost certainly does not contain THIS pi.
        let overlay = vec![("PATH".to_string(), bin_dir.to_string_lossy().into_owned())];
        assert!(pi_present_for_env(&overlay));

        // An overlay PATH pointing at an empty dir does NOT detect pi.
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let overlay_empty = vec![("PATH".to_string(), empty.to_string_lossy().into_owned())];
        assert!(!pi_present_for_env(&overlay_empty));
    }

    #[test]
    fn default_extension_dir_is_pi_global_layout() {
        // Pin the real target path shape without touching the real HOME's files.
        // With HOME set (the normal test env) the strict resolver yields the Pi
        // global layout; if HOME were unset it would refuse (`None`), which the
        // dedicated refusal test below covers.
        let Some(dir) = default_extension_dir() else {
            return;
        };
        assert!(dir.ends_with("dot-agent-deck"));
        assert!(dir.to_string_lossy().contains(".pi/agent/extensions"));
    }

    /// The EXPLICIT setup path refuses (yields no target dir) when NEITHER
    /// `PI_CODING_AGENT_DIR` nor HOME is set/non-empty — it never guesses a `/tmp`
    /// (unset) or relative `./` (empty) location Pi would never load. Exercised
    /// through the pure [`resolve_agent_dir_inner`] core (which `agent_dir_strict`
    /// feeds the process env) so it needs no process-global env mutation. With no
    /// dir resolved, [`materialize`] is never reached and nothing is written; the
    /// CLI turns the `None` into a non-zero error (see `src/main.rs`).
    #[test]
    fn explicit_setup_refuses_when_agent_dir_and_home_unset_or_empty() {
        // Neither override nor HOME → None (NO `/tmp` fallback).
        assert_eq!(resolve_agent_dir_inner(None, None), None);
        // Empty HOME, no override → None (NO relative `./` base).
        assert_eq!(resolve_agent_dir_inner(None, Some("")), None);
        // HOME set, no override → the extension dir under `<HOME>/.pi/agent`.
        assert_eq!(
            resolve_agent_dir_inner(None, Some("/home/alice")).map(|d| extension_dir_in(&d)),
            Some(extension_dir_under(Path::new("/home/alice")))
        );
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
