//! PRD #80 M8 — L1 widget test for the new-pane-form mouse affordances.
//!
//! Per PRD #77 Decision 2 this is an in-process test driving the production
//! `render_new_pane_form` through the `render_new_pane_form_to_buffer` seam
//! (TestBackend wrapper mirroring `render_button_bar_to_buffer`). No
//! subprocess, no PTY. File-layout-mirrors-catalog (Decision 7): catalog ID
//! `mouse/form/001`'s render half lands here; the click→outcome half lives in
//! `tests/e2e_mouse_form.rs`.
//!
//! M8 contract: the form gains clickable mode chips (one per option) and
//! `[Submit]` / `[Cancel]` buttons, rendered ALONGSIDE the existing field
//! rows. The bracketed button labels and the per-mode chip labels are
//! distinct from the field labels, so their substrings prove the new
//! affordances while the field labels prove the form chrome still renders.

use dot_agent_deck::ui::render_new_pane_form_to_buffer;
use spec::spec;

/// Flatten the rendered buffer to one string of cell symbols (rows joined).
fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area();
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// Scenario: Render the new-pane form (with two modes, `demo` and `demo2`)
/// into an 80×24 `TestBackend` buffer. The form must render a clickable chip
/// for each mode option (`demo`, `demo2`) and `[Submit]` / `[Cancel]`
/// buttons, AND still render its existing field chrome (the `Name:` field and
/// the ` New Agent ` title). RED until M8 adds the chips + buttons: today the
/// form shows only the single currently-selected mode in a `◀ … ▶` cycler
/// (so the non-selected chip labels are absent) and has no Submit/Cancel
/// buttons.
#[spec("mouse/form/001")]
#[test]
fn form_001_renders_mode_chips_and_submit_cancel() {
    let buf = buffer_text(&render_new_pane_form_to_buffer(&["demo", "demo2"], 80, 24));

    // Existing form chrome still present (affordances are additive).
    assert!(
        buf.contains("New Agent"),
        "form must still render its title, got:\n{buf}"
    );
    assert!(
        buf.contains("Name:"),
        "form must still render its Name field, got:\n{buf}"
    );

    // Clickable mode chips — one per mode option.
    for chip in ["demo", "demo2"] {
        assert!(
            buf.contains(chip),
            "form must render a clickable {chip} mode chip, got:\n{buf}"
        );
    }

    // Submit / Cancel buttons.
    for btn in ["[Submit]", "[Cancel]"] {
        assert!(
            buf.contains(btn),
            "form must render the {btn} button, got:\n{buf}"
        );
    }
}

/// Scenario: Render the new-pane form into an 80×24 `TestBackend` buffer. PRD
/// #170 M2.2 adds an agent-command picker, visible by default, so users can pick
/// `claude` / `opencode` / a path / a custom command. Assert the form renders
/// both the `claude` and `opencode` agent presets (the picker) AND still shows
/// the pre-filled Command value the seam supplies (`mycmd`, which production
/// fills from `default_command`) — proving the picker is additive to the free-text
/// command, not a replacement. RED today: the form has only the free-text Command
/// field; neither `claude` nor `opencode` preset is rendered.
#[spec("prompt/new-pane/010")]
#[test]
fn new_pane_010_renders_agent_command_picker() {
    let buf = buffer_text(&render_new_pane_form_to_buffer(&["demo"], 80, 24));

    // The existing Command field still renders its pre-filled value — the picker
    // is additive. The seam passes "mycmd" as the command; in production this is
    // the configured `default_command` the form opens pre-filled with.
    assert!(
        buf.contains("mycmd"),
        "the agent-command picker must keep the Command field pre-filled with the \
         default_command value (`mycmd` via the seam), got:\n{buf}"
    );

    // The agent-command picker offers the known agent presets, visible by default.
    for preset in ["claude", "opencode"] {
        assert!(
            buf.contains(preset),
            "the new-pane form must surface an agent-command picker offering the \
             `{preset}` preset (visible by default), got:\n{buf}"
        );
    }
}
