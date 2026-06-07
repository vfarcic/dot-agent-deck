//! PRD #80 M7 — L1 widget test for the directory-picker mouse affordances.
//!
//! Per PRD #77 Decision 2 this is an in-process test driving the production
//! `render_dir_picker` through the `render_dir_picker_to_buffer` seam
//! (TestBackend wrapper mirroring `render_button_bar_to_buffer`). No
//! subprocess, no PTY. File-layout-mirrors-catalog (Decision 7): catalog ID
//! `mouse/picker/001`'s render half lands here; the click→outcome half lives
//! in `tests/e2e_mouse_picker.rs`.
//!
//! M7 contract: the picker chrome gains clickable `[Confirm]` and `[Cancel]`
//! buttons (== Space/confirm and q/Esc) plus a clickable filter affordance
//! (== `/`), rendered ALONGSIDE the existing list and footer hints. The
//! bracketed labels are distinct from the footer text, so a `[Label]`
//! substring proves the button while the footer text proves the list chrome
//! still renders.

use dot_agent_deck::ui::render_dir_picker_to_buffer;
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

/// Scenario: Render the directory picker (rooted at the system temp dir) into
/// an 80×24 `TestBackend` buffer. The picker must render its clickable
/// `[Confirm]`, `[Cancel]`, and `[Filter]` affordances AND still render its
/// existing chrome — the ` Select Directory ` title and the navigation
/// footer. RED until M7 adds the affordances: today the picker renders only
/// the title/list/footer text, so the bracketed labels are absent.
#[spec("mouse/picker/001")]
#[test]
fn picker_001_renders_confirm_cancel_filter_affordances() {
    let buf = buffer_text(&render_dir_picker_to_buffer(std::env::temp_dir(), 80, 24));

    // Existing chrome still present (affordances are additive).
    assert!(
        buf.contains("Select Directory"),
        "picker must still render its title, got:\n{buf}"
    );
    assert!(
        buf.contains("cancel"),
        "picker must still render its navigation/footer hints, got:\n{buf}"
    );

    // New clickable affordances.
    for label in ["[Confirm]", "[Cancel]", "[Filter]"] {
        assert!(
            buf.contains(label),
            "picker must render the {label} affordance, got:\n{buf}"
        );
    }
}
