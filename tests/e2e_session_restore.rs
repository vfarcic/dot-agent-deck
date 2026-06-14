#![cfg(feature = "e2e")]

//! PRD #89 Phase 2 — L2 (real-binary PTY) coverage for *auto-restore on
//! startup*.
//!
//! Phase 1 made the saved-session snapshot continuously fresh; Phase 2 makes
//! restoring it UNCONDITIONAL on every TUI startup — no `--continue` flag.
//! Precedence: try daemon hydration first; if hydration produced any panes the
//! daemon state wins and snapshot restore is skipped; if hydration produced
//! zero panes (fresh daemon / crash recovery), load and apply the disk
//! snapshot; if both are empty, land at an empty dashboard.
//!
//! These tests drive the REAL binary through a PTY with `DOT_AGENT_DECK_SESSION`
//! redirected to a test-owned path. No LLM tokens are spent — restored/spawned
//! panes run `sleep 600` (Agent: none).
//!
//! Decision 6: gated behind the `e2e` feature so `cargo test-fast` never
//! compiles it.

mod common;

use std::path::Path;
use std::time::Duration;

use common::TuiDeck;
use spec::spec;

/// Stage a saved-session `session.toml` at `session_file` describing each
/// `(name, command)` pane, all rooted at `dir` (which must already exist on
/// disk so the restore path's dir-exists check does not skip them). Hand-rolled
/// TOML mirroring `dot_agent_deck::config::SavedPane` — the multi-pane analogue
/// of the harness's private `write_continue_session_file`, but usable WITHOUT
/// `--continue` (we write only the file; the launch passes no flag).
fn stage_session_snapshot(session_file: &Path, dir: &Path, panes: &[(&str, &str)]) {
    let dir = dir.to_str().expect("snapshot dir is UTF-8");
    let mut s = String::new();
    for (name, command) in panes {
        s.push_str("[[panes]]\n");
        s.push_str(&format!("dir = \"{}\"\n", toml_basic_escape(dir)));
        s.push_str(&format!("name = \"{}\"\n", toml_basic_escape(name)));
        s.push_str(&format!("command = \"{}\"\n\n", toml_basic_escape(command)));
    }
    std::fs::write(session_file, s).expect("write staged session.toml");
}

/// Minimal TOML basic-string escape for the values we embed (filesystem paths
/// and short ASCII names) — backslash and double-quote only, which is all a
/// Linux tempdir path or a `restored-*` name can contain here.
fn toml_basic_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Scenario: Stage a `session.toml` describing two dashboard panes
/// (`restored-alpha`, `restored-beta`, both `sleep 600`) at the path
/// `DOT_AGENT_DECK_SESSION` points to, then launch the deck against a fresh
/// (empty) daemon with NO `--continue` flag. Auto-restore must recreate both
/// saved panes as dashboard cards without any flag. RED today: the snapshot
/// load is gated behind `if continue_session` in `run_tui`, so with no flag the
/// block never runs and neither saved pane appears — the dashboard stays at
/// "No active sessions".
#[spec("session/restore/001")]
#[test]
fn restore_001_no_flag_startup_restores_panes_from_snapshot() {
    // A test-owned snapshot dir the deck's `session_path()` reads via
    // `DOT_AGENT_DECK_SESSION`. It also doubles as the restored panes' working
    // directory — it exists on disk, so the restore path's `dir.is_dir()` guard
    // keeps both panes (rather than skipping them as missing-dir).
    let session_dir = common::race_safe_tempdir();
    let session_file = session_dir.path().join("session.toml");
    stage_session_snapshot(
        &session_file,
        session_dir.path(),
        &[
            ("restored-alpha", "sleep 600"),
            ("restored-beta", "sleep 600"),
        ],
    );

    // No `--continue` — `launch_with_fixture` only passes the flag when a
    // `with_continue_session(...)` was staged, which it was not. The daemon
    // this deck lazy-spawns is brand new (empty), so hydration yields nothing
    // and the disk snapshot is the only possible source of panes.
    let deck = TuiDeck::builder()
        .with_env(
            "DOT_AGENT_DECK_SESSION",
            session_file.to_str().expect("session path is UTF-8"),
        )
        .launch_with_fixture("modes");

    // After Phase 2, both saved panes auto-restore as dashboard cards. Their
    // saved names appear in the card title rows (e.g. "1 restored-alpha").
    let restored = common::wait_until(Duration::from_secs(10), || {
        let grid = deck.snapshot_grid();
        grid.contains("restored-alpha") && grid.contains("restored-beta")
    });
    assert!(
        restored,
        "PRD #89 M2.1: launching with NO --continue and a 2-pane snapshot on disk must \
         auto-restore BOTH saved panes (`restored-alpha`, `restored-beta`) as dashboard \
         cards, but they never appeared. RED until the snapshot-load block in `run_tui` \
         is made unconditional (today it is gated on `continue_session`).\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Launch the deck against a fresh (empty) daemon with NO snapshot on
/// disk and NO `--continue` flag — the both-empty case. The deck must land on a
/// clean empty dashboard ("No active sessions") with no restore warning, and
/// remain interactive (Ctrl+N opens the new-pane directory picker). This locks
/// the post-Phase-2 invariant that making restore unconditional must still fall
/// through cleanly when there is nothing to restore from either source.
#[spec("session/restore/006")]
#[test]
fn restore_006_empty_daemon_and_no_snapshot_lands_on_clean_dashboard() {
    let session_dir = common::race_safe_tempdir();
    let session_file = session_dir.path().join("session.toml");
    // Nothing staged → `SavedSession::load()` returns the empty default.
    assert!(
        !session_file.exists(),
        "no snapshot must exist for the both-empty case, but one was found at {session_file:?}"
    );

    let deck = TuiDeck::builder()
        .with_env(
            "DOT_AGENT_DECK_SESSION",
            session_file.to_str().expect("session path is UTF-8"),
        )
        .launch_with_fixture("modes");

    // Empty daemon + empty snapshot → the empty-dashboard placeholder.
    deck.wait_for_string("No active sessions");

    // No restore warning should be surfaced when there is nothing to restore.
    let grid = deck.snapshot_grid();
    assert!(
        !grid.contains("Warning:"),
        "the both-empty startup must not surface any restore warning, but the dashboard \
         shows one.\nFinal grid:\n{grid}"
    );

    // Interactive: the global Ctrl+N opens the new-pane directory picker.
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
}
