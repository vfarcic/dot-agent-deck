//! L1 close-confirmation render test.
//!
//! The real Cancel/confirm dispatch and persistent `[Close]` mouse path are
//! exercised through the spawned binary by `tests/e2e_pane_close.rs`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use dot_agent_deck::issue_dispatch_run::KeptWorktree;
use dot_agent_deck::keybindings::KeybindingConfig;
use dot_agent_deck::ui::{
    Action, CloseConfirmState, CloseScope, close_confirmation_for_action, global_action,
    render_close_confirm_to_buffer,
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

/// Scenario: Resolve command-mode Ctrl+W for a selected target, feed its CloseSelected action into the close-confirmation transition, and render both pane- and tab-scoped modal states. Each scope must state its real blast radius while keeping the non-destructive Cancel option selected by default.
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

    let tab_prompt = CloseConfirmState {
        scope: CloseScope::Tab,
        ..prompt.clone()
    };
    let tab_text = buffer_to_text(&render_close_confirm_to_buffer(&tab_prompt, 80, 24));
    assert!(
        tab_text.contains("Close this tab and all its panes?"),
        "{tab_text}"
    );
    assert!(
        tab_text.contains("stop all agents and remove the tab"),
        "{tab_text}"
    );
    assert!(!tab_text.contains("Close selected pane?"), "{tab_text}");
    assert!(tab_text.contains("> Cancel"), "{tab_text}");
}

/// Scenario: Arm the close confirmation for a close that would leave a dispatched worktree on disk, and render it in both probe outcomes — a confirmed-dirty tree and one whose status probe never answered. Each must name the path and say the work is kept before the Cancel/Close options, and a close that leaves nothing behind must render exactly as it did before the warning existed.
#[spec("prompt/close-confirm/007")]
#[test]
fn close_confirm_007_kept_worktree_warns_before_the_keystroke() {
    const PATH: &str = "/home/dev/code/deck-dispatch-fix-login";

    let confirmed = CloseConfirmState {
        kept_worktree: Some(KeptWorktree {
            path: PATH.to_string(),
            confirmed_dirty: true,
        }),
        ..CloseConfirmState::default()
    };
    let text = buffer_to_text(&render_close_confirm_to_buffer(&confirmed, 80, 24));
    assert!(
        text.contains("Uncommitted work here is KEPT, not deleted"),
        "a confirmed-dirty tree must be stated flatly\n{text}"
    );
    assert!(
        text.contains(PATH),
        "the warning is only actionable if it carries the path\n{text}"
    );
    // The warning must precede the options: it changes what answering means.
    let warn_at = text.find("Uncommitted work").expect("warning rendered");
    let cancel_at = text.find("> Cancel").expect("Cancel rendered");
    assert!(
        warn_at < cancel_at,
        "the warning must come before the options the user is about to answer\n{text}"
    );
    // Still an ordinary close confirmation in every other respect.
    assert!(text.contains("Close selected pane?"), "{text}");
    assert!(text.contains("  Close"), "{text}");

    // An inconclusive probe is kept too, so the path is still reported — under
    // wording that does not claim more than was measured.
    let inconclusive = CloseConfirmState {
        kept_worktree: Some(KeptWorktree {
            path: PATH.to_string(),
            confirmed_dirty: false,
        }),
        ..CloseConfirmState::default()
    };
    let hedged = buffer_to_text(&render_close_confirm_to_buffer(&inconclusive, 80, 24));
    assert!(
        hedged.contains("Uncommitted work here, if any, is KEPT"),
        "an unanswered probe must hedge rather than assert dirtiness\n{hedged}"
    );
    assert!(hedged.contains(PATH), "{hedged}");
    assert!(
        !hedged.contains("KEPT, not deleted"),
        "the hedged wording must not also carry the flat claim\n{hedged}"
    );

    // A close that leaves nothing behind says nothing — the whole point is that
    // the interesting case is the one that used to be silent.
    let clean = buffer_to_text(&render_close_confirm_to_buffer(
        &CloseConfirmState::default(),
        80,
        24,
    ));
    assert!(
        !clean.contains("Uncommitted work"),
        "a close that removes its worktree must not warn about keeping one\n{clean}"
    );
}

/// Scenario: Render the kept-worktree warning for a path far longer than the dialog's 64-column body, in a terminal too narrow for the popup to widen to fit it. The path must be clipped from the FRONT, so the distinguishing tail survives, and the popup must stay inside the terminal.
#[spec("prompt/close-confirm/008")]
#[test]
fn close_confirm_008_a_long_kept_path_keeps_its_tail() {
    let path = format!(
        "/home/dev/{}/deck-dispatch-the-distinguishing-tail",
        "x".repeat(120)
    );
    let state = CloseConfirmState {
        kept_worktree: Some(KeptWorktree {
            path: path.clone(),
            confirmed_dirty: true,
        }),
        ..CloseConfirmState::default()
    };
    let buffer = render_close_confirm_to_buffer(&state, 60, 24);
    let text = buffer_to_text(&buffer);
    assert!(
        text.contains("deck-dispatch-the-distinguishing-tail"),
        "the tail is what tells one worktree from its siblings; it must survive\n{text}"
    );
    assert!(
        text.contains('\u{2026}'),
        "a clipped path must say it was clipped\n{text}"
    );
    assert!(
        !text.contains(&path),
        "the full path cannot fit in 60 columns, so it must not be claimed verbatim\n{text}"
    );
    for line in text.lines() {
        assert!(
            line.chars().count() <= 60,
            "the popup must stay inside the terminal\n{text}"
        );
    }
}
