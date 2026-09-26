//! PRD #80 M1 — tests for the action layer and the Button widget.
//!
//! `mouse/dispatch/001` is a pure-data test that proves the foundational
//! parity claim WITHOUT a TUI harness: the keyboard mapper
//! (`global_action`) and the button hit-test (`hit_test_button`) must
//! yield the *same* [`Action`] variant, so a keystroke and a future button
//! click funnel into the one `dispatch_action`. `mouse/button/001` is an L1
//! widget test that renders a Button (enabled and disabled) into a
//! `ratatui::buffer::Buffer`. File-layout-mirrors-catalog (PRD #77
//! Decision 7): catalog IDs `mouse/dispatch/NNN` and `mouse/button/NNN` live
//! here with function names `<sub-area>_<NNN>_<suffix>` (Decision 17).

use std::mem::discriminant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

use dot_agent_deck::keybindings::KeybindingConfig;
use dot_agent_deck::ui::{Action, Button, global_action, hit_test_button, opened_link_status};
use spec::spec;

/// Scenario: Build the Ctrl+N key event and run it through the keyboard
/// mapper `global_action`; separately build a synthetic
/// `[New Agent Ctrl+N]` button carrying `Action::NewPane`, record its rect
/// in a `button_rects` vec, and hit-test a click landing inside that rect
/// via `hit_test_button`. Both paths must produce the same `Action` variant
/// (`Action::NewPane`) — proving key and click funnel into one action
/// before any button bar is even rendered.
#[spec("mouse/dispatch/001")]
#[test]
fn dispatch_001_key_and_click_map_to_same_action() {
    // Keyboard path: Ctrl+N maps to the New Agent command.
    let ctrl_n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
    let key_action =
        global_action(&KeybindingConfig::default(), &ctrl_n).expect("Ctrl+N must map to an Action");
    assert!(
        matches!(key_action, Action::NewPane),
        "Ctrl+N should map to Action::NewPane, got {key_action:?}"
    );

    // Click path: a synthetic New-Pane button carrying the SAME action,
    // recorded the way the M2 button bar will record it, then a click
    // landing inside its rect hit-tests back to that action.
    let button = Button::new("New Agent", "Ctrl+N", Action::NewPane, true);
    assert_eq!(
        button.display_label(),
        "[New Agent Ctrl+N]",
        "the inline shortcut must be part of the on-screen label"
    );
    let rect = Rect::new(10, 24, button.display_label().len() as u16, 1);
    let button_rects = vec![button.pair(rect)];

    // A click inside the button rect resolves to its action.
    let click_action = hit_test_button(&button_rects, rect.x + 1, rect.y)
        .expect("a click inside the button rect must resolve to an Action");

    // Same variant from both paths → key and click share one action layer.
    assert_eq!(
        discriminant(&key_action),
        discriminant(&click_action),
        "keyboard and click must produce the same Action variant"
    );
    assert!(matches!(click_action, Action::NewPane));

    // A click that misses every button rect falls through (no action), so the
    // existing pane/selection logic still gets the event (PRD #80 hit order).
    assert!(hit_test_button(&button_rects, 0, 0).is_none());
    assert!(hit_test_button(&button_rects, rect.x + rect.width, rect.y).is_none());
}

/// Scenario: Render an enabled `[New Agent Ctrl+N]` button into a
/// `ratatui::buffer::Buffer`, then a disabled `[Close Ctrl+W]` button. The
/// enabled button's cells carry the full inline-shortcut label and are not
/// dimmed; the disabled button renders the same label shape but with the DIM
/// modifier — and render returns the `(Action, Rect)` pair the M2 bar will
/// record for hit-testing.
#[spec("mouse/button/001")]
#[test]
fn button_001_render_label_and_disabled_dim() {
    let area = Rect::new(0, 0, 20, 1);

    // Enabled button: full label, not dimmed, returns its action+rect pair.
    let enabled = Button::new("New Agent", "Ctrl+N", Action::NewPane, true);
    let mut buf = Buffer::empty(area);
    let (action, rect) = enabled.render(area, &mut buf);
    assert!(matches!(action, Action::NewPane));
    assert_eq!(rect, area);
    let rendered: String = (0..area.width).map(|x| buf[(x, 0)].symbol()).collect();
    assert!(
        rendered.contains("[New Agent Ctrl+N]"),
        "enabled button must render its inline-shortcut label, got {rendered:?}"
    );
    assert!(
        !buf[(1, 0)].modifier.contains(Modifier::DIM),
        "enabled button must not be dimmed"
    );

    // Disabled button: same label shape, rendered dimmed.
    let disabled = Button::new("Close", "Ctrl+W", Action::CloseSelected, false);
    let mut buf2 = Buffer::empty(area);
    disabled.render(area, &mut buf2);
    let rendered2: String = (0..area.width).map(|x| buf2[(x, 0)].symbol()).collect();
    assert!(
        rendered2.contains("[Close Ctrl+W]"),
        "disabled button still renders its label, got {rendered2:?}"
    );
    assert!(
        buf2[(1, 0)].modifier.contains(Modifier::DIM),
        "disabled button must render dimmed"
    );
}

/// The longest prefix of `s` that fits in `max` bytes without splitting a
/// character, computed by walking characters rather than by slicing bytes — a
/// deliberately different algorithm from the production truncator, so the
/// expectation states independently what a char-boundary cut is.
///
/// Deliberately duplicated in `tests/render_dashboard.rs` rather than shared:
/// `tests/common` is the 9k-line PTY harness, and neither of these pure-L1 test
/// binaries links it today — pulling it in for nine lines would cost both of
/// them that compile.
fn char_boundary_prefix(s: &str, max: usize) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if out.len() + ch.len_utf8() > max {
            break;
        }
        out.push(ch);
    }
    out
}

/// Scenario: Hand `opened_link_status` — the function the live Ctrl+click arm
/// calls to build its status line — the URLs an agent can write into its own
/// PTY as an OSC-8 hyperlink. A short URL is reported whole; an over-long one
/// is cut on a character boundary and marked, rather than panicking the event
/// loop on a byte index that lands inside a character.
#[spec("mouse/hyperlink/001")]
#[test]
fn hyperlink_001_opened_status_cuts_a_long_url_on_a_char_boundary() {
    // Issue #574, second site: `format!("{}...", &url[..57])`. The URL comes
    // from the embedded pane's hyperlink map — text the agent emitted — so the
    // 57th byte is as producer-controlled as the string itself.
    let short = "https://example.com/ok";
    assert_eq!(
        opened_link_status(short),
        format!("Opened: {short}"),
        "a URL inside the budget must be reported verbatim"
    );

    // Exactly at the boundary of the shortening rule: 60 bytes is still whole.
    let sixty = format!("https://example.com/{}", "a".repeat(40));
    assert_eq!(sixty.len(), 60);
    assert_eq!(opened_link_status(&sixty), format!("Opened: {sixty}"));

    for filler in ['α', 'あ', '𝄞', '😀'] {
        for pad in 0..4usize {
            let url: String =
                format!("https://ex.co/{}", "x".repeat(pad)) + &filler.to_string().repeat(40);
            assert!(url.len() > 60, "the sweep must exercise the shortening arm");
            let want = format!("Opened: {}…", char_boundary_prefix(&url, 57));
            assert_eq!(
                opened_link_status(&url),
                want,
                "URL `{url}` must be cut on a character boundary"
            );
        }
    }
}
