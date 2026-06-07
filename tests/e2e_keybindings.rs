#![cfg(feature = "e2e")]

//! L2 end-to-end keybinding tests (PRD #40 — Customizable Keybindings).
//!
//! Each function spawns the real `dot-agent-deck` binary inside an
//! isolated PTY, stages a `keybindings.toml` in the per-test HOME's
//! config dir (`$HOME/.config/dot-agent-deck/keybindings.toml`), drives
//! keystrokes through the PTY master, and asserts on the rendered grid
//! via the `vt100` parser. These tests are *interface-agnostic*: they
//! drop a config file and assert observable TUI behaviour, so they do
//! not depend on any not-yet-written config struct API.
//!
//! Decision 6: gated behind the `e2e` feature so CI's `cargo test-fast`
//! never compiles them. PRD #40 design: keybindings resolve CLIENT-SIDE
//! (the TUI event loop reads the config and matches keys to actions; the
//! daemon stays binding-agnostic), so placing the file under the
//! client's HOME and asserting on the rendered grid is the right level.
//!
//! ROUND 1 (RED): every test here is expected to FAIL until the
//! keybinding config system is implemented — today the deck does not
//! read `keybindings.toml` at all, so remapped keys do nothing, unbound
//! keys still fire their hardcoded defaults, and no fallback warning is
//! emitted.

mod common;

use common::TuiDeck;
use spec::spec;

/// Scenario: Stage a `keybindings.toml` that rebinds the global
/// `toggle_layout` action from its REAL default `Ctrl+t` to
/// `Alt+Shift+l`, launch the deck against the `minimal` fixture, and
/// press `Alt+Shift+l`. The dashboard layout should toggle
/// (stacked <-> tiled), surfacing a `Layout: …` status message in the
/// bottom bar. Then press the old real default toggle key (`Ctrl+t`) and
/// confirm it no longer toggles — proving the binding was resolved
/// client-side from the config rather than from the hardcoded default.
#[spec("keybindings/remap/001")]
#[test]
fn remap_001_global_action_rebind() {
    // PRD #40 catalog: keybindings/remap/001 — a config remap of a
    // GLOBAL action takes effect on the new combo and removes the old
    // default. Bindings resolve CLIENT-SIDE: the file lives under the
    // TUI client's HOME and the TUI event loop matches the keypress.
    //
    // The REAL default for toggle_layout in this build is Ctrl+t (the
    // dashboard hints bar reads "Ctrl+t: layout"), NOT the Alt+t named in
    // the stale PRD body — so the old-key negative check below presses
    // Ctrl+t.
    let deck = TuiDeck::builder()
        .with_keybindings_toml(
            "[global]\n\
             toggle_layout = \"Alt+Shift+l\"\n",
        )
        .launch_with_fixture("minimal");

    deck.wait_for_string("No active sessions");

    // Alt+Shift+l: Alt sends an ESC prefix; Shift+l is the uppercase
    // `L`. One write so crossterm decodes it as a single chord.
    deck.send_keys(b"\x1bL");

    // The toggle handler sets a `Layout: stacked` / `Layout: tiled`
    // status message — capital-L-plus-colon is unique to that message
    // (the hints bar uses lowercase "layout"). Its appearance proves
    // the remapped key fired the toggle. RED today: the deck ignores
    // keybindings.toml, so Alt+Shift+l does nothing and this times out.
    deck.wait_for_string("Layout:");

    // The OLD default toggle key (Ctrl+t) must no longer toggle once the
    // action has been rebound. Press it, then a known-default sentinel
    // (`?` opens help) so we can prove both keys were processed in
    // order; a second toggle would have produced "Layout: stacked".
    deck.send_keys(b"\x14"); // Ctrl+t
    deck.send_keys(b"?"); // help (default binding, unchanged)
    deck.wait_for_string("Create new pane");
    assert!(
        !deck.snapshot_grid().contains("Layout: stacked"),
        "old default Ctrl+t still toggled the layout after the action was \
         rebound to Alt+Shift+l — client-side resolution should have \
         replaced the default binding.\nGrid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Stage a `keybindings.toml` that rebinds the dashboard
/// `help` action from `?` to `F1`, launch against the `minimal` fixture,
/// and press `F1`. The help overlay should appear (it carries the
/// "Create new pane" line).
#[spec("keybindings/remap/002")]
#[test]
fn remap_002_dashboard_action_rebind() {
    // PRD #40 catalog: keybindings/remap/002 — a config remap of a
    // DASHBOARD action (help: ? -> F1) takes effect; pressing F1 opens
    // the help overlay.
    let deck = TuiDeck::builder()
        .with_keybindings_toml(
            "[dashboard]\n\
             help = \"F1\"\n",
        )
        .launch_with_fixture("minimal");

    deck.wait_for_string("No active sessions");

    // F1 under TERM=xterm-256color is the SS3 sequence ESC O P.
    deck.send_keys(b"\x1bOP");

    // The help overlay lists "Create new pane" among the global
    // shortcuts. RED today: F1 is not bound to anything, so the overlay
    // never opens and this times out.
    deck.wait_for_string("Create new pane");
}

/// Scenario: Stage a `keybindings.toml` that tries to hijack `Ctrl+C`
/// for another action (`new_pane = "Ctrl+C"`), launch against the
/// `minimal` fixture, and press `Ctrl+C`. The quit/detach modal ("Quit
/// dot-agent-deck?") must still open — `Ctrl+C` is a non-overridable
/// safety net, so an action bound to it can never hijack it. This is a
/// guard test: it must stay green so config can never disable the
/// emergency quit. (Quit is not a configurable action — `Ctrl+C` is
/// hardcoded in the event loop — so this exercises the GLOBAL-block
/// Ctrl+C exclusion path.)
#[spec("keybindings/safety/001")]
#[test]
fn safety_001_ctrl_c_always_quits() {
    // PRD #40 catalog: keybindings/safety/001 — Ctrl+C always opens the
    // quit modal even when another action is bound to Ctrl+C. Edge case
    // from the PRD: "Ctrl+C -> always quits regardless of config".
    let deck = TuiDeck::builder()
        .with_keybindings_toml(
            "[global]\n\
             new_pane = \"Ctrl+C\"\n",
        )
        .launch_with_fixture("minimal");

    deck.wait_for_string("No active sessions");

    // Ctrl+C == 0x03.
    deck.send_keys(b"\x03");

    // The quit-confirmation modal renders "Quit dot-agent-deck?".
    deck.wait_for_string("Quit dot-agent-deck?");
}

/// Scenario: Stage a `keybindings.toml` that tries to hijack `Ctrl+C`
/// through the *tab-navigation* dispatch path by binding both
/// `move_left` and `move_right` to `Ctrl+C`, launch against the
/// `minimal` fixture, and press `Ctrl+C`. The quit/detach modal ("Quit
/// dot-agent-deck?") must still open — `Ctrl+C` is never routed through
/// the config tab-cycle path, so it can never be turned into a tab
/// switch. Regression guard for FIX2 (the `!is_ctrl_c` gate on the
/// Normal-mode move_left/move_right dispatch in src/ui.rs): before that
/// fix, `move_left = "Ctrl+C"` would have matched the tab-cycle branch
/// and consumed the keypress, so the quit modal would not open and this
/// would have been RED.
#[spec("keybindings/safety/002")]
#[test]
fn safety_002_ctrl_c_survives_tab_nav_hijack() {
    // PRD #40 catalog: keybindings/safety/002 — Ctrl+C always opens the
    // quit modal even when a TAB-NAV action (move_left / move_right) is
    // bound to Ctrl+C. Complements safety/001 (global-block path) by
    // covering the Normal-mode tab-cycle dispatch path.
    let deck = TuiDeck::builder()
        .with_keybindings_toml(
            "[dashboard]\n\
             move_left = \"Ctrl+C\"\n\
             move_right = \"Ctrl+C\"\n",
        )
        .launch_with_fixture("minimal");

    deck.wait_for_string("No active sessions");

    // Ctrl+C == 0x03.
    deck.send_keys(b"\x03");

    // The quit-confirmation modal still renders "Quit dot-agent-deck?".
    deck.wait_for_string("Quit dot-agent-deck?");
}

/// Scenario: Stage a `keybindings.toml` that unbinds `new_pane`
/// (`new_pane = ""`), launch against the `minimal` fixture, and press
/// the default `Ctrl+n`. Nothing should happen — no directory picker /
/// new-pane flow opens. We then press the default `?` (help) and wait
/// for the help overlay, proving the deck is still in Normal mode and
/// processed the keystrokes in order, before asserting the directory
/// picker never appeared.
#[spec("keybindings/unbind/001")]
#[test]
fn unbind_001_empty_binding_is_noop() {
    // PRD #40 catalog: keybindings/unbind/001 — an empty-string binding
    // unbinds the action; the default key becomes a no-op. Edge case
    // from the PRD: "Empty binding (quit = \"\") -> action is unbound".
    let deck = TuiDeck::builder()
        .with_keybindings_toml(
            "[global]\n\
             new_pane = \"\"\n",
        )
        .launch_with_fixture("minimal");

    deck.wait_for_string("No active sessions");

    // Ctrl+n == 0x0e. With new_pane unbound this must do nothing; today
    // the deck ignores the config and Ctrl+n opens the directory picker.
    deck.send_keys(b"\x0e");

    // `?` opens help only from Normal mode. If Ctrl+n was correctly a
    // no-op the deck is still in Normal mode and help opens; if the
    // directory picker had opened (RED, today), `?` is swallowed and the
    // help overlay never appears, so this times out.
    deck.send_keys(b"?");
    deck.wait_for_string("Create new pane");

    // And the directory-picker chrome must be absent.
    assert!(
        !deck.snapshot_grid().contains("Select Directory"),
        "Ctrl+n opened the directory picker even though new_pane was \
         unbound — the empty binding should have made it a no-op.\nGrid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Stage a malformed (unparseable) `keybindings.toml`, launch
/// against the `minimal` fixture, and confirm the deck still comes up
/// with default bindings working (`?` opens help) and emits a warning
/// mentioning "keybindings" on stderr (captured in the PTY byte stream).
#[spec("keybindings/fallback/001")]
#[test]
fn fallback_001_malformed_config() {
    // PRD #40 catalog: keybindings/fallback/001 — a malformed config
    // does not crash the deck: it falls back to defaults and warns on
    // stderr. Edge case from the PRD: "Malformed config -> warn on
    // stderr, fall back to defaults".
    let deck = TuiDeck::builder()
        .with_keybindings_toml(
            // Not valid TOML: dangling key, unterminated string, junk.
            "[global\n\
             quit = \n\
             === not toml ===\n",
        )
        .launch_with_fixture("minimal");

    // The deck still launches to its empty dashboard.
    deck.wait_for_string("No active sessions");

    // Default bindings still work: `?` opens the help overlay.
    deck.send_keys(b"?");
    deck.wait_for_string("Create new pane");

    // A warning mentioning "keybindings" is printed to stderr at startup
    // (merged into the PTY byte stream before the TUI clears the screen,
    // so it survives in the rolling byte history). RED today: the deck
    // never reads keybindings.toml, so no such warning is emitted and
    // this times out.
    deck.wait_for_stream_string("keybindings");
}
