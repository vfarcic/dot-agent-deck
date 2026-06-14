#![cfg(feature = "e2e")]

//! PRD #89 Phase 1 — L2 (real-binary PTY) coverage for *snapshot freshness*.
//!
//! Today the saved-session snapshot is written ONLY at clean teardown/quit
//! (`run_tui`'s pre-teardown `config::SavedSession::snapshot(...)` block). PRD
//! #89 Phase 1 makes it continuously fresh: the snapshot must also be written
//! on meaningful TUI state changes (M1.2 — new pane, rename, tab open/close…)
//! and on the detach paths (M1.3 — Ctrl+W close-pane, disconnect, detach from
//! the quit dialog), coalesced so a burst of changes is one or two writes.
//!
//! These two tests drive the REAL binary through a PTY with
//! `DOT_AGENT_DECK_SESSION` redirected to a test-owned path, perform a state
//! change / detach WITHOUT quitting, and assert the snapshot file on disk
//! reflects it. They are RED until M1.2/M1.3 land: the only writer today is
//! teardown, which these tests never reach.
//!
//! No LLM tokens are spent — the panes run `sleep 600` (Agent: none).
//!
//! Decision 6: gated behind the `e2e` feature so `cargo test-fast` never
//! compiles it.

mod common;

use std::time::Duration;

use common::TuiDeck;
use spec::spec;

/// Create a plain dashboard pane (no mode) running `command` via the new-pane
/// flow, then wait until it has spawned and auto-focused (PaneInput → the
/// bottom bar shows `[Detach Ctrl+D]`). Mirrors `e2e_dashboard_selection`'s
/// `spawn_mode` minus the mode-selection Right(s): Ctrl+N → dir-picker (Space
/// confirms the cwd) → form (Enter past Mode, Enter past the default Name, type
/// the command, Enter submits). The leading Ctrl+D guarantees we drive the
/// global Ctrl+N from Normal mode even when a previously spawned pane left us in
/// PaneInput (a no-op when already Normal).
fn spawn_plain_pane(deck: &TuiDeck, command: &str) {
    deck.send_keys(b"\x04"); // Ctrl+D → Normal mode (no-op if already Normal)
    deck.send_keys(b"\x0e"); // Ctrl+N → directory picker
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" "); // Space → confirm current dir → new-pane form
    deck.wait_for_string("No mode"); // form up, Mode field focused at "No mode"
    deck.send_keys(b"\r"); // Mode → Name
    deck.send_keys(b"\r"); // Name (default) → Command
    deck.send_keys(command.as_bytes());
    deck.send_keys(b"\r"); // submit
    deck.wait_for_string("[Detach Ctrl+D]"); // pane spawned & auto-focused
}

/// Scenario: Launch the deck with `DOT_AGENT_DECK_SESSION` redirected to a
/// test-owned path that has NO prior snapshot, then create a new dashboard pane
/// running `sleep 600` via the new-pane flow (Ctrl+N → Space → form → submit)
/// and DO NOT quit. The new pane is a meaningful TUI state change, so a fresh
/// `session.toml` must be written to that path containing the new pane's
/// `sleep 600` command. RED today: the snapshot is only written at clean
/// teardown/quit, which this test never reaches — so the file never appears.
#[spec("session/save/001")]
#[test]
fn save_001_new_pane_state_change_writes_snapshot() {
    // A test-owned snapshot path the deck's `session_path()` will read/write
    // (via the `DOT_AGENT_DECK_SESSION` env). Kept alive for the whole test so
    // the dir is not reaped early.
    let session_dir = common::race_safe_tempdir();
    let session_file = session_dir.path().join("session.toml");

    let deck = TuiDeck::builder()
        .with_env(
            "DOT_AGENT_DECK_SESSION",
            session_file.to_str().expect("session path is UTF-8"),
        )
        .launch_with_fixture("modes");
    deck.wait_for_string("No active sessions");

    // Precondition: a fresh launch (no `--continue`, restore not yet wired)
    // must not have written any snapshot yet — the RED signal below is purely
    // "the state change wrote nothing".
    assert!(
        !session_file.exists(),
        "no prior snapshot must exist at launch, but one was found at {session_file:?}"
    );

    // Meaningful state change WITHOUT quitting: create a new dashboard pane.
    spawn_plain_pane(&deck, "sleep 600");

    // The snapshot must now reflect the new pane. RED today (only teardown
    // writes the snapshot; we never quit), so the file never appears.
    let written =
        common::wait_for_file_substr_count(&session_file, "sleep 600", 1, Duration::from_secs(10));
    assert!(
        written,
        "creating a new dashboard pane is a meaningful state change (PRD #89 M1.2): a fresh \
         snapshot containing the pane's `sleep 600` command must be written to \
         {session_file:?} WITHOUT quitting, but no such snapshot appeared.\nFile exists: {}",
        session_file.exists()
    );
}

/// Scenario: Launch the deck with `DOT_AGENT_DECK_SESSION` redirected to a
/// test-owned path, create two `sleep 600` dashboard panes, detach to Normal
/// mode and arm the dashboard selection (`j` → `▸`), then DELETE any snapshot
/// already on disk and close the selected pane with Ctrl+W. Closing a pane is a
/// detach path that must flush a fresh snapshot reflecting the (still non-empty)
/// workspace — so a new `session.toml` containing a surviving `sleep 600` pane
/// must reappear, without quitting. RED today: Ctrl+W tears the pane down but
/// writes no snapshot (only clean teardown does), so the deleted file never
/// comes back.
#[spec("session/save/002")]
#[test]
fn save_002_detach_path_writes_snapshot() {
    let session_dir = common::race_safe_tempdir();
    let session_file = session_dir.path().join("session.toml");

    let deck = TuiDeck::builder()
        .with_env(
            "DOT_AGENT_DECK_SESSION",
            session_file.to_str().expect("session path is UTF-8"),
        )
        .launch_with_fixture("modes");
    deck.wait_for_string("No active sessions");

    // Two dashboard panes present → a real workspace to detach from.
    spawn_plain_pane(&deck, "sleep 600");
    spawn_plain_pane(&deck, "sleep 600");

    // Detach to Normal mode and confirm both panes registered as cards.
    deck.send_keys(b"\x04"); // Ctrl+D → Normal mode / Dashboard
    deck.wait_for_string("2 session(s)");

    // Arm the dashboard selection so Ctrl+W has a concrete card to close
    // (PRD #113: the destructive close is a no-op on an unarmed dashboard).
    deck.send_keys(b"j");
    deck.wait_for_string("\u{25b8}"); // ▸ — a card is highlighted

    // Remove any snapshot already written by the new-pane state changes (M1.2)
    // so the assertion below is provably driven by the detach path (M1.3), not
    // a stale earlier write.
    let _ = std::fs::remove_file(&session_file);

    // Detach path: close the selected pane with Ctrl+W. One card remains, so
    // the workspace is still non-empty.
    deck.send_keys(b"\x17"); // Ctrl+W → CloseSelected
    deck.wait_for_string("1 session(s)"); // close took effect

    // The detach must have flushed a fresh snapshot reflecting the surviving
    // workspace. RED today: Ctrl+W writes no snapshot (only clean teardown
    // does), so the deleted file never reappears.
    let written =
        common::wait_for_file_substr_count(&session_file, "sleep 600", 1, Duration::from_secs(10));
    assert!(
        written,
        "closing a pane with Ctrl+W is a detach path (PRD #89 M1.3): a fresh snapshot \
         reflecting the surviving `sleep 600` workspace must be flushed to {session_file:?} \
         WITHOUT quitting, but no such snapshot reappeared after the close.\nFile exists: {}",
        session_file.exists()
    );
}
