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

/// Scenario: Inspect Codex screens whose composer holds the deck's own doorbell line after Codex dropped its Enter (shortcuts hint hidden, with and without a warning, wrapped), and screens that only look close: a line the human edited, a human draft, a start dialog, and an empty composer below a submitted doorbell in history. Only the unsubmitted line in the composer itself may be flushed.
#[test]
fn codex_stranded_doorbell_line_is_recognised_only_in_the_composer() {
    let placeholder = include_str!("fixtures/codex-inbox/composer.txt");
    let line = " dot-agent-deck: message m-oj4hazfz-6d18338071ac2788 waiting, run: dot-agent-deck inbox fetch";
    let holding = |text: &str, footer: &str| {
        placeholder
            .replace("Ask Codex to do anything", text)
            .replace("? for shortcuts", footer)
    };
    for footer in ["⚠ 1 warning · f2 to view", ""] {
        let stranded = holding(line, footer);
        assert!(
            codex_composer_holds_doorbell_line(&rows(&stranded)),
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
        codex_composer_holds_doorbell_line(&rows(&wrapped)),
        "a wrapped doorbell line must be recognised"
    );
    for text in [
        &format!("{line} please"),
        "human-retained-draft",
        &format!("{line}\n  and more"),
    ] {
        assert!(
            !codex_composer_holds_doorbell_line(&rows(&holding(text, ""))),
            "only the deck's own line may be flushed: {text:?}"
        );
    }
    assert!(
        !codex_composer_holds_doorbell_line(&rows(placeholder)),
        "an empty composer holds no stranded line"
    );
    let history = placeholder.replacen(
        "/scratch/project\n",
        &format!("/scratch/project\n\n›{line}\n"),
        1,
    );
    assert!(
        !codex_composer_holds_doorbell_line(&rows(&history)),
        "a doorbell already submitted into history is not in the composer"
    );
    for dialog in [
        include_str!("fixtures/codex-inbox/update.txt"),
        include_str!("fixtures/codex-inbox/folder.txt"),
        include_str!("fixtures/codex-inbox/hooks.txt"),
    ] {
        assert!(!codex_composer_holds_doorbell_line(&rows(dialog)));
    }
}
