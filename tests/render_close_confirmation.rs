//! L1 close-confirmation render test.
//!
//! The real Cancel/confirm dispatch and persistent `[Close]` mouse path are
//! exercised through the spawned binary by `tests/e2e_pane_close.rs`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use dot_agent_deck::issue_dispatch_run::KeptWorktree;
use dot_agent_deck::keybindings::KeybindingConfig;
use dot_agent_deck::ui::{
    Action, CloseConfirmState, CloseScope, close_confirmation_for_action, global_action,
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

/// Scenario: Resolve command-mode Ctrl+W for a selected target, feed its CloseSelected action into the close-confirmation transition, and render both pane- and tab-scoped modal states. Each scope must state its real blast radius while keeping the non-destructive Cancel option selected by default, and with no dispatched worktree to keep it offers only Cancel and Close, by drawing and by keyboard alike.
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

    assert!(text.contains("Close selected agent?"), "{text}");
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
        tab_text.contains("Close this tab and all its agents?"),
        "{tab_text}"
    );
    assert!(
        tab_text.contains("stop all agents and remove the tab"),
        "{tab_text}"
    );
    assert!(!tab_text.contains("Close selected agent?"), "{tab_text}");
    assert!(tab_text.contains("> Cancel"), "{tab_text}");

    // Issue #1958: a close the daemon cannot keep a worktree for — nothing
    // dispatched in the selection, or a daemon too old to advertise the keep —
    // offers exactly the two choices it always did, in either scope.
    for (scope, rendered) in [("pane", &text), ("tab", &tab_text)] {
        assert!(
            !rendered.contains("keep worktree"),
            "the {scope}-scoped dialog offered a keep-worktree choice with no \
             dispatched worktree to keep\n{rendered}"
        );
    }
    // …and the keyboard agrees with the drawing: Down twice from Cancel stops
    // on Close, so Enter there is the ordinary close.
    let mut keyed = prompt.clone();
    let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
    let _ = handle_close_confirm_key(&mut keyed, down);
    let _ = handle_close_confirm_key(&mut keyed, down);
    assert_eq!(
        keyed.selected, 1,
        "without a keep choice there is no third option to move to"
    );
    assert!(matches!(
        handle_close_confirm_key(
            &mut keyed,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
        ),
        Action::ConfirmCloseSelected
    ));
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
            on_request: false,
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
    assert!(text.contains("Close selected agent?"), "{text}");
    assert!(text.contains("  Close"), "{text}");

    // An inconclusive probe is kept too, so the path is still reported — under
    // wording that does not claim more than was measured.
    let inconclusive = CloseConfirmState {
        kept_worktree: Some(KeptWorktree {
            path: PATH.to_string(),
            confirmed_dirty: false,
            on_request: false,
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
            on_request: false,
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

/// The row `label` is drawn on, as the dialog paints an option: the selection
/// cursor (or a space), a space, then the label.
fn option_row(text: &str, label: &str) -> Option<usize> {
    text.lines()
        .position(|l| l.contains(&format!("> {label}")) || l.contains(&format!("  {label}")))
}

/// Scenario: Arm the close confirmation for a selection whose dispatched worktree the daemon can keep, and render it in the pane and the tab scope. Each offers Cancel, Close and `Close, keep worktree` top to bottom with Cancel still the default; Down, Down, Enter answers the keep choice, while Down, Enter is still the ordinary close.
#[spec("prompt/close-confirm/011")]
#[test]
fn close_confirm_011_a_dispatched_worktree_offers_close_keep_worktree() {
    const PATH: &str = "/home/dev/code/deck-dispatch-keep-me";
    const KEEP: &str = "Close, keep worktree";

    // Issue #1958: the TUI arms the third choice only when the daemon
    // advertises the keep and its close preview names the selection's
    // dispatched worktree — whatever that tree's state.
    let armed = CloseConfirmState {
        dispatched_worktree: Some(PATH.to_string()),
        ..CloseConfirmState::default()
    };

    for (scope, header, close_desc) in [
        (
            CloseScope::Pane,
            "Close selected agent?",
            "stop the agent and remove it",
        ),
        (
            CloseScope::Tab,
            "Close this tab and all its agents?",
            "stop all agents and remove the tab",
        ),
    ] {
        let state = CloseConfirmState {
            scope,
            ..armed.clone()
        };
        let text = buffer_to_text(&render_close_confirm_to_buffer(&state, 80, 24));
        assert!(text.contains(header), "{scope:?}\n{text}");
        let cancel = option_row(&text, "Cancel").expect("Cancel drawn");
        let close = text
            .lines()
            .position(|l| l.contains(close_desc) && !l.contains(KEEP))
            .unwrap_or_else(|| panic!("the ordinary {scope:?} Close drawn\n{text}"));
        let keep = option_row(&text, KEEP)
            .unwrap_or_else(|| panic!("{scope:?}: `{KEEP}` not offered\n{text}"));
        assert!(
            cancel < close && close < keep,
            "{scope:?}: the order is Cancel, Close, `{KEEP}`\n{text}"
        );
        assert!(
            text.contains("> Cancel"),
            "{scope:?}: Cancel stays the default selection\n{text}"
        );
        assert!(
            !text.contains(&format!("> {KEEP}")),
            "{scope:?}: the keep choice must not be pre-selected\n{text}"
        );
        // The popup grows for the third row: the keyboard hint is still inside it.
        let hint = text
            .lines()
            .position(|l| l.contains("Up/Down: navigate"))
            .unwrap_or_else(|| panic!("{scope:?}: hint line clipped\n{text}"));
        assert!(keep < hint, "{scope:?}\n{text}");
    }

    // A dirty tree warns AND offers the keep: the warning, the three choices
    // and the hint all fit.
    let dirty = CloseConfirmState {
        kept_worktree: Some(KeptWorktree {
            path: PATH.to_string(),
            confirmed_dirty: true,
            on_request: false,
        }),
        ..armed.clone()
    };
    let text = buffer_to_text(&render_close_confirm_to_buffer(&dirty, 80, 24));
    let warn = text
        .lines()
        .position(|l| l.contains("Uncommitted work here is KEPT"))
        .unwrap_or_else(|| panic!("dirty warning drawn\n{text}"));
    let keep = option_row(&text, KEEP).unwrap_or_else(|| panic!("keep offered\n{text}"));
    let hint = text
        .lines()
        .position(|l| l.contains("Up/Down: navigate"))
        .unwrap_or_else(|| panic!("hint line clipped\n{text}"));
    assert!(warn < keep && keep < hint, "{text}");

    // The keyboard reaches the third choice, and only Enter there keeps.
    let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let mut keyed = armed.clone();
    let _ = handle_close_confirm_key(&mut keyed, down);
    let _ = handle_close_confirm_key(&mut keyed, down);
    assert_eq!(keyed.selected, 2, "Down twice from Cancel reaches the keep");
    let _ = handle_close_confirm_key(&mut keyed, down);
    assert_eq!(keyed.selected, 2, "there is nothing below the keep");
    let keyed_text = buffer_to_text(&render_close_confirm_to_buffer(&keyed, 80, 24));
    assert!(keyed_text.contains(&format!("> {KEEP}")), "{keyed_text}");
    assert!(matches!(
        handle_close_confirm_key(&mut keyed, enter),
        Action::ConfirmCloseKeepWorktree
    ));

    let mut plain = armed.clone();
    let _ = handle_close_confirm_key(&mut plain, down);
    assert!(
        matches!(
            handle_close_confirm_key(&mut plain, enter),
            Action::ConfirmCloseSelected
        ),
        "Close next to a keep choice is still the ordinary close"
    );
    let mut cancel = armed;
    assert!(matches!(
        handle_close_confirm_key(&mut cancel, enter),
        Action::DismissModal
    ));
}
