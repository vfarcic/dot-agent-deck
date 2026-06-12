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

use std::sync::Mutex;

use dot_agent_deck::config::{
    EXPERIMENTAL_ENV, features_config_path, load_features_file, parse_features, resolve_features,
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
    let dir = tempfile::tempdir().unwrap();
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

    let dir = tempfile::tempdir().unwrap();
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

// The `DOT_AGENT_DECK_FEATURES_CONFIG` override resolves the watched path so
// tests (and the watcher) never depend on the real cwd.
#[test]
fn features_config_path_honors_override() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("DOT_AGENT_DECK_FEATURES_CONFIG").ok();
    // SAFETY: ENV_LOCK held for the duration of this test.
    unsafe {
        std::env::set_var(
            "DOT_AGENT_DECK_FEATURES_CONFIG",
            "/tmp/explicit/.dot-agent-deck.toml",
        );
    }
    assert_eq!(
        features_config_path(),
        std::path::PathBuf::from("/tmp/explicit/.dot-agent-deck.toml")
    );
    // SAFETY: ENV_LOCK held; restore the prior value.
    unsafe {
        match prev {
            Some(v) => std::env::set_var("DOT_AGENT_DECK_FEATURES_CONFIG", v),
            None => std::env::remove_var("DOT_AGENT_DECK_FEATURES_CONFIG"),
        }
    }
}
