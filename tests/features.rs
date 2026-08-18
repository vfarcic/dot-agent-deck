//! Pure-data unit tests for the experimental feature-flag plumbing (PRD
//! #139): `[features]` TOML parsing, the `DOT_AGENT_DECK_EXPERIMENTAL` env
//! override precedence, partial/invalid-TOML reload tolerance, and the
//! file-watch reload *apply path* (the same path `set_for_test` and the real
//! watcher use) updating the process-global shared `Features`.
//!
//! These are lib units, NOT `#[spec]` catalog tests — no scenario comments,
//! no catalog entries. The L1 widget snapshots and the L2 env-injection test
//! for the gated surface live in `tests/experimental_flag.rs` and
//! `tests/e2e_experimental_flag.rs` (tester-owned).

// Issue #322. Fast-tier, does NOT link `tests/common/mod.rs`; the ~40-line
// crate-internal resolver is `#[path]`-included instead, at two extra test
// executions rather than the harness's ~530. The two scratch dirs here hold a
// throwaway `.dot-agent-deck.toml`, so they are tiny — but a file with two
// conventions invites the wrong one, and rule 8 now covers all of `tests/`.
// This is the one file in the group that is not `#![cfg(unix)]`; the resolver's
// non-Unix arm falls back to the OS temp dir, which is what this call site did
// before. See `docs/develop/e2e-temp-dirs.md`.
#[path = "../src/test_temp.rs"]
mod test_temp;

use std::sync::Mutex;

use dot_agent_deck::config::{
    EXPERIMENTAL_ENV, features_config_path, load_features_file, parse_features, resolve_features,
    resolve_project_dir,
};
use dot_agent_deck::features::{self, Features};

/// Serializes the tests that mutate the process-global
/// `DOT_AGENT_DECK_EXPERIMENTAL` env var. Under `cargo nextest` each test is
/// its own process so this is belt-and-suspenders, but it keeps plain
/// `cargo test` (threads in one process) correct too.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard: set/clear `DOT_AGENT_DECK_EXPERIMENTAL` and restore the prior
/// value on drop, even on panic. The caller must hold `ENV_LOCK`.
struct ExperimentalEnvGuard {
    prev: Option<String>,
}

impl ExperimentalEnvGuard {
    fn set(value: Option<&str>) -> Self {
        let prev = std::env::var(EXPERIMENTAL_ENV).ok();
        // SAFETY: the caller holds ENV_LOCK for the guard's lifetime, which
        // serializes all access to this env var across the test binary.
        unsafe {
            match value {
                Some(v) => std::env::set_var(EXPERIMENTAL_ENV, v),
                None => std::env::remove_var(EXPERIMENTAL_ENV),
            }
        }
        Self { prev }
    }
}

impl Drop for ExperimentalEnvGuard {
    fn drop(&mut self) {
        // SAFETY: see ExperimentalEnvGuard::set — ENV_LOCK is held.
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var(EXPERIMENTAL_ENV, v),
                None => std::env::remove_var(EXPERIMENTAL_ENV),
            }
        }
    }
}

/// RAII guard for an arbitrary env var: set/clear it and restore the prior
/// value on drop, even on panic. Sibling of [`ExperimentalEnvGuard`] for env
/// vars other than `DOT_AGENT_DECK_EXPERIMENTAL`. The caller must hold
/// `ENV_LOCK`.
struct EnvVarGuard {
    key: &'static str,
    prev: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: Option<&str>) -> Self {
        let prev = std::env::var(key).ok();
        // SAFETY: the caller holds ENV_LOCK for the guard's lifetime, which
        // serializes all access to this env var across the test binary.
        unsafe {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        Self { key, prev }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: see EnvVarGuard::set — ENV_LOCK is held.
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

// (a) `[features]` TOML parse, including absent-table → default false.
#[test]
fn parse_features_table_and_absent_default() {
    // Absent table (and empty file) defaults to experimental = false.
    assert!(!parse_features("").unwrap().experimental);
    assert!(
        !parse_features("default_command = \"echo hi\"")
            .unwrap()
            .experimental,
        "a file with other keys but no [features] table defaults to OFF"
    );

    // Present and true / false.
    assert!(
        parse_features("[features]\nexperimental = true")
            .unwrap()
            .experimental
    );
    assert!(
        !parse_features("[features]\nexperimental = false")
            .unwrap()
            .experimental
    );

    // An empty [features] table also defaults the field to false.
    assert!(!parse_features("[features]").unwrap().experimental);

    // Malformed TOML is an error (so the reload path can keep the previous
    // value rather than silently flipping the flag).
    assert!(parse_features("[features]\nexperimental = ").is_err());
}

// (b) Env override precedence: env wins when set (case-insensitive
// `1`/`true` → ON, any other set value → OFF); unset falls back to the file.
#[test]
fn env_override_precedence() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let file_on = Features::test_with(true);
    let file_off = Features::test_with(false);

    // Unset → defer to the file value.
    {
        let _e = ExperimentalEnvGuard::set(None);
        assert!(
            resolve_features(file_on).experimental,
            "unset defers to file ON"
        );
        assert!(
            !resolve_features(file_off).experimental,
            "unset defers to file OFF"
        );
    }

    // Set to a truthy value → ON, overriding a file value of false (the
    // documented "env wins" case from the PRD validation strategy).
    for truthy in ["1", "true", "TRUE", "True", " true "] {
        let _e = ExperimentalEnvGuard::set(Some(truthy));
        assert!(
            resolve_features(file_off).experimental,
            "env {truthy:?} must force ON over file=false"
        );
    }

    // Set to a non-truthy value → OFF, overriding a file value of true
    // (env wins in both directions; file edits are ignored while set).
    for falsy in ["0", "false", "no", ""] {
        let _e = ExperimentalEnvGuard::set(Some(falsy));
        assert!(
            !resolve_features(file_on).experimental,
            "env {falsy:?} must force OFF over file=true"
        );
    }
}

// (c) Invalid/partial TOML on reload keeps the previous value (and logs a
// warning via `tracing::warn!`). A missing file resets to the default.
#[test]
fn invalid_toml_reload_keeps_previous() {
    let dir = test_temp::tempdir().unwrap();
    let path = dir.path().join(".dot-agent-deck.toml");
    let previous = Features::test_with(true);

    // Garbage / partial content → keep `previous`.
    std::fs::write(&path, "this is not valid toml = = =").unwrap();
    assert!(
        load_features_file(&path, previous).experimental,
        "a malformed file must keep the previous experimental=true"
    );

    // A valid file flips to its own value.
    std::fs::write(&path, "[features]\nexperimental = false").unwrap();
    assert!(
        !load_features_file(&path, previous).experimental,
        "a valid file with experimental=false overrides the previous true"
    );

    // A missing file is the default (OFF), regardless of `previous`.
    let missing = dir.path().join("does-not-exist.toml");
    assert!(
        !load_features_file(&missing, previous).experimental,
        "a missing file resets to the default (OFF)"
    );
}

// (d) The file-watch reload APPLY PATH updates the process-global shared
// `Features`. Models exactly what the watcher does on a config change:
// load_features_file → resolve_features → set_for_test (== the production
// `install`), with `show_experimental_footer()` reading the shared value.
#[test]
fn reload_apply_path_updates_shared_features() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Keep env unset so `resolve_features` defers to the file value.
    let _e = ExperimentalEnvGuard::set(None);

    let dir = test_temp::tempdir().unwrap();
    let path = dir.path().join(".dot-agent-deck.toml");

    // Start OFF and install via the same apply path the watcher uses.
    std::fs::write(&path, "[features]\nexperimental = false").unwrap();
    features::set_for_test(resolve_features(load_features_file(
        &path,
        features::current(),
    )));
    assert!(
        !features::show_experimental_footer(),
        "wrapper reports hidden while the file says experimental=false"
    );

    // Synthetic config change → flip ON; re-run the apply path. No restart.
    std::fs::write(&path, "[features]\nexperimental = true").unwrap();
    features::set_for_test(resolve_features(load_features_file(
        &path,
        features::current(),
    )));
    assert!(
        features::show_experimental_footer(),
        "wrapper re-evaluates to visible after the file flips experimental=true"
    );
}

// The `DOT_AGENT_DECK_FEATURES_CONFIG` override names the watched file
// outright, so it wins over the project directory the caller passes — tests
// (and an operator who really does want one specific file) never depend on
// any directory at all.
#[test]
fn features_config_path_honors_override() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // RAII guard: restores the prior value on drop even if the assertion
    // panics, so the override never leaks to other tests that read
    // `DOT_AGENT_DECK_FEATURES_CONFIG` without holding ENV_LOCK.
    let _config_env = EnvVarGuard::set(
        "DOT_AGENT_DECK_FEATURES_CONFIG",
        Some("/tmp/explicit/.dot-agent-deck.toml"),
    );
    assert_eq!(
        features_config_path(std::path::Path::new("/some/project")),
        std::path::PathBuf::from("/tmp/explicit/.dot-agent-deck.toml"),
        "the explicit-path override wins over the project directory"
    );
}

/// RAII guard for the process-global current directory: switch to `dir` and
/// restore the prior cwd on drop, even on panic. The caller must hold
/// `ENV_LOCK` — the cwd is process-global state exactly like an env var, so
/// it is serialized by the same mutex.
struct CwdGuard {
    prev: std::path::PathBuf,
}

impl CwdGuard {
    fn set(dir: &std::path::Path) -> Self {
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();
        Self { prev }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.prev).unwrap();
    }
}

// (f) Issue #577: the `[features]` table is read from the project directory
// the caller names, NOT from whatever directory the deck process happens to be
// running in. Before the fix `features_config_path` joined
// `std::env::current_dir()` unconditionally, so a deck launched anywhere but
// its project read the flag from the launch directory's `.dot-agent-deck.toml`
// — usually a file that does not exist — and every experimental surface
// silently resolved OFF, indistinguishable from the feature having been
// removed.
#[test]
fn features_config_path_resolves_against_project_dir_not_process_cwd() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Both overrides unset: this is about the DEFAULT resolution, so neither
    // `DOT_AGENT_DECK_EXPERIMENTAL` nor the explicit-path override may
    // participate.
    let _e = ExperimentalEnvGuard::set(None);
    let _o = EnvVarGuard::set("DOT_AGENT_DECK_FEATURES_CONFIG", None);

    // The project the deck is pointed at: experimental is ON here.
    let project = test_temp::tempdir().unwrap();
    let project_config = project.path().join(".dot-agent-deck.toml");
    std::fs::write(&project_config, "[features]\nexperimental = true").unwrap();

    // The directory the operator happened to launch from — a DIFFERENT
    // project, whose config says OFF. Distinct content rather than a missing
    // file, so a wrong read shows up as the wrong value and not merely as a
    // miss that the default would also produce.
    let launch = test_temp::tempdir().unwrap();
    std::fs::write(
        launch.path().join(".dot-agent-deck.toml"),
        "[features]\nexperimental = false",
    )
    .unwrap();
    let _cwd = CwdGuard::set(launch.path());

    assert_eq!(
        features_config_path(project.path()),
        project_config,
        "the watched file must be the named project's config, not the one in \
         the process's current directory"
    );

    // At the user's altitude: the flag the deck ends up with is the project's.
    let resolved = resolve_features(load_features_file(
        &features_config_path(project.path()),
        Features::default(),
    ));
    assert!(
        resolved.experimental,
        "a project whose .dot-agent-deck.toml says experimental = true must \
         resolve ON even though the deck was launched from a directory whose \
         own config says false"
    );

    // Control: the launch directory genuinely resolves the other way, so the
    // assertion above is attributable to WHICH directory was used rather than
    // to the loader ignoring the file or defaulting to ON.
    let from_launch_dir = resolve_features(load_features_file(
        &features_config_path(launch.path()),
        Features::default(),
    ));
    assert!(
        !from_launch_dir.experimental,
        "control: named explicitly, the launch directory still resolves to \
         its own experimental = false"
    );
}

// (g) Issue #577, the ancestor walk. `resolve_project_dir` answers "which
// project am I in?" the way project-scoped config is normally found: the
// nearest directory at or above the launch directory that holds a trusted
// `.dot-agent-deck.toml`. Before it existed, the deck keyed the flag to its
// own working directory, so running it from `repo/src` read a file that is
// not there and every experimental surface silently resolved OFF.
//
// These need no cwd and no env, so they hold no lock: every path is passed in.
#[test]
fn project_dir_walks_up_to_the_nearest_ancestor_config() {
    let root = test_temp::tempdir().unwrap();
    std::fs::write(
        root.path().join(".dot-agent-deck.toml"),
        "[features]\nexperimental = true",
    )
    .unwrap();
    let deep = root.path().join("crates").join("app").join("src");
    std::fs::create_dir_all(&deep).unwrap();

    assert_eq!(
        resolve_project_dir(&deep),
        root.path(),
        "a deck launched three levels below the project root must resolve to \
         the root that actually holds the config"
    );

    // ... and the flag that falls out of it is the project's.
    assert!(
        resolve_features(load_features_file(
            &features_config_path(&resolve_project_dir(&deep)),
            Features::default(),
        ))
        .experimental,
        "launched from a subdirectory, the deck must still see the project's \
         experimental = true"
    );
}

#[test]
fn project_dir_prefers_the_nearest_config_over_a_higher_one() {
    // An outer project containing an inner one — a workspace with a crate that
    // carries its own config, or a git worktree checked out inside a repo. The
    // inner config is the one you are working in, so it must win.
    let outer = test_temp::tempdir().unwrap();
    std::fs::write(
        outer.path().join(".dot-agent-deck.toml"),
        "[features]\nexperimental = true",
    )
    .unwrap();
    let inner = outer.path().join("inner");
    std::fs::create_dir_all(inner.join("src")).unwrap();
    std::fs::write(
        inner.join(".dot-agent-deck.toml"),
        "[features]\nexperimental = false",
    )
    .unwrap();

    assert_eq!(
        resolve_project_dir(&inner.join("src")),
        inner,
        "the nearest config wins over one further up"
    );
    // Load through it too: nearest-wins has to reach the VALUE, not just the
    // path, or the two configs' disagreement would be decided by the wrong one.
    assert!(
        !resolve_features(load_features_file(
            &features_config_path(&resolve_project_dir(&inner.join("src"))),
            Features::default(),
        ))
        .experimental,
        "the inner project's experimental = false must win over the outer \
         project's true"
    );
}

#[test]
fn project_dir_falls_back_to_the_start_directory_when_no_config_is_found() {
    // The pre-#577 behaviour is the fallback, so a deck launched outside any
    // project resolves exactly the path it always did and reads exactly the
    // file it always read.
    //
    // Assumes no ancestor of the scratch dir carries a `.dot-agent-deck.toml`
    // — true for the temp roots `test_temp` allocates, and a stray one there
    // would fail this loudly rather than silently.
    let dir = test_temp::tempdir().unwrap();
    let deep = dir.path().join("a").join("b");
    std::fs::create_dir_all(&deep).unwrap();

    assert_eq!(
        resolve_project_dir(&deep),
        deep,
        "with no config at or above it, the launch directory is the answer — \
         byte-identical to the pre-#577 path"
    );
}

#[test]
fn project_dir_skips_a_directory_whose_config_is_not_a_regular_file() {
    // A directory NAMED `.dot-agent-deck.toml` is not a config. The walk must
    // step over it and keep going rather than stopping there and reporting a
    // project root whose config can never load.
    let root = test_temp::tempdir().unwrap();
    std::fs::write(
        root.path().join(".dot-agent-deck.toml"),
        "[features]\nexperimental = true",
    )
    .unwrap();
    let decoy = root.path().join("decoy");
    std::fs::create_dir_all(decoy.join(".dot-agent-deck.toml")).unwrap();

    assert_eq!(
        resolve_project_dir(&decoy),
        root.path(),
        "a non-regular-file candidate is skipped and the walk continues up"
    );
}
