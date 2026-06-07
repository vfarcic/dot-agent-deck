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

/// Scenario: Verify existing pane mouse behavior survives the PRD #80 button
/// layer. A real `--continue`-spawned pane (`realpane`, running a long-lived
/// command) is auto-focused on launch (PaneInput → [Detach Ctrl+D]), which
/// already exercises the focus_pane / focused_pane_rect path. (1) A non-button
/// click inside the focused-pane region and (2) a scroll wheel event in that
/// region must NOT be swallowed by the button hit-test layer (which only runs
/// on Down/Up) — they reach the existing pane path, so no global button action
/// (no picker) fires. We then detach via the [Detach Ctrl+D] affordance and
/// return cleanly to Normal mode; if a mid-pane event had wrongly navigated
/// away, the detach would not return us to the global bar. Should be GREEN
/// (asserts existing behavior). DEFERRED, with reasons, in the body:
/// explicit double-click-to-focus from the dashboard (covered by
/// mouse/dashboard/001), mode-tab side/agent click-to-focus, text-selection
/// drag/multi-click, Ctrl+click hyperlink, and child-app mouse forwarding.
#[spec("mouse/preserve/001")]
#[test]
fn preserve_001_existing_pane_mouse_behavior_intact() {
    // DEFERRED sub-behaviors (not asserted here) and why:
    //  - Explicit double-click-to-focus a card: covered by mouse/dashboard/001.
    //  - Mode-tab side/agent pane click-to-focus: focus there is visual-only
    //    (border highlight, no PaneInput status), not robustly readable via
    //    vt100, and needs heavy mode-tab setup; same `pane.focus_pane` path.
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
    // --continue auto-focuses the restored pane → PaneInput, shown by the
    // [Detach Ctrl+D] affordance (the focus path works).
    deck.wait_for_string("[Detach Ctrl+D]");

    // (1)+(2) A non-button click AND a scroll inside the focused-pane region
    // (right-hand preview) must NOT be swallowed by the button layer. Capture
    // the [Detach Ctrl+D] affordance, then send mid-pane click, mid-pane
    // scroll, and the detach click — buffered and processed in order. If a
    // mid-pane event had wrongly fired the New-Pane button, the picker would
    // cover the bar and the detach click would not return us to Normal.
    let (dcol, drow) = deck
        .find_in_grid("[Detach Ctrl+D]")
        .expect("PaneInput bottom bar should render [Detach Ctrl+D]");
    deck.click(60, 5); // non-button click inside the focused-pane preview
    deck.scroll(60, 5, true); // scroll inside the pane region (not Down/Up → never hits buttons)
    deck.click(dcol, drow); // detach
    deck.wait_for_string("[New Pane Ctrl+N]"); // cleanly back to Normal — events didn't navigate away
    assert!(
        !deck.snapshot_grid().contains("Select Directory"),
        "a non-button click/scroll in the pane region must not open the New-Pane picker:\n{}",
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

    // (1) Miss-falls-through: clicking the bravo card selects it (▸ marker
    // on bravo's row). The deterministic wait IS the assertion.
    let (col, row) = deck
        .find_in_grid("bravo")
        .expect("bravo card should be on the dashboard");
    deck.click(col, row);
    let bravo_selected = |g: &str| {
        g.lines()
            .any(|l| l.contains("bravo") && (l.contains('▸') || l.contains("> ")))
    };
    deck.wait_until_grid("bravo card selected", bravo_selected);

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
    deck.wait_for_absence("Select Directory");
    deck.wait_until_grid("bravo still selected", bravo_selected);
}

/// Scenario: PRD #80 review FIX 1 — a click that misses every modal button
/// must be CONSUMED by the modal layer, never falling through to the content
/// behind the popup (where it could start a text selection / OSC-52 copy).
/// With a real `--continue` pane on the dashboard behind it, open the
/// quit-confirm modal (Ctrl+C), then double-click a blank interior cell of the
/// modal offset to the right so it overlaps the region behind the centered
/// popup. The modal must stay open (its [Cancel] button still present) and no
/// `Copied to clipboard` status may appear — proving the stray click was
/// consumed rather than leaking behind.
#[test]
fn preserve_modal_click_miss_is_consumed() {
    let deck = TuiDeck::builder()
        .with_continue_session("realpane", "sleep 600")
        .launch_with_fixture("minimal");
    // --continue auto-focuses the restored pane (PaneInput). Detach to the
    // dashboard so the modal opens over the dashboard, with the (now
    // unfocused) pane region behind the centered popup.
    deck.wait_for_string("[Detach Ctrl+D]");
    deck.send_bytes(b"\x04"); // Ctrl+D → dashboard / Normal
    deck.wait_for_string("[New Pane Ctrl+N]");

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

    // The modal stayed open and consumed the stray click: clicking its
    // [Cancel] button (buffered after the stray clicks, processed in order)
    // closes it. If the stray click had leaked/closed the modal, [Cancel]
    // would be gone and this would fail. No copy may have leaked to the pane
    // behind.
    let (ccol, crow) = deck
        .find_in_grid("[Cancel]")
        .expect("quit-confirm modal should still render its [Cancel] button");
    assert!(
        !deck.snapshot_grid().contains("Copied to clipboard"),
        "a modal-interior click must not leak into a text selection/copy behind the popup:\n{}",
        deck.snapshot_grid()
    );
    deck.click(ccol, crow);
    deck.wait_for_absence("Quit dot-agent-deck?");
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

    // Anchor: open the help overlay (?). It renders only from Normal mode, so
    // reaching it proves (a) the deck processed the prior disabled-click in
    // order and (b) the disabled click did NOT open the config-gen prompt
    // (from which `?` would do nothing and this wait would time out).
    deck.send_bytes(b"?");
    deck.wait_for_string("works from any pane");

    let grid = deck.snapshot_grid();
    assert!(
        !grid.contains("No workspace modes config found"),
        "clicking the disabled Generate button must not open the config-gen prompt:\n{grid}"
    );
    assert!(
        !grid.contains("No active agent session"),
        "clicking the disabled Generate button must be a true no-op (no RequestConfigGen side effect):\n{grid}"
    );
}
