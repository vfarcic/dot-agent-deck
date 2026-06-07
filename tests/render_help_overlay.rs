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

use dot_agent_deck::ui::render_help_overlay_to_buffer;
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
/// advertises (New Pane Ctrl+N, Close Ctrl+W, Toggle Layout Ctrl+T, Help ?,
/// Quit Ctrl+C) and the key dashboard / navigation actions (filter `/`,
/// rename, generate, tab switching, card nav). Substring checks are
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
        ("ctrl+n", "New Pane (Ctrl+N)"),
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
}
