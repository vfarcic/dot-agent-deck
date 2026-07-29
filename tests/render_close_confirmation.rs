//! L1 close-confirmation render test.
//!
//! The real Cancel/confirm dispatch and persistent `[Close]` mouse path are
//! exercised through the spawned binary by `tests/e2e_pane_close.rs`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use dot_agent_deck::keybindings::KeybindingConfig;
use dot_agent_deck::ui::{
    Action, close_confirmation_for_action, global_action, render_close_confirm_to_buffer,
};
use spec::spec;

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

/// Scenario: Resolve command-mode Ctrl+W for a selected target, feed its CloseSelected action into the close-confirmation transition, and render the modal. The dialog must show Cancel and Close with the non-destructive Cancel option selected by default.
#[spec("prompt/close-confirm/001")]
#[test]
fn close_confirm_001_ctrl_w_opens_with_cancel_selected() {
    let ctrl_w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
    let action = global_action(&KeybindingConfig::default(), &ctrl_w)
        .expect("command-mode Ctrl+W must resolve a close request");
    assert!(matches!(action, Action::CloseSelected));

    let prompt = close_confirmation_for_action(&action, true)
        .expect("an armed CloseSelected action must open confirmation");
    let text = buffer_to_text(&render_close_confirm_to_buffer(&prompt, 80, 24));

    assert!(text.contains("Close selected pane?"), "{text}");
    assert!(
        text.contains("> Cancel"),
        "Cancel must carry the default selection cursor\n{text}"
    );
    assert!(text.contains("  Close"), "{text}");
}
