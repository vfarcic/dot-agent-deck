//! PRD #80 M5 — L1 widget test for modal mouse-parity buttons.
//!
//! Per PRD #77 Decision 2 these are in-process tests driving the production
//! modal renderers through per-modal `render_*_to_buffer` seams (TestBackend
//! wrappers mirroring `render_button_bar_to_buffer`). No subprocess, no PTY.
//! File-layout-mirrors-catalog (Decision 7): catalog ID `mouse/modal/002`
//! lands here with a function name `<sub-area>_<NNN>_<short_suffix>`
//! (Decision 17). The click→action half (`mouse/modal/001`) lives in
//! `tests/e2e_mouse_modal.rs`.
//!
//! M5 contract (decision: buttons render ALONGSIDE the existing highlighted
//! selection list, never replacing it): each modal gains explicit clickable
//! buttons — quit-confirm `[Detach] [Stop] [Cancel]`, config-gen `[Yes]
//! [No] [Never]`, star-prompt `[Star] [Snooze] [Dismiss]`, help `[Close]` —
//! while its existing list / hint text stays present. The bracketed button
//! labels are distinct from the list text (e.g. list `Detach — leave agents
//! running`, button `[Detach]`), so a `[Label]` substring proves the button
//! and a list-only phrase proves the list still renders.

use dot_agent_deck::ui::{
    render_config_gen_prompt_to_buffer, render_help_overlay_to_buffer,
    render_quit_confirm_to_buffer, render_star_prompt_to_buffer,
};
use spec::spec;

/// Flatten the rendered buffer to one string of cell symbols (rows joined),
/// so content assertions read like the on-screen modal.
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

/// Scenario: Render each of the four modals (quit-confirm, config-gen,
/// star-prompt, help) into an 80×24 `TestBackend` buffer via its render
/// seam. Each modal must render its explicit clickable buttons —
/// `[Detach] [Stop] [Cancel]`, `[Yes] [No] [Never]`, `[Star] [Snooze]
/// [Dismiss]`, `[Close]` — AND still render its existing selection-list /
/// hint content (the buttons are additive, not a replacement). RED until M5
/// adds the buttons: today the renderers draw only the list, so every
/// bracketed `[Label]` is absent while the list text is present.
#[spec("mouse/modal/002")]
#[test]
fn modal_002_buttons_render_alongside_selection_list() {
    // ── Quit-confirm: [Detach] [Stop] [Cancel] alongside the option list ──
    let quit = buffer_text(&render_quit_confirm_to_buffer(0, 80, 24));
    assert!(
        quit.contains("leave agents running"),
        "quit-confirm must still render its option list, got:\n{quit}"
    );
    for btn in ["[Detach]", "[Stop]", "[Cancel]"] {
        assert!(
            quit.contains(btn),
            "quit-confirm must render the {btn} button alongside the list, got:\n{quit}"
        );
    }

    // ── Config-gen: [Yes] [No] [Never] alongside the option list ──────────
    let cfg = buffer_text(&render_config_gen_prompt_to_buffer(0, 80, 24));
    assert!(
        cfg.contains("skip for now"),
        "config-gen must still render its option list, got:\n{cfg}"
    );
    for btn in ["[Yes]", "[No]", "[Never]"] {
        assert!(
            cfg.contains(btn),
            "config-gen must render the {btn} button alongside the list, got:\n{cfg}"
        );
    }

    // ── Star-prompt: [Star] [Snooze] [Dismiss] alongside the hint line ────
    let star = buffer_text(&render_star_prompt_to_buffer(80, 24));
    assert!(
        star.contains("github.com/vfarcic/dot-agent-deck"),
        "star-prompt must still render its existing content, got:\n{star}"
    );
    for btn in ["[Star]", "[Snooze]", "[Dismiss]"] {
        assert!(
            star.contains(btn),
            "star-prompt must render the {btn} button alongside the hint, got:\n{star}"
        );
    }

    // ── Help: [Close] alongside the help content ──────────────────────────
    let help = buffer_text(&render_help_overlay_to_buffer(80, 24));
    assert!(
        help.contains("Press ? or Esc to close"),
        "help overlay must still render its existing content, got:\n{help}"
    );
    assert!(
        help.contains("[Close]"),
        "help overlay must render a [Close] button alongside its content, got:\n{help}"
    );
}
