#![cfg(feature = "e2e")]

//! PRD #80 M9 — L2 synthetic tests that existing mouse behavior is PRESERVED
//! after the PRD #80 button layer was added, and that the button hit-test
//! order short-circuits correctly.
//!
//! Unlike the per-milestone RED specs, these assert behavior that should
//! ALREADY hold after M1–M8, so they are expected to pass (GREEN). A failure
//! here is a regression. Spawns the real binary in a PTY and drives the mouse
//! via `TuiDeck::click` / `scroll` / `find_in_grid` / `send_bytes` /
//! `write_hook_line`. Decision 6: gated behind the `e2e` feature.

mod common;

use common::{TuiDeck, write_hook_line};
use spec::spec;

/// Inject a synthetic Claude Code `SessionStart` hook to create a dashboard
/// card. Mirrors `e2e_hook_delivery.rs`.
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

/// Return the first rendered grid line containing `needle`, if any.
fn grid_line_containing(deck: &TuiDeck, needle: &str) -> Option<String> {
    deck.snapshot_grid()
        .lines()
        .find(|l| l.contains(needle))
        .map(|l| l.to_string())
}

/// Scenario: Verify existing mouse behavior survives the PRD #80 button
/// layer, using a real `--continue`-spawned pane (`realpane`, running a
/// long-lived command). (1) Double-clicking its dashboard card still focuses
/// the pane and enters PaneInput (the focus_pane / focused_pane_rect path).
/// (2) A non-button click inside the focused-pane region is NOT swallowed by
/// the button layer — it does not fire a global button action (no picker
/// opens). (3) A scroll wheel event in the pane region likewise reaches the
/// scroll path, not the button hit-test (which only runs on Down/Up). Should
/// be GREEN (asserts existing behavior). DEFERRED, with reasons, in the body:
/// mode-tab side/agent click-to-focus, text-selection drag/multi-click,
/// Ctrl+click hyperlink, and child-app mouse forwarding.
#[spec("mouse/preserve/001")]
#[test]
fn preserve_001_existing_pane_mouse_behavior_intact() {
    // DEFERRED sub-behaviors (not asserted here) and why:
    //  - Mode-tab side/agent pane click-to-focus: focus there is visual-only
    //    (border highlight, no PaneInput status), not robustly readable via
    //    vt100, and needs heavy mode-tab setup. It shares the same
    //    `pane.focus_pane` mechanism exercised below via the dashboard.
    //  - Text selection (drag / double-click word / triple-click): the
    //    harness sends discrete clicks; driving a Drag sequence and reading
    //    the selection highlight from the grid is not robust. The
    //    dispatch/last_click coexistence is already unit-covered.
    //  - Ctrl+click hyperlink: opens a URL via `open::that` — no way to
    //    observe link-open in the harness.
    //  - Child-app mouse forwarding (mouse_mode_enabled): needs a child TUI
    //    that enables mouse mode; `sleep` does not.
    let deck = TuiDeck::builder()
        .with_continue_session("realpane", "sleep 600")
        .launch_with_fixture("minimal");
    deck.wait_until_quiescent();
    deck.send_bytes(b"\x04"); // Ctrl+D → ensure dashboard
    deck.wait_for_string("realpane");

    // (1) Double-click the card → pane focused, PaneInput entered.
    let (col, row) = deck
        .find_in_grid("realpane")
        .expect("realpane card should be on the dashboard");
    deck.click(col, row);
    deck.click(col, row);
    deck.wait_for_string("PaneInput mode");

    // (2) A non-button click inside the focused-pane region is not swallowed
    // into a button action — clicking mid-pane must NOT open the New-Pane
    // picker (the button layer's action). Coordinates are well inside the
    // pane and away from the bottom button bar (last row).
    deck.click(20, 5);
    deck.wait_until_quiescent();
    assert!(
        !deck.snapshot_grid().contains("Select Directory"),
        "a non-button click in the pane region must not fire a global button action:\n{}",
        deck.snapshot_grid()
    );

    // (3) A scroll event in the pane region reaches the scroll path, not the
    // button layer (which only hit-tests Down/Up) — it must not fire a button
    // action either.
    deck.scroll(20, 5, true);
    deck.wait_until_quiescent();
    assert!(
        !deck.snapshot_grid().contains("Select Directory"),
        "a scroll event must not be intercepted by the button layer:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Verify the button hit-test order — buttons short-circuit, misses
/// fall through. With two dashboard cards (`alpha`, `bravo`): (1) clicking the
/// `bravo` card (which misses every button) falls through to the existing
/// card-selection path, moving the `▸` selection marker to `bravo`. (2)
/// Clicking the global `[New Pane Ctrl+N]` bar button fires its action (the
/// directory picker opens) AND short-circuits — after dismissing the picker
/// the card selection is still on `bravo`, proving the button click did not
/// also fall through to the card/pane layer underneath. Should be GREEN
/// (asserts existing M2 + M4 + hit-test-order behavior).
#[spec("mouse/preserve/002")]
#[test]
fn preserve_002_button_short_circuits_miss_falls_through() {
    let deck = TuiDeck::launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");
    send_session_start(&deck, "alpha", "pane-alpha", "/tmp");
    deck.wait_for_string("alpha");
    send_session_start(&deck, "bravo", "pane-bravo", "/tmp");
    deck.wait_for_string("bravo");

    // (1) Miss-falls-through: clicking the bravo card selects it (▸ marker).
    let (col, row) = deck
        .find_in_grid("bravo")
        .expect("bravo card should be on the dashboard");
    deck.click(col, row);
    deck.wait_until_quiescent();
    let bravo_line = grid_line_containing(&deck, "bravo").expect("bravo card row");
    assert!(
        bravo_line.contains("> ") || bravo_line.contains('▸'),
        "clicking the bravo card should select it (selection marker), got: {bravo_line:?}"
    );

    // (2) Short-circuit: clicking the [New Pane Ctrl+N] bar button fires its
    // action (picker opens) and does NOT also act on the cards underneath.
    let (bcol, brow) = deck
        .find_in_grid("[New Pane Ctrl+N]")
        .expect("global button bar should render [New Pane Ctrl+N]");
    deck.click(bcol, brow);
    deck.wait_for_string("Select Directory");

    // Dismiss the picker and confirm the card selection was untouched by the
    // button click (it short-circuited rather than falling through).
    deck.send_bytes(b"\x1b"); // Esc → close picker → back to dashboard
    deck.wait_for_string("bravo");
    let bravo_line_after = grid_line_containing(&deck, "bravo").expect("bravo card row");
    assert!(
        bravo_line_after.contains("> ") || bravo_line_after.contains('▸'),
        "the button click must short-circuit — bravo should still be selected, got: {bravo_line_after:?}"
    );
}

/// Scenario: PRD #80 review FIX 1 — a click that misses every modal button
/// must be CONSUMED by the modal layer, never falling through to the pane
/// behind the popup (where it could start a text selection / OSC-52 copy).
/// Focus a real `--continue` pane so a focused pane renders behind, return to
/// the dashboard, open the quit-confirm modal (Ctrl+C), then double-click a
/// blank interior cell of the modal offset to the right so it overlaps the
/// pane region behind the centered popup. The modal must stay open (its prompt
/// still visible) and no `Copied to clipboard` status may appear — proving the
/// stray click was consumed rather than leaking into the pane behind. (Build-
/// checked here; not run in the fast tier.)
#[test]
fn preserve_modal_click_miss_is_consumed() {
    let deck = TuiDeck::builder()
        .with_continue_session("realpane", "sleep 600")
        .launch_with_fixture("minimal");
    deck.wait_until_quiescent();
    deck.send_bytes(b"\x04"); // Ctrl+D → dashboard
    deck.wait_for_string("realpane");

    // Focus the pane (Enter) so a focused pane renders behind, then return to
    // the dashboard — the pane stays focused behind the modal.
    deck.send_bytes(b"\r");
    deck.wait_for_string("PaneInput mode");
    deck.send_bytes(b"\x04"); // Ctrl+D → back to dashboard
    deck.wait_until_quiescent();

    // Open the quit-confirm modal over the dashboard.
    deck.send_bytes(b"\x03"); // Ctrl+C → quit-confirm
    deck.wait_for_string("Quit dot-agent-deck?");

    // Double-click a blank interior cell of the modal, offset right so it sits
    // over the pane region behind the centered popup.
    let (qcol, qrow) = deck
        .find_in_grid("Quit dot-agent-deck?")
        .expect("quit-confirm modal should be open");
    let cx = qcol + 20;
    let cy = qrow + 1; // the blank line just below the title
    deck.click(cx, cy);
    deck.click(cx, cy); // second click within the double-click window
    deck.wait_until_quiescent();

    let grid = deck.snapshot_grid();
    assert!(
        grid.contains("Quit dot-agent-deck?"),
        "the modal must stay open after a click on its empty interior:\n{grid}"
    );
    assert!(
        !grid.contains("Copied to clipboard"),
        "a modal-interior click must not leak into a text selection/copy behind the popup:\n{grid}"
    );
}

/// Scenario: PRD #80 review FIX 3 — a disabled (dimmed) button is inert. On an
/// empty dashboard (no cards) the `[Generate g]` context button is disabled,
/// so clicking it must be a no-op, exactly like pressing `g` with no cards.
/// The config-gen prompt must NOT open, and the "no active agent session"
/// status that the RequestConfigGen action would otherwise set must NOT appear
/// — i.e. the disabled button records no clickable rect. (Build-checked here;
/// not run in the fast tier.)
#[test]
fn preserve_disabled_button_is_inert() {
    let deck = TuiDeck::launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    // The dashboard bar renders [Generate g] dimmed (no cards → disabled).
    let (col, row) = deck
        .find_in_grid("Generate")
        .expect("dashboard bar should render a (dimmed) Generate button");
    deck.click(col, row);
    deck.wait_until_quiescent();

    let grid = deck.snapshot_grid();
    assert!(
        !grid.contains("Generate .dot-agent-deck.toml"),
        "clicking the disabled Generate button must not open the config-gen prompt:\n{grid}"
    );
    assert!(
        !grid.contains("No active agent session"),
        "clicking the disabled Generate button must be a true no-op (no RequestConfigGen side effect):\n{grid}"
    );
}
