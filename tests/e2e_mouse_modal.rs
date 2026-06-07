#![cfg(feature = "e2e")]

//! PRD #80 M5 — L2 synthetic tests for modal mouse parity.
//!
//! Spawns the real `dot-agent-deck` binary inside an isolated PTY, opens
//! each modal that can be triggered synthetically, clicks one of its
//! buttons, and asserts the outcome equals the corresponding keystroke.
//! Buttons are located by their bracketed label via `find_in_grid`.
//! Decision 6: gated behind the `e2e` feature so `cargo test-fast` never
//! compiles it.
//!
//! Coverage (see report): quit-confirm `[Cancel]`, config-gen `[Never]`,
//! help `[Close]`. Deferred:
//!   - star-prompt — only opens when the persisted launch counter trips
//!     (`StarPromptState::increment_and_check`), not deterministically
//!     triggerable from the harness; its button rendering is covered by the
//!     L1 spec `mouse/modal/002` instead.
//!   - quit-confirm `[Detach]`/`[Stop]` — these break the TUI loop and exit
//!     the process; the harness has no clean "process exited" assertion, so
//!     the `[Cancel]` click (modal dismisses, app stays) proves the modal's
//!     click→dispatch path. The destructive actions remain keyboard-tested.

mod common;

use common::{TuiDeck, write_hook_line};
use spec::spec;

/// Inject a synthetic Claude Code `SessionStart` hook so a dashboard card
/// (with `cwd`, required by the config-gen path) exists. Mirrors
/// `e2e_hook_delivery.rs`.
fn send_session_start(deck: &TuiDeck, session_id: &str, pane_id: &str, cwd: &str) {
    let event = serde_json::json!({
        "session_id": session_id,
        "agent_type": "claude_code",
        "event_type": "session_start",
        "timestamp": "2026-06-07T12:00:00Z",
        "pane_id": pane_id,
        "cwd": cwd,
    });
    write_hook_line(deck.hook_socket_path(), &event.to_string())
        .expect("write SessionStart hook to per-test socket");
}

/// Click the button whose bracketed label is `needle` (e.g. `[Cancel]`).
fn click_button(deck: &TuiDeck, needle: &str) {
    let (col, row) = deck
        .find_in_grid(needle)
        .unwrap_or_else(|| panic!("modal should render a clickable {needle} button"));
    deck.click(col, row);
}

/// Scenario: From the empty dashboard, press Ctrl+C to open the quit-confirm
/// modal, then click its `[Cancel]` button. The modal must dismiss and the
/// app must stay running — the same outcome as Esc / selecting Cancel — so
/// the dashboard's `No active sessions` empty state is shown again. RED
/// until M5 renders the modal's clickable buttons (today the `[Cancel]`
/// lookup fails).
#[spec("mouse/modal/001")]
#[test]
fn modal_001_quit_confirm_cancel_dismisses() {
    let deck = TuiDeck::launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    deck.send_bytes(b"\x03"); // Ctrl+C → quit-confirm modal
    deck.wait_for_string("Quit dot-agent-deck?");

    click_button(&deck, "[Cancel]");

    // Modal dismissed, app still running → dashboard empty state returns.
    deck.wait_for_string("No active sessions");
}

/// Scenario: With a session card present, press `g` to open the config-gen
/// prompt, then click its `[Never]` button. The prompt must resolve exactly
/// as the keyboard `Never` choice does — it closes and the deck shows the
/// `Config prompt suppressed for this directory.` status. RED until M5
/// renders the modal's clickable buttons.
#[spec("mouse/modal/001")]
#[test]
fn modal_001_config_gen_never_resolves() {
    let deck = TuiDeck::launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");
    send_session_start(&deck, "alpha", "pane-alpha", "/tmp");
    deck.wait_for_string("alpha");

    deck.send_bytes(b"g"); // open config-gen prompt for the selected card
    deck.wait_for_string("Generate .dot-agent-deck.toml");

    click_button(&deck, "[Never]");

    // Same outcome as pressing Never: prompt closes, suppression status set.
    deck.wait_for_string("Config prompt suppressed");
}

/// Scenario: From the dashboard, press `?` to open the help overlay, then
/// click its `[Close]` button. The overlay must close — the same outcome as
/// `?` / Esc / `q` — so the dashboard's `No active sessions` empty state is
/// shown again. RED until M5 renders the help overlay's `[Close]` button.
#[spec("mouse/modal/001")]
#[test]
fn modal_001_help_close_closes_overlay() {
    let deck = TuiDeck::launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    deck.send_bytes(b"?"); // open help overlay
    deck.wait_for_string("Press ? or Esc to close");

    click_button(&deck, "[Close]");

    // Overlay closed → dashboard empty state visible again.
    deck.wait_for_string("No active sessions");
}
