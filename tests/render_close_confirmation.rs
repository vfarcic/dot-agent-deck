//! L1 close-confirmation state-machine and render tests.
//!
//! The production seams named here are deliberately shared with the live key
//! and button dispatch paths: `CloseSelected` may only arm the modal, while the
//! distinct `ConfirmCloseSelected` action authorizes destructive teardown.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use dot_agent_deck::keybindings::KeybindingConfig;
use dot_agent_deck::ui::{
    Action, Button, CloseConfirmState, close_confirmation_for_action, global_action,
    handle_close_confirm_key, render_close_confirm_to_buffer,
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

/// Scenario: Start from the default close prompt twice: press Enter immediately to choose Cancel, then navigate Down to Close and press Enter. Cancel must emit no destructive action (zero close calls), while the explicit Close choice emits exactly one ConfirmCloseSelected action.
#[spec("prompt/close-confirm/002")]
#[test]
fn close_confirm_002_cancel_keeps_target_and_confirm_closes() {
    let mut close_calls = 0;
    let mut cancel_prompt = CloseConfirmState::default();
    let cancel = handle_close_confirm_key(
        &mut cancel_prompt,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    if matches!(cancel, Action::ConfirmCloseSelected) {
        close_calls += 1;
    }
    assert!(
        matches!(cancel, Action::DismissModal),
        "Enter on the default Cancel option must dismiss without closing, got {cancel:?}"
    );
    assert_eq!(close_calls, 0, "Cancel must issue zero close operations");

    let mut confirm_prompt = CloseConfirmState::default();
    assert!(matches!(
        handle_close_confirm_key(
            &mut confirm_prompt,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)
        ),
        Action::Continue
    ));
    let confirm = handle_close_confirm_key(
        &mut confirm_prompt,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    if matches!(confirm, Action::ConfirmCloseSelected) {
        close_calls += 1;
    }
    assert!(
        matches!(confirm, Action::ConfirmCloseSelected),
        "only an explicit Close selection may authorize teardown, got {confirm:?}"
    );
    assert_eq!(close_calls, 1, "confirmation must authorize one close");
}

/// Scenario: Build the same CloseSelected action from command-mode Ctrl+W and from the persistent `[Close]` button, then pass each through the production confirmation transition. With no armed target the action remains a no-op; with a target both sources arm the same Cancel-default modal, proving confirmation belongs to the action rather than only to the key handler.
#[spec("prompt/close-confirm/003")]
#[test]
fn close_confirm_003_button_and_key_share_confirmation() {
    let ctrl_w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
    let key_action = global_action(&KeybindingConfig::default(), &ctrl_w)
        .expect("Ctrl+W must resolve CloseSelected");
    let button_action = Button::new("Close", "Ctrl+W", Action::CloseSelected, true).action;

    assert!(matches!(key_action, Action::CloseSelected));
    assert!(matches!(button_action, Action::CloseSelected));
    assert!(
        close_confirmation_for_action(&key_action, false).is_none(),
        "CloseSelected with no armed card/tab must remain a no-op and open no modal"
    );
    assert!(close_confirmation_for_action(&key_action, true).is_some());
    assert!(close_confirmation_for_action(&button_action, true).is_some());
}
