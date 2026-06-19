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
/// the ` New Agent ` title). Before M8 the form showed only the single
/// currently-selected mode in a `◀ … ▶` cycler (so the non-selected chip labels
/// were absent) and had no Submit/Cancel buttons; M8 added the chips + buttons.
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
