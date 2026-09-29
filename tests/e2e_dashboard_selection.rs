#![cfg(feature = "e2e")]

//! L2 (real-binary PTY) coverage for orchestration selection highlights.
//!
//! Selection cue note: PRD #13 Option A cues the highlighted card with a `▸`
//! title prefix (and a cyan-bold border), not an absolute background fill, and
//! `▸` is used ONLY for that selection marker (src/ui.rs `render_session_card`).
//! So "no card carries the highlight" is asserted as the absence of `▸` from the
//! rendered grid — the same observable the L1 selection tests use.
//!
//! Decision 6: gated behind the `e2e` feature so `cargo test-fast` never
//! compiles it.

mod common;

use common::TuiDeck;
use spec::spec;

/// Drive the new-pane dialog to open the (single) orchestration in the fixture.
/// The Mode chip row begins `[No mode] [Orch: …]`, so ONE Right selects
/// the orchestration; selecting an
/// orchestration HIDES the Command field, so a second Enter submits the form.
fn open_orchestration(deck: &TuiDeck) {
    deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
    deck.send_keys(b" "); // Space → confirm current dir → new-pane form
    deck.wait_for_string("No mode"); // form up, Mode field focused at "No mode"
    deck.send_keys(b"\x1b[C"); // Right → [Orch: demo-orch]
    deck.send_keys(b"\r"); // Mode → Name
    deck.send_keys(b"\r"); // submit (Command hidden for an orchestration)
}

/// Scenario: PR #151 manual-test regression (Issue 1) against the real binary —
/// Enter must PAINT the selection highlight on the Orchestration deck after a
/// tab round-trip. Open an orchestration (two `cat` role cards), detach to
/// Normal mode, arm a role with `j` (a `▸` marker appears), then round-trip
/// Orchestration → Dashboard → Orchestration (the highlight clears, SC1). On
/// return the orchestration re-focuses its remembered role pane, so pressing
/// Enter re-focuses the SAME pane — not a focus transition — and under the
/// pre-fix behavior the reconcile never re-arms the highlight, so `▸` never
/// reappears (this `wait_for_string` times out). The unified fix makes
/// Action::Focus set `selected_index` directly, so Enter paints the highlight on
/// the orchestration deck just like the Dashboard. This is the higher-risk
/// VISUAL behavior the L1 mocks repeatedly missed (they never run the real
/// reconcile + focus-restore on an orchestration tab), hence the L2 coverage.
#[spec("dashboard/selection/019")]
#[test]
fn selection_019_enter_paints_highlight_on_orchestration_real_binary() {
    let deck = TuiDeck::builder()
        .with_pty_size(120, 40)
        .launch_with_fixture("orch-deck");
    deck.wait_for_string("No active agents");

    // Open the orchestration tab (Dashboard + Orchestration = the ≥2 tabs the
    // round-trip needs). Its two `cat` role panes render as deck cards.
    open_orchestration(&deck);
    deck.wait_for_string("worker"); // the 2nd role card → orchestration deck is up

    // Detach to Normal mode on the Orchestration tab so `j` / Enter drive the
    // deck (Enter would otherwise type into the focused role pane).
    deck.send_bytes(b"\x04"); // Ctrl+D → Normal mode
    // Arm a role: `j` activates the deck selection, painting the `▸` marker.
    deck.send_bytes(b"j");
    deck.wait_for_string("\u{25b8}"); // ▸ — a role card is now highlighted

    // Round-trip: Orchestration → Dashboard → Orchestration. The highlight
    // clears on the tab switch (SC1 / orchestration_003).
    deck.send_bytes(b"\x1b[D"); // Left → previous tab → Dashboard
    deck.wait_for_absence("worker"); // left the orchestration (role cards gone; role panes are excluded from the Dashboard)
    deck.wait_for_absence("\u{25b8}"); // highlight cleared
    deck.send_bytes(b"\x1b[C"); // Right → next tab → Orchestration
    deck.wait_for_string("worker"); // back on the Orchestration deck

    // THE REGRESSION: Enter on the Orchestration deck must RESTORE the previous
    // role AND paint its highlight. Pre-fix the role pane is already focused, so
    // Enter is not a focus transition and the highlight never reappears — this
    // wait times out. Post-fix Action::Focus sets `selected_index` directly.
    deck.send_bytes(b"\r"); // Enter → Action::Focus (restore + paint)
    deck.wait_for_string("\u{25b8}"); // ▸ must reappear on the restored role
}
