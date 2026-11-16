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
//! [No]`, star-prompt `[Star] [Snooze] [Dismiss]`, help `[Close]` —
//! while its existing list / hint text stays present. The bracketed button
//! labels are distinct from the list text (e.g. list `Detach — leave agents
//! running`, button `[Detach]`), so a `[Label]` substring proves the button
//! and a list-only phrase proves the list still renders.

use dot_agent_deck::repo_identity;
use dot_agent_deck::ui::{
    render_config_gen_prompt_to_buffer, render_help_overlay_to_buffer,
    render_quit_confirm_to_buffer, render_star_prompt_to_buffer,
};
use spec::spec;

#[path = "../src/test_temp.rs"]
mod test_temp;

/// Scenario: Read an existing config with automatic config prompts disabled and an existing JSON state file recording a Never choice. Both deserialize successfully and retain their stored values even though the manual dialog now offers Yes/No.
#[spec("dashboard/config-gen/002")]
#[test]
fn config_gen_002_legacy_settings_and_never_state_still_load() {
    let config: dot_agent_deck::config::DashboardConfig =
        toml::from_str("auto_config_prompt = false\n").expect("legacy config must load");
    assert!(!config.auto_config_prompt);
    let directory = test_temp::tempdir().unwrap();
    let path = directory.path().join("config-gen-state.json");
    std::fs::write(&path, r#"{"suppressed_dirs":["/legacy/project"]}"#).unwrap();
    let state: dot_agent_deck::config::ConfigGenState =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap())
            .expect("persisted Never choices must still load");
    assert!(state.is_suppressed("/legacy/project"));
}

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

/// Scenario: Render the quit, config-generation, star and help modals into an 80×24 buffer. Their clickable buttons and explanatory content remain visible, while config generation offers Yes/No without Never or the obsolete automatic-prompt hint.
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

    // ── Config-gen: Yes / No alongside the option list ─────────────────
    let cfg = buffer_text(&render_config_gen_prompt_to_buffer(0, 80, 24));
    assert!(
        cfg.contains("skip for now"),
        "config-gen must still render its option list, got:\n{cfg}"
    );
    for btn in ["[Yes]", "[No]"] {
        assert!(
            cfg.contains(btn),
            "config-gen must render the {btn} button alongside the list, got:\n{cfg}"
        );
    }
    assert!(
        !cfg.contains("Never"),
        "config-gen offers Yes/No only:\n{cfg}"
    );
    assert!(
        !cfg.contains("auto_config_prompt") && !cfg.contains("Disable:"),
        "config-gen must not advertise the inert automatic-prompt setting:\n{cfg}"
    );
    insta::assert_snapshot!("config_generation_yes_no", cfg);

    // ── Star-prompt: [Star] [Snooze] [Dismiss] alongside the hint line ────
    let star = buffer_text(&render_star_prompt_to_buffer(80, 24));
    assert!(
        star.contains(repo_identity::DISPLAY),
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
