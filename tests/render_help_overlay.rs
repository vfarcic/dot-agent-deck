//! PRD #80 M10 — L1 widget test that the `?` help overlay documents the
//! current canonical shortcut set (the same commands the M2/M4 button bar
//! advertises, plus the dashboard / navigation keys).
//!
//! Per PRD #77 Decision 2 this is an in-process test driving the production
//! `render_help_overlay` through the `render_help_overlay_to_buffer` seam
//! (TestBackend wrapper, added in M5). No subprocess, no PTY. The overlay is
//! the canonical reference, so it must stay in sync with the button bar
//! (PRD #80 risk row: "help overlay drifts out of sync with the set of
//! buttons"). Assertions are robust substring checks on shortcut tokens /
//! documented command words (case-insensitive), NOT a full-screen snapshot,
//! so they pin "these shortcuts are documented" without being brittle to
//! layout or label wording.

use dot_agent_deck::keybindings::KeybindingConfig;
use dot_agent_deck::ui::{
    render_help_overlay_to_buffer, render_help_overlay_with_bindings_to_buffer,
};
use spec::spec;

/// Flatten the rendered buffer to one lowercased string of cell symbols
/// (rows joined), for case-insensitive substring assertions.
fn buffer_text_lower(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area();
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out.to_lowercase()
}

/// Scenario: Render the `?` help overlay into a tall (110×60) `TestBackend`
/// buffer so all of its multi-section content fits, then assert it documents
/// the canonical shortcut set: the five global commands the button bar
/// advertises (New Agent Ctrl+N, Close Ctrl+W, Toggle Layout Ctrl+T, Help ?,
/// Quit Ctrl+C) and the key dashboard / navigation actions (filter `/`,
/// rename, generate, tab switching, card nav). The labels must name agents
/// and the schedule manager consistently. Ctrl+D is documented under Global as
/// "Toggle command / pane". The Mouse column says a drag selects and copies
/// and a click focuses a pane, and the dashboard keys list copy mode under its
/// bound key, `v` by default and the remapped key when `copy_mode` is rebound.
/// Substring checks are
/// case-insensitive so a `Ctrl+n` vs `Ctrl+N` casing difference between the
/// overlay and the buttons does not fail the test — it pins that each
/// shortcut/command is *present* in the canonical reference.
#[spec("mouse/help/001")]
#[test]
fn help_001_overlay_documents_canonical_shortcut_set() {
    let buf = buffer_text_lower(&render_help_overlay_to_buffer(110, 60));

    // Each entry: (token, human description for the failure message).
    let required: &[(&str, &str)] = &[
        // Five global commands the M2 button bar advertises.
        ("ctrl+n", "New Agent (Ctrl+N)"),
        ("ctrl+w", "Close (Ctrl+W)"),
        ("ctrl+t", "Toggle Layout (Ctrl+T)"),
        ("ctrl+c", "Quit (Ctrl+C)"),
        ("?", "Help (?)"),
        // Dashboard context commands (M4 buttons) + their keys.
        ("/", "Filter (/)"),
        ("rename", "Rename (r)"),
        ("generate", "Generate-config (g)"),
        // Navigation: tab switching and card navigation.
        ("ctrl+pgdn", "Next tab (Ctrl+PgDn)"),
        ("select next", "Card navigation (j/k)"),
    ];

    let missing: Vec<&str> = required
        .iter()
        .filter(|(token, _)| !buf.contains(token))
        .map(|(_, desc)| *desc)
        .collect();

    assert!(
        missing.is_empty(),
        "help overlay is missing these canonical shortcuts/commands: {missing:?}\n--- rendered overlay ---\n{buf}"
    );
    for label in [
        "create new agent",
        "filter agents",
        "rename agent",
        "jump to card n",
        "close selected agent (confirms)",
        "schedules manager",
    ] {
        assert!(
            buf.contains(label),
            "help overlay must show {label:?}\n{buf}"
        );
    }

    // PRD #385 M7: the copy gestures.
    let row_with = |buf: &str, needle: &str| -> String {
        buf.lines()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("help overlay must show {needle:?}\n{buf}"))
            .to_string()
    };
    for (key, label) in [("drag", "select & copy"), ("mouse click", "focus pane")] {
        let row = row_with(&buf, label);
        assert!(
            row.contains(key),
            "the {label:?} row must name {key:?}: {row:?}\n{buf}"
        );
    }
    let copy_row = row_with(&buf, "copy mode");
    assert!(
        copy_row.split_whitespace().any(|word| word == "v"),
        "help must show the default v copy-mode key: {copy_row:?}\n{buf}"
    );
    let (config, warnings) =
        KeybindingConfig::from_toml_str("[dashboard]\ncopy_mode = \"x\"\n").expect("parses");
    assert!(warnings.is_empty(), "{warnings:?}");
    let remapped = buffer_text_lower(&render_help_overlay_with_bindings_to_buffer(
        &config, 110, 60,
    ));
    let copy_row = row_with(&remapped, "copy mode");
    assert!(
        copy_row.split_whitespace().any(|word| word == "x")
            && !copy_row.split_whitespace().any(|word| word == "v"),
        "help must show the remapped copy-mode key: {copy_row:?}\n{remapped}"
    );
}

/// Scenario: Render the default help overlay into a `TestBackend` buffer and snapshot its complete text. The Ctrl+D row must describe a bidirectional command-mode / pane-input toggle, and the dashboard shortcuts must advertise the `m` host-metrics overlay.
#[spec("dashboard/help/002")]
#[test]
fn help_002_overlay_documents_ctrl_d_toggle() {
    let buf = buffer_text_lower(&render_help_overlay_to_buffer(110, 60));

    let host_row = buf
        .lines()
        .find(|line| line.contains("host"))
        .unwrap_or_else(|| panic!("help must document the host-metrics overlay\n{buf}"));
    assert!(
        host_row.split_whitespace().any(|word| word == "m"),
        "help must show the default m host-metrics key\n{buf}"
    );
    insta::assert_snapshot!(buf);
    assert!(
        buf.contains("toggle command / pane"),
        "Ctrl+D help must describe a bidirectional toggle, not only the dashboard destination\n--- rendered overlay ---\n{buf}"
    );
    assert!(
        !buf.contains("command mode (dashboard)"),
        "the stale one-way Ctrl+D description must be removed\n--- rendered overlay ---\n{buf}"
    );
}

/// Scenario: Render the dashboard help overlay and locate the row for opening
/// the selected agent's pull request. That row advertises the `o` shortcut.
#[spec("dashboard/help/003")]
#[test]
fn help_003_overlay_documents_open_pull_request() {
    let buf = buffer_text_lower(&render_help_overlay_to_buffer(110, 60));
    assert!(
        buf.lines().any(|row| {
            let words: Vec<_> = row.split_whitespace().collect();
            words.contains(&"o")
                && row.contains("open")
                && (row.contains("pull request") || words.contains(&"pr"))
        }),
        "dashboard help must advertise o to open the selected pull request:\n{buf}"
    );
}
