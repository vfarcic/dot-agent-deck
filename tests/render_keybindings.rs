//! L1 widget / layout snapshot tests for the keybinding-aware renderers
//! (PRD #40 — Customizable Keybindings).
//!
//! Per PRD #77 Decision 2 these are in-process tests using ratatui's
//! `TestBackend` plus `insta` file snapshots — no subprocess, no PTY.
//! They mirror `tests/render_dashboard.rs::pane_004_card_title_row`:
//! build an in-memory state, render it into a `Buffer`, and snapshot the
//! stringified buffer.
//!
//! These exercise two render entrypoints — `render_help_overlay_to_buffer`
//! and `render_hints_bar_to_buffer` in `dot_agent_deck::ui`. Both must
//! generate their content from the *active* `KeybindingConfig` (not from
//! hardcoded strings), which these tests prove by remapping bindings and
//! asserting the custom key notation appears in the rendered output. They
//! were authored RED (the render fns did not exist yet) and went GREEN
//! once the renderers landed.

use dot_agent_deck::keybindings::{Action, KeybindingConfig, parse_binding};
use dot_agent_deck::theme::{ColorPalette, Theme, resolve_palette};
use dot_agent_deck::ui::{
    render_button_bar_with_bindings_to_buffer, render_help_overlay_with_bindings_to_buffer,
    render_hints_bar_to_buffer,
};
use spec::spec;

/// Stringify the rendered buffer — one line per row, cells joined into
/// the symbol layer — so `insta` diffs read like the rendered widget
/// itself. Mirrors the same helper in `tests/render_dashboard.rs`.
fn buffer_to_text(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area();
    let mut out = String::with_capacity((area.width as usize + 1) * area.height as usize);
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// A `KeybindingConfig` with two actions remapped away from their
/// defaults: the global `toggle_layout` (`Ctrl+t` → `Alt+Shift+l`) and
/// the dashboard `help` (`?` → `F1`). Both renderers should reflect
/// these custom notations.
fn remapped_config() -> KeybindingConfig {
    let mut c = KeybindingConfig::default();
    c.set(
        Action::ToggleLayout,
        parse_binding("Alt+Shift+l").expect("valid notation"),
    );
    c.set(Action::Help, parse_binding("F1").expect("valid notation"));
    c
}

/// Scenario: Build a `KeybindingConfig` with `toggle_layout` remapped to
/// `Alt+Shift+l` and `help` remapped to `F1`, render the help overlay
/// against that config into a `TestBackend` buffer, and snapshot it. The
/// rendered overlay must show the CUSTOM key strings (`Alt+Shift+l`,
/// `F1`) rather than the defaults — proving the overlay is generated from
/// the active keybinding config, not from hardcoded strings.
#[spec("keybindings/help/001")]
#[test]
fn help_001_overlay_reflects_active_bindings() {
    // PRD #40 catalog: keybindings/help/001 — help overlay rendered
    // against a remapped config shows the custom keys (dynamic
    // generation). dashboard/help/002 remains the defaults-content guard.
    let config = remapped_config();
    let palette: ColorPalette = resolve_palette(Theme::Dark);

    // Full default-ish viewport so the centered overlay popup is not
    // clipped (120×44 comfortably fits the help columns + footer).
    let width: u16 = 120;
    let height: u16 = 44;
    let buffer = render_help_overlay_with_bindings_to_buffer(&config, None, palette, width, height);

    let text = buffer_to_text(&buffer);
    assert!(
        text.contains("Alt+Shift+l"),
        "help overlay must render the remapped toggle_layout key \
         (Alt+Shift+l); overlay was generated from hardcoded strings?\n{text}"
    );
    assert!(
        text.contains("F1"),
        "help overlay must render the remapped help key (F1); overlay was \
         generated from hardcoded strings?\n{text}"
    );
    insta::assert_snapshot!(text);
}

/// Scenario: Build the same remapped `KeybindingConfig` (`toggle_layout`
/// → `Alt+Shift+l`), render the dashboard hints bar against it into a
/// `TestBackend` buffer, and snapshot it. The hints bar must show the
/// custom key for the layout-toggle action rather than the default
/// `Ctrl+t` — proving the hints bar is generated from the active config.
#[spec("keybindings/hints/001")]
#[test]
fn hints_001_bar_reflects_active_bindings() {
    // PRD #40 catalog: keybindings/hints/001 — hints bar rendered against
    // a remapped config shows the custom keys (dynamic generation).
    let config = remapped_config();
    let palette: ColorPalette = resolve_palette(Theme::Dark);

    // Single-row hints bar at the default 120-column width.
    let width: u16 = 120;
    let height: u16 = 1;
    let buffer = render_hints_bar_to_buffer(&config, palette, width, height);

    let text = buffer_to_text(&buffer);
    assert!(
        text.contains("Alt+Shift+l"),
        "hints bar must render the remapped toggle_layout key \
         (Alt+Shift+l); hints were generated from hardcoded strings?\n{text}"
    );
    insta::assert_snapshot!(text);
}

/// Scenario: Build a default `KeybindingConfig` and unbind `new_pane`
/// (`set(NewPane, parse_binding("").unwrap())`), then render the hints
/// bar against it. An unbound action has an empty notation, so the hints
/// bar must substitute `(unbound)` for its key (matching the help
/// overlay's behaviour) and render `(unbound): new` — never a bare
/// `: new` with an empty key column. Asserts on the buffer text directly
/// (no `insta` snapshot) so this guard needs no `.snap` accept step.
/// Greptile P2 regression guard.
#[spec("keybindings/hints/002")]
#[test]
fn hints_002_unbound_action_not_bare() {
    // PRD #40 catalog: keybindings/hints/002 — an unbound action renders
    // as `(unbound)` in the hints bar, never as a bare `: <label>`.
    let mut config = KeybindingConfig::default();
    config.set(
        Action::NewPane,
        parse_binding("").expect("empty == unbound"),
    );
    let palette: ColorPalette = resolve_palette(Theme::Dark);

    let buffer = render_hints_bar_to_buffer(&config, palette, 120, 1);
    let text = buffer_to_text(&buffer);
    let line = text.lines().next().unwrap_or("");

    assert!(
        text.contains("(unbound)"),
        "unbound new_pane must render as '(unbound): new', not a bare key. \
         Hints bar text was:\n{line:?}"
    );
    // The bare artifact for the first (new_pane) slot is a line that
    // begins with ': new'; a mid-string empty slot would show '  : '.
    assert!(
        !line.trim_start().starts_with(": "),
        "hints bar starts with a bare ': <label>' (empty key column) — \
         unbound new_pane was not substituted with '(unbound)'. Line:\n{line:?}"
    );
    assert!(
        !text.contains("  : "),
        "hints bar contains a bare '  : <label>' (empty key column) for some \
         unbound action. Text:\n{line:?}"
    );
}

/// Scenario: Build a default `KeybindingConfig`, remap `new_pane` to
/// `Alt+P` and `help` to `F1`, then render the prd-80 button bar against
/// it. The bar's labels are derived from the active config, so the
/// New-pane button must show the remapped `Alt+P` (never the default
/// `Ctrl+N`) and the Help button must show `F1`. Asserts on the buffer
/// text directly (no `insta` snapshot) so this guard needs no `.snap`
/// accept step. Guards against a future refactor silently re-hardcoding
/// the button labels.
#[spec("keybindings/buttons/001")]
#[test]
fn buttons_001_bar_reflects_active_bindings() {
    // PRD #40 catalog: keybindings/buttons/001 — the button bar labels
    // track the active KeybindingConfig (remapped key shown, default not).
    let mut config = KeybindingConfig::default();
    config.set(
        Action::NewPane,
        parse_binding("Alt+P").expect("valid notation"),
    );
    config.set(Action::Help, parse_binding("F1").expect("valid notation"));
    let palette: ColorPalette = resolve_palette(Theme::Dark);

    // Width wide enough that the New-pane and Help buttons render in full.
    let buffer = render_button_bar_with_bindings_to_buffer(&config, palette, 200, 1);
    let text = buffer_to_text(&buffer);

    assert!(
        text.contains("Alt+P"),
        "button bar must show the remapped New-pane key (Alt+P); labels were \
         hardcoded?\n{text}"
    );
    assert!(
        !text.contains("Ctrl+N"),
        "button bar still shows the default New-pane key (Ctrl+N) after the \
         action was remapped to Alt+P — labels are not config-derived.\n{text}"
    );
    assert!(
        text.contains("F1"),
        "button bar must show the remapped Help key (F1); labels were \
         hardcoded?\n{text}"
    );
}
