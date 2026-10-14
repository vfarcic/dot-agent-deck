//! Fast pin for the composer recogniser used by the byte-writing guard.
use dot_agent_deck::agent_registry::{
    codex_composer_holds_doorbell_line, codex_composer_on_screen,
};

fn rows(text: &str) -> Vec<String> {
    text.lines().map(|row| row.trim().to_owned()).collect()
}

/// Scenario: Inspect Codex composer rows with a placeholder, an empty input and visible multiline text. Only the empty and placeholder cases may admit a doorbell.
#[test]
fn codex_composer_empty_and_placeholder_allow_but_visible_text_holds() {
    let placeholder = include_str!("fixtures/codex-inbox/composer.txt");
    assert!(
        codex_composer_on_screen(&rows(placeholder)),
        "placeholder is not a human draft"
    );
    assert!(
        codex_composer_on_screen(&rows(&placeholder.replace("Ask Codex to do anything", ""))),
        "empty composer allows delivery"
    );
    // The draft text and wrap are from codex_011's live composer-draft.txt;
    // combine them with the known idle footer to isolate content recognition.
    assert!(
        !codex_composer_on_screen(&rows(include_str!(
            "fixtures/codex-inbox/composer-draft.txt"
        ))),
        "the two-line Codex draft must hold the doorbell"
    );
    for draft in [
        "human-retained-draft",
        "first line\n  second line",
        "Ask Codex to do anything else",
    ] {
        assert!(
            !codex_composer_on_screen(&rows(
                &placeholder.replace("Ask Codex to do anything", draft)
            )),
            "visible composer text must hold the doorbell even if Enter cleared the input tracker: {draft:?}"
        );
    }
}

/// Where Codex 0.160.0 keeps its cursor while the composer holds text: right
/// after the composer's last text row (live `codex/worker/001`: row 36,
/// column 95, the length of the stranded row).
fn cursor_after_composer(rows: &[String]) -> (usize, usize) {
    let prompt = rows
        .iter()
        .rposition(|row| row.starts_with('›'))
        .expect("a composer row");
    let last = prompt
        + rows[prompt + 1..]
            .iter()
            .take_while(|row| !row.is_empty())
            .count();
    (last, rows[last].chars().count())
}

/// Scenario: Inspect Codex screens whose composer holds the deck's own outstanding doorbell line after Codex dropped its Enter (shortcuts hint hidden, with and without a warning, wrapped), and screens that only look close: a line the human edited, a human draft, a start dialog, and an empty composer below a submitted doorbell in history. Only the deck's line in the composer itself may be flushed.
#[test]
fn codex_stranded_doorbell_line_is_recognised_only_in_the_composer() {
    let placeholder = include_str!("fixtures/codex-inbox/composer.txt");
    let line = " dot-agent-deck: message m-oj4hazfz-6d18338071ac2788 waiting, run: dot-agent-deck inbox fetch";
    let holding = |text: &str, footer: &str| {
        placeholder
            .replace("Ask Codex to do anything", text)
            .replace("? for shortcuts", footer)
    };
    let recognised = |screen: &str| {
        let rows = rows(screen);
        codex_composer_holds_doorbell_line(&rows, cursor_after_composer(&rows), line)
    };
    for footer in ["⚠ 1 warning · f2 to view", ""] {
        let stranded = holding(line, footer);
        assert!(
            recognised(&stranded),
            "a stranded doorbell line must be recognised (footer {footer:?})"
        );
        assert!(
            !codex_composer_on_screen(&rows(&stranded)),
            "a stranded line is not an empty composer"
        );
    }
    let wrapped = holding(
        " dot-agent-deck: message m-oj4hazfz-6d18338071ac2788 waiting, run:\n  dot-agent-deck inbox fetch",
        "",
    );
    assert!(
        recognised(&wrapped),
        "a wrapped doorbell line must be recognised"
    );
    for text in [
        &format!("{line} please"),
        "human-retained-draft",
        &format!("{line}\n  and more"),
    ] {
        assert!(
            !recognised(&holding(text, "")),
            "only the deck's own line may be flushed: {text:?}"
        );
    }
    assert!(
        !recognised(placeholder),
        "an empty composer holds no stranded line"
    );
    let history = placeholder.replacen(
        "/scratch/project\n",
        &format!("/scratch/project\n\n›{line}\n"),
        1,
    );
    assert!(
        !recognised(&history),
        "a doorbell already submitted into history is not in the composer"
    );
    for dialog in [
        include_str!("fixtures/codex-inbox/update.txt"),
        include_str!("fixtures/codex-inbox/folder.txt"),
        include_str!("fixtures/codex-inbox/hooks.txt"),
    ] {
        assert!(!recognised(dialog));
    }
}

/// Scenario: Show Codex's composer holding exactly the doorbell's line shape, but naming a message other than the one the deck rang, or holding the deck's own line with the cursor anywhere but right after it, or with no model line below it. None of them may be flushed: the match is bound to the deck's outstanding line at Codex's edit cursor, not to anything shaped like a doorbell.
#[test]
fn codex_stranded_line_is_bound_to_the_decks_own_line_and_the_edit_cursor() {
    let placeholder = include_str!("fixtures/codex-inbox/composer.txt");
    let ours = " dot-agent-deck: message m-oj4hazfz-6d18338071ac2788 waiting, run: dot-agent-deck inbox fetch";
    let stranded = rows(
        &placeholder
            .replace("Ask Codex to do anything", ours)
            .replace("? for shortcuts", ""),
    );
    let cursor = cursor_after_composer(&stranded);
    assert!(
        codex_composer_holds_doorbell_line(&stranded, cursor, ours),
        "control: the deck's own line at the edit cursor is recognised"
    );
    for other in [
        " dot-agent-deck: message m-oj4hazfz-6d18338071ac2789 waiting, run: dot-agent-deck inbox fetch",
        " dot-agent-deck: message invented-id waiting, run: dot-agent-deck inbox fetch",
    ] {
        assert!(
            !codex_composer_holds_doorbell_line(&stranded, cursor, other),
            "a screen line naming another message than the outstanding ring's must hold: {other:?}"
        );
    }
    let (row, column) = cursor;
    let bottom = stranded.iter().rposition(|r| !r.is_empty()).unwrap();
    for elsewhere in [
        (row, 0),
        (row, column - 1),
        (row, column + 1),
        (row + 2, column),
        (bottom, stranded[bottom].chars().count()),
        (0, 0),
    ] {
        assert!(
            !codex_composer_holds_doorbell_line(&stranded, elsewhere, ours),
            "the deck's line with the cursor at {elsewhere:?} is not Codex's focused composer"
        );
    }
    // Nothing below the line: not Codex's composer frame, whose model line
    // always follows it.
    let bare = rows(&format!("  >_ OpenAI Codex (v0.160.0)\n\n›{ours}\n"));
    let bare_cursor = cursor_after_composer(&bare);
    assert!(
        !codex_composer_holds_doorbell_line(&bare, bare_cursor, ours),
        "a lone doorbell row at the bottom of the screen is not the composer"
    );
}
