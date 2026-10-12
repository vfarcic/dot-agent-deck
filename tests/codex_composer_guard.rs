//! Fast pin for the composer recogniser used by the byte-writing guard.
use dot_agent_deck::agent_registry::codex_composer_on_screen;

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
