//! L1 widget / layout snapshot tests for the keybinding-aware renderers
//! (PRD #40 — Customizable Keybindings).
//!
//! Per PRD #77 Decision 2 these are in-process tests using ratatui's
//! `TestBackend` plus `insta` file snapshots — no subprocess, no PTY.
//! They mirror `tests/render_dashboard.rs::pane_004_card_title_row`:
//! build an in-memory state, render it into a `Buffer`, and snapshot the
//! stringified buffer.
//!
//! These exercise the help overlay and the same `render_bottom_bar` path the
//! running app draws. Both must generate their content from the *active*
//! `KeybindingConfig` (not from hardcoded strings), which these tests prove by
//! remapping bindings and asserting the custom key notation appears in the
//! rendered output.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use dot_agent_deck::keybindings::{Action as KbAction, KeybindingConfig, parse_binding};
use dot_agent_deck::ui::{
    Action as UiAction, UiMode, key_action_for_mode, render_button_bar_for_mode_to_buffer,
    render_button_bar_with_bindings_to_buffer, render_help_overlay_with_bindings_to_buffer,
};
use ratatui::style::Modifier;
use spec::spec;

type UiActionCase = (char, &'static str, fn(&UiAction) -> bool);

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

fn span_has_modifier(buffer: &ratatui::buffer::Buffer, needle: &str, modifier: Modifier) -> bool {
    let area = buffer.area();
    for y in 0..area.height {
        let line = (0..area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>();
        if let Some(byte_start) = line.find(needle) {
            let start = line[..byte_start].chars().count() as u16;
            return (start..start + needle.chars().count() as u16)
                .all(|x| buffer[(x, y)].modifier.contains(modifier));
        }
    }
    false
}

/// A `KeybindingConfig` with two actions remapped away from their
/// defaults: the global `toggle_layout` (`Ctrl+t` → `Alt+Shift+l`) and
/// the dashboard `help` (`?` → `F1`). Both renderers should reflect
/// these custom notations.
fn remapped_config() -> KeybindingConfig {
    let mut c = KeybindingConfig::default();
    c.set(
        KbAction::ToggleLayout,
        parse_binding("Alt+Shift+l").expect("valid notation"),
    );
    c.set(KbAction::Help, parse_binding("F1").expect("valid notation"));
    c
}

/// Scenario: Build a `KeybindingConfig` with `toggle_layout` remapped to
/// `Alt+Shift+l` and `help` remapped to `F1`, render the help overlay
/// against that config into a `TestBackend` buffer, and snapshot it. The
/// rendered overlay must show the CUSTOM key strings (`Alt+Shift+l`,
/// `F1`) rather than the defaults and describe Ctrl+D as a command/pane
/// toggle — proving the overlay is generated from the active config without
/// retaining the stale one-way help text.
#[spec("keybindings/help/001")]
#[test]
fn help_001_overlay_reflects_active_bindings() {
    // PRD #40 catalog: keybindings/help/001 — help overlay rendered
    // against a remapped config shows the custom keys (dynamic
    // generation). dashboard/help/002 remains the defaults-content guard.
    let config = remapped_config();

    // Full default-ish viewport so the centered overlay popup is not
    // clipped (120×44 comfortably fits the help columns + footer).
    let width: u16 = 120;
    let height: u16 = 44;
    let buffer = render_help_overlay_with_bindings_to_buffer(&config, None, width, height);

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
    assert!(
        text.contains("Toggle command / pane"),
        "Ctrl+D help must describe the dashboard/pane-input transition as a toggle\n{text}"
    );
    assert!(
        !text.contains("Command mode (dashboard)"),
        "the one-way Ctrl+D help copy must not survive\n{text}"
    );
    insta::assert_snapshot!(text);
}

/// Scenario: Build the same remapped `KeybindingConfig` (`toggle_layout` → `Alt+Shift+l`), render the production dashboard button bar against it, and snapshot it. The live bar must show the custom layout key and command mode's `[Back to Pane Ctrl+D]` affordance.
#[spec("keybindings/hints/001")]
#[test]
fn hints_001_bar_reflects_active_bindings() {
    let config = remapped_config();

    let buffer = render_button_bar_with_bindings_to_buffer(&config, 200, 2);

    let text = buffer_to_text(&buffer);
    assert!(
        text.contains("Alt+Shift+L"),
        "the live button bar must render the remapped toggle_layout key\n{text}"
    );
    assert!(
        text.contains("[Back to Pane Ctrl+D]"),
        "the live command-mode bar must tell the user how to return to the pane\n{text}"
    );
    insta::assert_snapshot!(text);
}

/// Scenario: Unbind `new_pane` in a default keybinding config and render the production button bar. The live New Pane button must show `(unbound)` rather than an empty shortcut field.
#[spec("keybindings/hints/002")]
#[test]
fn hints_002_unbound_action_not_bare() {
    let mut config = KeybindingConfig::default();
    config.set(
        KbAction::NewPane,
        parse_binding("").expect("empty == unbound"),
    );

    let buffer = render_button_bar_with_bindings_to_buffer(&config, 200, 2);
    let text = buffer_to_text(&buffer);

    assert!(
        text.contains("[New Pane (unbound)]"),
        "the live New Pane button must render an explicit unbound shortcut\n{text}"
    );
    assert!(
        !text.contains("[New Pane ]"),
        "the live bar must not leave the New Pane shortcut blank\n{text}"
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
        KbAction::NewPane,
        parse_binding("Alt+P").expect("valid notation"),
    );
    config.set(KbAction::Help, parse_binding("F1").expect("valid notation"));

    // Width wide enough that the New-pane and Help buttons render in full.
    let buffer = render_button_bar_with_bindings_to_buffer(&config, 200, 1);
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

/// Scenario: Resolve Ctrl+W once in PaneInput and once in command mode through the production key mapper. PaneInput must forward byte `0x17` to the PTY, while command mode must still request the selected-pane close path.
#[spec("keybindings/safety/003")]
#[test]
fn safety_003_ctrl_w_is_forwarded_only_in_pane_input() {
    let config = KeybindingConfig::default();
    let ctrl_w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);

    match key_action_for_mode(&config, UiMode::PaneInput, &ctrl_w) {
        Some(UiAction::ForwardToPane(bytes)) => assert_eq!(
            bytes,
            vec![0x17],
            "PaneInput Ctrl+W must reach the PTY as the readline word-delete byte"
        ),
        other => panic!("PaneInput Ctrl+W must fall through to PTY forwarding, got {other:?}"),
    }
    assert!(
        matches!(
            key_action_for_mode(&config, UiMode::Normal, &ctrl_w),
            Some(UiAction::CloseSelected)
        ),
        "command-mode Ctrl+W must still resolve the close request"
    );
}

/// Scenario: Resolve the other three global commands while PaneInput owns the keyboard. Ctrl+D, Ctrl+N, and Ctrl+T must retain their existing dashboard, new-pane, and layout actions; only Ctrl+W becomes mode-scoped.
#[spec("keybindings/safety/004")]
#[test]
fn safety_004_other_global_actions_remain_available_in_pane_input() {
    let config = KeybindingConfig::default();
    let cases: [UiActionCase; 3] = [
        ('d', "dashboard", |a| matches!(a, UiAction::DetachToNormal)),
        ('n', "new pane", |a| matches!(a, UiAction::NewPane)),
        ('t', "toggle layout", |a| {
            matches!(a, UiAction::ToggleLayout)
        }),
    ];

    for (ch, label, expected) in cases {
        let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL);
        let action = key_action_for_mode(&config, UiMode::PaneInput, &key)
            .unwrap_or_else(|| panic!("PaneInput must still resolve the global {label} action"));
        assert!(
            expected(&action),
            "PaneInput Ctrl+{ch} resolved the wrong {label} action: {action:?}"
        );
    }
}

/// Scenario: Parse an existing `[global] close_pane = "Ctrl+x"` configuration and resolve its custom chord in both modes. Command mode must request close while PaneInput forwards Ctrl+X to the PTY, proving the key stayed in the compatible `[global]` table even though dispatch became mode-aware.
#[spec("keybindings/remap/003")]
#[test]
fn remap_003_global_close_binding_survives_mode_gating() {
    let (config, warnings) = KeybindingConfig::from_toml_str(
        "[global]\n\
         close_pane = \"Ctrl+x\"\n",
    )
    .expect("the existing [global] close_pane config must remain valid");
    assert!(
        warnings.is_empty(),
        "compatible config warned: {warnings:?}"
    );
    let ctrl_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);

    assert!(
        matches!(
            key_action_for_mode(&config, UiMode::Normal, &ctrl_x),
            Some(UiAction::CloseSelected)
        ),
        "the custom [global] close chord must request close in command mode"
    );
    match key_action_for_mode(&config, UiMode::PaneInput, &ctrl_x) {
        Some(UiAction::ForwardToPane(bytes)) => assert_eq!(bytes, vec![0x18]),
        other => panic!(
            "the custom close chord must remain ordinary PTY input in PaneInput, got {other:?}"
        ),
    }
}

/// Scenario: Render the exact production bottom-bar path in command mode, Help, and PaneInput using the same bindings. Command mode must show enabled Back-to-Pane and Close buttons, Help must render Close dimmed while the mode mapper keeps it inert, and PaneInput must show only the Command Mode return affordance.
#[spec("keybindings/hints/003")]
#[test]
fn hints_003_bar_reflects_mode_scoped_close() {
    let config = KeybindingConfig::default();
    let normal_buffer = render_button_bar_for_mode_to_buffer(&config, UiMode::Normal, 200, 2);
    let help_buffer = render_button_bar_for_mode_to_buffer(&config, UiMode::Help, 200, 2);
    let pane_input_buffer =
        render_button_bar_for_mode_to_buffer(&config, UiMode::PaneInput, 200, 2);
    let normal = buffer_to_text(&normal_buffer);
    let help = buffer_to_text(&help_buffer);
    let pane_input = buffer_to_text(&pane_input_buffer);

    assert!(normal.contains("[Back to Pane Ctrl+D]"), "{normal}");
    assert!(normal.contains("[Close Ctrl+W]"), "{normal}");
    assert!(
        !span_has_modifier(&normal_buffer, "[Close Ctrl+W]", Modifier::DIM),
        "command-mode Close must be enabled\n{normal}"
    );
    assert!(help.contains("[Command Mode Ctrl+D]"), "{help}");
    assert!(help.contains("[Close Ctrl+W]"), "{help}");
    assert!(
        span_has_modifier(&help_buffer, "[Close Ctrl+W]", Modifier::DIM),
        "Close must render dimmed outside command mode\n{help}"
    );
    let ctrl_w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
    assert!(
        key_action_for_mode(&config, UiMode::Help, &ctrl_w).is_none(),
        "the dimmed Help-mode Close affordance must be inert"
    );
    assert!(pane_input.contains("[Command Mode Ctrl+D]"), "{pane_input}");
    assert!(!pane_input.contains("[Close Ctrl+W]"), "{pane_input}");
}
