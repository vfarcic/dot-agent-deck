#![cfg(feature = "e2e")]

//! PRD #84 M1 — L2 reproducers for the rendering-contract failure modes that
//! only manifest through the full spawned-binary layout/resize pipeline.
//!
//! Each test spawns the real `dot-agent-deck` binary inside an isolated PTY,
//! drives a layout-changing event (terminal enlarge, layout toggle, pane
//! close, mode switch), and asserts a render-contract invariant on the
//! settled grid through the `vt100` parser. No LLM tokens are spent — panes
//! run `sleep` or open the fixture's empty `demo` mode pane.
//!
//! IMPORTANT (PRD #84, and the PRD's own "race-y resize timing" note): these
//! reproducers were written against the PRE-M4 (pre-rework) code, which
//! resized every embedded pane's PTY on *every* layout-change path
//! (`Event::Resize`, `Action::ToggleLayout` → `resize_*_panes`, tab
//! open/close, mode switch). In that pre-rework state the scramble /
//! empty-band symptoms were transient one-frame races that self-healed once
//! the path's resize fired, and so were NOT deterministically observable
//! through a PTY+vt100 harness that reads the settled grid. These tests are
//! therefore written as **invariant guards**: they exercise the real path and
//! assert the contract property the settled frame must satisfy. They flagged
//! (passed) against the pre-M4 code and pin the invariant for the M3/M4/M5
//! contract work — a regression that leaves the frame in the broken state
//! turns the guard RED. The deterministic widget-level RED for the same
//! defect class lives in `tests/render_terminal_widget.rs`.
//!
//! Decision 6: gated behind the `e2e` feature so `cargo test-fast` never
//! compiles it.

mod common;

use common::TuiDeck;
use spec::spec;

/// Open a Mode tab (mirrors `e2e_mouse_tabstrip::open_second_tab`): Ctrl+N →
/// directory picker → choose current dir → new-pane form → pick the fixture's
/// `demo` mode → submit. Synchronizes on observable screen state at each step.
/// Leaves the deck with ≥2 tabs (Dashboard + the active Mode tab), so the tab
/// strip's `Dashboard` header is rendered.
fn open_mode_tab(deck: &TuiDeck) {
    deck.wait_for_string("No active sessions");
    deck.send_bytes(b"\x0e"); // Ctrl+N → directory picker
    deck.wait_for_string("Select Directory");
    deck.send_bytes(b" "); // Space: choose current dir → new-pane form
    deck.wait_for_string("Mode:"); // form ready (Mode field present)
    deck.send_bytes(b"\x1b[C"); // Right: move Mode selection off "No mode" to `demo`
    deck.wait_for_string("demo mode"); // selection reflected in the title
    let (scol, srow) = deck
        .find_in_grid("[Submit]")
        .expect("new-pane form should render a [Submit] button");
    deck.click(scol, srow);
    deck.wait_for_string("Dashboard"); // tab strip appears only with ≥2 tabs
}

/// Widest rendered row, in display columns. `vt100::Screen::contents` trims
/// trailing blanks per row, so a row that reaches the right edge (a pane
/// border, the bottom bar) keeps its full length — this is how we observe
/// that the frame filled the terminal width rather than leaving an unfilled
/// band near the old width.
fn max_row_width(grid: &str) -> usize {
    grid.lines().map(|l| l.chars().count()).max().unwrap_or(0)
}

/// Scenario: Launch the deck at 80×24 with a `--continue` pane running
/// `sleep 600`, which auto-focuses (PaneInput) so its bordered `TerminalWidget`
/// renders nearly full-screen (bottom bar shows `[Detach Ctrl+D]`). Enlarge the
/// outer terminal to 120×24 (W → W+40), driving the `Event::Resize` path. The
/// next settled frame must fill the new width — a rendered row (the pane border
/// or the bottom bar) reaches close to column 120 — with no band of unfilled
/// columns leaving the frame stuck near the old 80-col width. Invariant guard:
/// against the PRE-M4 (pre-rework) code the empty-band symptom was a transient
/// pre-resize-propagation race that self-healed, so this pins the post-resize
/// "fills the new width" contract (extends the `resize/sigwinch` area;
/// goes/stays GREEN at M4).
#[spec("resize/render/001")]
#[test]
fn render_001_enlarge_fills_new_width() {
    let mut deck = TuiDeck::builder()
        .with_pty_size(80, 24)
        .with_continue_session("rp", "sleep 600")
        .launch_with_fixture("minimal");
    // --continue auto-focuses the restored pane: its TerminalWidget renders
    // nearly full-screen and the bottom bar shows [Detach Ctrl+D].
    deck.wait_for_string("[Detach Ctrl+D]");

    // Enlarge the outer terminal by 40 columns. The Event::Resize handler must
    // recompute layout and re-render to the new width.
    deck.resize(120, 24);

    // Invariant: the settled frame fills the enlarged width — some row reaches
    // close to column 120 (>=110 tolerates inner padding), proving the frame
    // reflowed rather than staying stuck near the old 80-col extent.
    deck.wait_until_grid("frame reflowed to the enlarged 120-col width", |g| {
        max_row_width(g) >= 110
    });
}

/// Scenario: Launch the deck at 120×32 with a `--continue` pane running
/// `sleep 600`, detach to the dashboard (Ctrl+D → Normal mode), and confirm
/// the restored No-agent pane's card body (`Launch an agent`) is rendered.
/// Toggle the dashboard pane layout with `Ctrl+t` (stacked ↔ tiled), waiting
/// for the `Layout:` status to acknowledge the toggle. After the layout change
/// the pane's representation must stay intact — its card body is still
/// rendered, not dropped and not overwritten by a stale fragment of the
/// pre-toggle layout. Invariant guard (riskiest catalog entry): the bottom-row
/// scramble the PRD describes is transient (the toggle path resizes panes via
/// `resize_*_panes`), so this pins the observable "pane survives a layout
/// change intact" invariant; GREEN target at M4/M5.
#[spec("render/layout/001")]
#[test]
fn layout_001_toggle_layout_keeps_pane_intact() {
    // PRD #127: 200 cols so the Normal-mode bar renders the FULL labeled
    // `[New Pane Ctrl+N]` (at the default 120 it collapses to `[Ctrl+N]`
    // chips once the always-shown Scheduled Tasks button is included).
    let deck = TuiDeck::builder()
        .with_pty_size(200, 40)
        .with_continue_session("rp", "sleep 600")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Detach Ctrl+D]");
    deck.send_bytes(b"\x04"); // Ctrl+D → dashboard / Normal mode
    deck.wait_for_string("[New Pane Ctrl+N]");
    // The restored No-agent pane's card body anchors the assertion.
    deck.wait_for_string("Launch an agent");

    // Toggle the dashboard pane layout.
    deck.send_bytes(b"\x14"); // Ctrl+t → ToggleLayout
    deck.wait_for_string("Layout:"); // toggle acknowledged

    // Invariant: the pane card body is still rendered after the layout change.
    deck.wait_until_grid("pane card intact after layout toggle", |g| {
        g.contains("Launch an agent")
    });
}

/// Scenario: Launch the deck against the `modes` fixture, open a Mode tab
/// (creating an embedded `demo`-mode pane), then close that tab with `Ctrl+W`,
/// tearing the pane down and returning to a lone Dashboard. This exercises the
/// pane open/close + reactive-recreation path. After the replace the dashboard
/// must render cleanly: the tab strip collapses (no `×` close glyph remains)
/// and no stale `demo mode` fragment from the closed pane lingers anywhere on
/// screen. Invariant guard: scrambled fragments after a pane replace are
/// transient (open/close resizes the affected PTYs on the spot), so this pins
/// the "no stale fragment after replace" invariant; GREEN target at M4/M5.
#[spec("render/layout/002")]
#[test]
fn layout_002_pane_close_leaves_no_stale_fragment() {
    let deck = TuiDeck::builder()
        .with_pty_size(120, 32)
        .launch_with_fixture("modes");
    open_mode_tab(&deck);

    // Close the active Mode tab → its pane is torn down (== the click-to-close
    // path covered by mouse/tabstrip/002).
    deck.send_bytes(b"\x17"); // Ctrl+W → close tab

    // Invariant: the tab strip collapses and no stale mode fragment remains.
    deck.wait_for_absence("×");
    deck.wait_until_grid("no stale mode fragment after pane close", |g| {
        !g.contains("demo mode")
    });
}

/// Scenario: Launch the deck against the `modes` fixture and open a Mode tab,
/// switching the active view through the `render_mode_tab` path. After the
/// transition settles the destination mode view must render cleanly — the tab
/// strip's `Dashboard` header is present, and the dashboard-only empty-state
/// line (`No active sessions`, never shown on a Mode tab) is NOT bleeding
/// through from the source layout. Invariant guard: short-lived mode-switch
/// artefacts are transient (the switch resizes panes via
/// `resize_mode_tab_panes`), so this pins the "destination renders cleanly,
/// no source bleed-through" invariant; GREEN target at M4/M5.
#[spec("render/layout/003")]
#[test]
fn layout_003_mode_switch_renders_cleanly() {
    let deck = TuiDeck::builder()
        .with_pty_size(120, 32)
        .launch_with_fixture("modes");
    open_mode_tab(&deck);

    // Invariant: the Mode tab view is clean — tab strip present, no dashboard
    // empty-state line bleeding through from the pre-switch layout.
    deck.wait_until_grid("mode view renders without dashboard bleed-through", |g| {
        g.contains("Dashboard") && !g.contains("No active sessions")
    });
}
