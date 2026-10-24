#![cfg(unix)]

//! A deck process armed as a test — by the containment marker, or by nextest's
//! own environment pair — does not open a `DOT_AGENT_DECK_LOG` outside its
//! owned roots (issues #1791, #1796).
//!
//! A test run started from a shell that carries `DOT_AGENT_DECK_LOG` — a
//! developer's, or the deck pane's the tests were started in — passed it to
//! every deck binary the tests spawned without clearing the environment, and
//! those binaries appended their lines to the real `deck.log`, where they read
//! as the production daemon's own activity. The lines measured there were the
//! wrapper's automatic hook install and its containment refusals, so these
//! tests drive exactly that: the real `wrap` subcommand, whose Codex install
//! is refused and logged because `CODEX_HOME` is outside the owned root.
//!
//! Fast tier, and does NOT link `tests/common/mod.rs`: like
//! `tests/codex_hooks_safety.rs`, it spawns cargo's build of the binary with a
//! cleared environment and needs nothing else from the harness.
#[path = "../src/test_temp.rs"]
mod test_temp;

use spec::spec;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The line the refused install writes, which is what reached the real log.
const REFUSAL: &str = "agent config write refused by test containment";

/// How the wrapper learns it is a test process.
enum Armed<'a> {
    /// The containment marker and an explicit owned root, which is what the
    /// e2e harness pins into every environment it builds.
    Explicitly { owned_root: &'a Path },
    /// Only the pair nextest sets for every test it runs, which is what a
    /// binary spawned without a cleared environment inherits.
    ByNextest,
}

/// Run the real wrapper around `/bin/true` with its Codex home at `codex_home`
/// and `DOT_AGENT_DECK_LOG` at `log`.
fn wrap(fixture: &Path, codex_home: &Path, log: &Path, armed: Armed<'_>) {
    let home = fixture.join("home");
    // A durable install for the hook command to name, so the install gets as
    // far as the guarded write instead of refusing cargo's build artifact.
    let durable = home.join(".local/bin/dot-agent-deck");
    std::fs::create_dir_all(durable.parent().unwrap()).unwrap();
    std::fs::write(&durable, b"#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&durable, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir_all(child_tmp(fixture)).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", child_tmp(fixture))
        .env("HOME", &home)
        .env("CODEX_HOME", codex_home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("PI_CODING_AGENT_DIR", home.join(".pi/agent"))
        .env("DOT_AGENT_DECK_PANE_ID", "log-containment-worker")
        .env(
            "DOT_AGENT_DECK_SOCKET",
            fixture.join("no-hook-listener.sock"),
        )
        .env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            fixture.join("no-attach-listener.sock"),
        )
        .env("DOT_AGENT_DECK_STATE_DIR", home.join("state"))
        .env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "20")
        .env("DOT_AGENT_DECK_LOG", log)
        .current_dir(fixture)
        .args(["wrap", "--agent", "codex", "--", "/bin/true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match armed {
        Armed::Explicitly { owned_root } => {
            command
                .env("DOT_AGENT_DECK_TEST_CONFIG_WRITE", "1")
                .env("DOT_AGENT_DECK_TEST_CONFIG_ROOT", owned_root);
        }
        Armed::ByNextest => {
            command
                .env("NEXTEST", "1")
                .env("NEXTEST_RUN_ID", "deck-log-containment");
        }
    }
    command.status().expect("run the wrapper");
}

/// The temp dir the child is handed: its own, so the system temp dir — where
/// cargo's target tmp dir can live — is not one of a nextest-armed child's
/// default roots.
fn child_tmp(fixture: &Path) -> PathBuf {
    fixture.join("tmp")
}

/// The positive control both tests share: with the log inside the owned root,
/// the same run writes the refusal there, so an absent outside log means the
/// logger refused the path and not that the run had nothing to say.
fn assert_logged_inside(log: &Path) {
    let body = std::fs::read_to_string(log).unwrap_or_else(|e| {
        panic!(
            "a log inside the owned root must be opened ({}): {e}",
            log.display()
        )
    });
    assert!(
        body.contains(REFUSAL),
        "the wrapper must log its refused install into a contained log:\n{body}"
    );
}

/// Scenario: Start the real Codex wrapper as the e2e harness does, armed with an explicit owned root, while `DOT_AGENT_DECK_LOG` names a file in a separate directory standing in for the developer's state dir. The wrapper refuses its out-of-root install and must not create that log; with the log inside the owned root, the same refusal is written there.
#[spec("logging/containment/001")]
#[test]
fn logging_containment_001_explicitly_armed_wrapper_never_opens_an_outside_log() {
    let fixture = test_temp::tempdir().unwrap();
    let developer_state = test_temp::tempdir().unwrap();
    let outside_codex = test_temp::tempdir().unwrap();
    let real_log = developer_state.path().join("deck.log");

    wrap(
        fixture.path(),
        outside_codex.path(),
        &real_log,
        Armed::Explicitly {
            owned_root: fixture.path(),
        },
    );
    assert!(
        !real_log.exists(),
        "a test process opened a deck.log outside its owned root: {}\n{}",
        real_log.display(),
        std::fs::read_to_string(&real_log).unwrap_or_default()
    );

    let contained = fixture.path().join("deck.log");
    wrap(
        fixture.path(),
        outside_codex.path(),
        &contained,
        Armed::Explicitly {
            owned_root: fixture.path(),
        },
    );
    assert_logged_inside(&contained);
}

/// Scenario: Start the real Codex wrapper with only nextest's own environment pair, the way a binary a test spawned without clearing its environment inherits it, while `DOT_AGENT_DECK_LOG` names a file outside every directory tests make scratch space in. The wrapper must not create that log; with the log in a test scratch directory, its refused install is written there.
#[spec("logging/containment/002")]
#[test]
fn logging_containment_002_nextest_armed_wrapper_never_opens_an_outside_log() {
    let fixture = test_temp::tempdir().unwrap();
    // Cargo creates this directory when it builds, but a test run from a
    // nextest archive (CI's e2e shards) extracts `target/` without it, so it
    // is created here; the path itself is the compile-time one either way.
    std::fs::create_dir_all(env!("CARGO_TARGET_TMPDIR"))
        .expect("create cargo's per-target scratch dir");
    let outside = tempfile::Builder::new()
        .prefix("deck-log-containment-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let outside_path = std::fs::canonicalize(outside.path()).unwrap();
    for root in scratch_roots() {
        assert!(
            !outside_path.starts_with(&root),
            "PRECONDITION NOT MET: cargo's target tmp dir {} is under the test scratch root {}, \
             so it cannot stand in for a log outside them",
            outside_path.display(),
            root.display()
        );
    }
    let real_log = outside_path.join("deck.log");
    let outside_codex = outside_path.join("codex");

    wrap(fixture.path(), &outside_codex, &real_log, Armed::ByNextest);
    assert!(
        !real_log.exists(),
        "a nextest-armed process opened a deck.log outside every test root: {}\n{}",
        real_log.display(),
        std::fs::read_to_string(&real_log).unwrap_or_default()
    );

    let contained = child_tmp(fixture.path()).join("deck.log");
    wrap(fixture.path(), &outside_codex, &contained, Armed::ByNextest);
    assert_logged_inside(&contained);
}

/// The default roots a nextest-armed child shares with this process,
/// canonicalized: the private base, and a `DAD_E2E_TMPDIR` set here (which the
/// child's cleared environment drops, so it only makes the precondition
/// stricter). Not this process's temp dir: the child is handed its own,
/// [`child_tmp`].
fn scratch_roots() -> Vec<PathBuf> {
    let own_tmp = std::env::temp_dir();
    dot_agent_deck::config_write_guard::default_test_roots()
        .into_iter()
        .filter(|root| *root != own_tmp)
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect()
}
