#![cfg(feature = "e2e")]

//! PRD #80 M6 — L2 synthetic tests for inline-edit + PaneInput mouse parity.
//!
//! Spawns the real `dot-agent-deck` binary inside an isolated PTY and drives
//! the mouse via SGR reports through `TuiDeck::click` / `wait_for_in_grid` /
//! `send_bytes`. Covers the filter-row `[Apply]`/`[Cancel]`, rename-row
//! `[Save]`/`[Cancel]`, click-in-field focus retention, and the PaneInput
//! `[Command Mode Ctrl+D]` affordance — each asserted to equal the corresponding
//! keystroke. Decision 6: gated behind the `e2e` feature so `cargo
//! test-fast` never compiles it.

mod common;

use common::{TuiDeck, write_hook_line};
use spec::spec;

/// Inject a synthetic Claude Code `SessionStart` hook so a dashboard card
/// exists (filter/rename operate on cards). Mirrors `e2e_hook_delivery.rs`.
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

/// Click the button/affordance whose label text is `needle`.
fn click_button(deck: &TuiDeck, needle: &str) {
    let (col, row) = deck.wait_for_in_grid(needle);
    deck.click(col, row);
}

/// Scenario: With a card present, press `/` to enter filter mode and type
/// `zq`; click inside the filter input field (it must keep input focus), type
/// `x`, then click `[Apply]`. The filter applies exactly as Enter does — the
/// app returns to Normal (the global `[New Pane Ctrl+N]` bar is back) with
/// the `zqx` filter still active, so the non-matching `alpha` card stays
/// hidden. (Click-in-field is asserted as focus-retention/typing-still-
/// captured; exact cursor column is not read from the vt100 grid.) RED until
/// M6 renders the inline `[Apply]` button.
#[spec("mouse/inline/001")]
#[test]
fn inline_001_filter_apply_commits() {
    // PRD #127: 200 cols so the Normal-mode bar (reached after Apply) renders
    // the FULL `[New Pane Ctrl+N]` label; at 120 it collapses to chips once the
    // always-shown Scheduled Tasks button is included.
    let deck = TuiDeck::builder()
        .with_pty_size(200, 40)
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");
    send_session_start(&deck, "alpha", "pane-alpha", "/tmp");
    deck.wait_for_string("alpha");

    deck.send_bytes(b"/"); // enter filter mode
    deck.send_bytes(b"zq");
    deck.wait_for_string("zq");

    // Click inside the filter input field — focus must stay in the input.
    click_button(&deck, "/ zq");
    deck.send_bytes(b"x");
    deck.wait_for_string("zqx");

    click_button(&deck, "[Apply]");

    // Applied like Enter: Normal mode (button bar back), filter still active.
    deck.wait_for_string("[New Pane Ctrl+N]");
    assert!(
        !deck.snapshot_grid().contains("alpha"),
        "applied 'zqx' filter should keep the non-matching alpha card hidden:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: With a card present, press `/` to enter filter mode and type a
/// non-matching sentinel, then click `[Cancel]`. The filter is abandoned
/// exactly as Esc does — the app returns to Normal and the `alpha` card
/// (hidden while the sentinel filter was live) is visible again. RED until
/// M6 renders the inline `[Cancel]` button.
#[spec("mouse/inline/001")]
#[test]
fn inline_001_filter_cancel_abandons() {
    // PRD #127: 200 cols so the Normal-mode bar (reached after Cancel) renders
    // the FULL `[New Pane Ctrl+N]` label; at 120 it collapses to chips once the
    // always-shown Scheduled Tasks button is included.
    let deck = TuiDeck::builder()
        .with_pty_size(200, 40)
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");
    send_session_start(&deck, "alpha", "pane-alpha", "/tmp");
    deck.wait_for_string("alpha");

    deck.send_bytes(b"/");
    deck.send_bytes(b"zqx");
    deck.wait_for_string("zqx");

    click_button(&deck, "[Cancel]");

    // Abandoned like Esc: filter cleared, the alpha card returns.
    deck.wait_for_string("[New Pane Ctrl+N]");
    deck.wait_for_string("alpha");
}

/// Scenario: With a selected card backed by a real pane, press `r` to enter
/// rename mode, type a new name, then click `[Save]`. The rename commits
/// exactly as Enter does — the new name replaces the pane id in the card's
/// own title. RED until M6 renders the inline `[Save]` button.
#[spec("mouse/inline/001")]
#[test]
fn inline_001_rename_save_commits() {
    // PRD #127: 200 cols so the Normal-mode bar (reached after Save) renders
    // the FULL `[New Pane Ctrl+N]` label; at 120 it collapses to chips once the
    // always-shown Scheduled Tasks button is included.
    //
    // Issue #818: this drives a REAL pane, and that is load-bearing rather
    // than incidental. A card synthesized by a `session_start` hook line names
    // a `pane_id` the pane controller does not own, so `rename_pane` resolves
    // it against `self.panes` and returns `Err(Pane … not found)`
    // (`embedded_pane.rs`), which `commit_rename` swallows to a `debug!` with
    // the UI display-name maps left untouched — correct behaviour for a pane
    // that does not exist. The card was therefore NEVER renamed, and the
    // assertion below could only ever be satisfied by the `Rename: renamed7`
    // text still resident in the previous frame. That won the race locally
    // (2.0s) and lost it on a loaded runner, where the bottom bar repainted
    // first and the wait burned its full 10s bound.
    let deck = TuiDeck::builder()
        .with_pty_size(200, 40)
        .with_continue_session("realpane", "sleep 600")
        .launch_with_fixture("minimal");
    // --continue auto-focuses the restored pane → PaneInput; Ctrl+D returns to
    // the dashboard, where a card is selected and `r` is bound.
    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Pane Ctrl+N]");
    deck.wait_for_string("realpane");

    deck.send_bytes(b"r"); // enter rename mode for the selected card
    deck.wait_for_string("Rename:");
    deck.send_bytes(b"renamed7");
    deck.wait_for_string("Rename: renamed7");

    click_button(&deck, "[Save]");

    // Committed like Enter: the new name reaches the CARD. Asserted as the
    // title's `<type> · <name>` shape, which the rename input cannot produce —
    // a bare `renamed7` would also match the `Rename: renamed7` row this click
    // is dismissing, which is exactly how this test passed without the rename
    // ever having happened.
    deck.wait_for_string("· renamed7");
    deck.wait_for_string("[New Pane Ctrl+N]"); // back to Normal
}

/// Scenario: With a selected card backed by a real pane, press `r` to enter
/// rename mode, type a new name, then click `[Cancel]`. The rename is
/// abandoned exactly as Esc does — the card keeps its original `realpane`
/// title and the typed name is not applied. RED until M6 renders the inline
/// `[Cancel]` button on the rename row.
#[spec("mouse/inline/001")]
#[test]
fn inline_001_rename_cancel_abandons() {
    // Issue #818: a real pane here too, and for a sharper reason than in
    // `inline_001_rename_save_commits`. Against a hook-synthesized card the
    // rename could never apply at all, so "the typed name is not applied"
    // held whatever `[Cancel]` did — this assertion passed identically if the
    // click had COMMITTED. With a renameable pane, Cancel and Save are
    // genuinely distinguishable, which is the contract this pair exists to
    // pin. 200 cols to match its sibling, so both reach the full Normal bar.
    let deck = TuiDeck::builder()
        .with_pty_size(200, 40)
        .with_continue_session("realpane", "sleep 600")
        .launch_with_fixture("minimal");
    // --continue auto-focuses the restored pane → PaneInput; Ctrl+D returns to
    // the dashboard, where a card is selected and `r` is bound.
    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Pane Ctrl+N]");
    deck.wait_for_string("· realpane");

    deck.send_bytes(b"r");
    deck.wait_for_string("Rename:");
    deck.send_bytes(b"discarded9");
    deck.wait_for_string("Rename: discarded9");

    click_button(&deck, "[Cancel]");

    // Abandoned like Esc: the rename input is discarded and the card keeps the
    // title it had. Both halves are asserted — the typed name gone proves the
    // row was dismissed, and `· realpane` still present proves the click did
    // not commit, which is what tells Cancel apart from Save.
    deck.wait_for_absence("discarded9");
    deck.wait_for_string("· realpane");
}

/// Scenario: A real `--continue`-spawned pane (`realpane`, running a long-
/// lived command) is auto-focused on launch, so the TUI is in PaneInput mode
/// showing the `[Command Mode Ctrl+D]` affordance. Click that affordance — it must
/// return to the dashboard exactly as pressing Ctrl+D (`Action::
/// DetachToNormal`) does, so the PaneInput `[Command Mode Ctrl+D]` bar is replaced
/// by the Normal-mode global bar. RED until M6 renders the `[Command Mode Ctrl+D]`
/// affordance.
#[spec("mouse/inline/001")]
#[test]
fn inline_001_pane_input_detach_returns_to_dashboard() {
    // PRD #127: 200 cols so the Normal-mode bar (reached after detach) renders
    // the FULL `[New Pane Ctrl+N]` label; at 120 it collapses to chips once the
    // always-shown Scheduled Tasks button is included.
    let deck = TuiDeck::builder()
        .with_pty_size(200, 40)
        .with_continue_session("realpane", "sleep 600")
        .launch_with_fixture("minimal");
    // --continue auto-focuses the single restored pane → PaneInput, so the
    // [Command Mode Ctrl+D] affordance is already shown. That is exactly the
    // affordance under test.
    deck.wait_for_string("[Command Mode Ctrl+D]");

    // Click the detach affordance — same outcome as pressing Ctrl+D.
    click_button(&deck, "[Command Mode Ctrl+D]");

    // Detached to the dashboard: PaneInput's affordance is replaced by the
    // Normal-mode global bar.
    deck.wait_for_string("[New Pane Ctrl+N]");
    deck.wait_for_absence("[Command Mode Ctrl+D]");
}
