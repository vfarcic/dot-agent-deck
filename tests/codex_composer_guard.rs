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

/// Codex 0.160.0's last frame in live `load_004` (PRD #1666), transcribed
/// from the pane's PTY bytes with the scratch path substituted: under CPU
/// pressure it took the ring's CR as a newline in the composer, so the line
/// stays on the prompt row and the visible cursor sits on the empty row below
/// it, at the composer indent (0-based row 36, column 2), above a second blank
/// row, the model line and the warning footer. The pane was 40 x 140.
const LINE_THEN_NEWLINE_FRAME: &str = concat!(
    "\x1b[?1049h\x1b[2J\x1b[H",
    "\x1b[39;3H\x1b[38;2;246;226;183;49mGPT-5.6-Luna low\x1b[39;20H\x1b[39;49m·",
    "\x1b[39;22H\x1b[38;2;171;223;167;49m/scratch/project",
    "\x1b[40;116H\x1b[39;49m⚠\x1b[40;118H\x1b[38;2;196;167;103;49m1 warning",
    "\x1b[40;128H\x1b[39;49m·\x1b[40;130H\x1b[1mf2\x1b[40;133H\x1b[22mto\x1b[40;136Hview\x1b[39m\x1b[49m",
    "\x1b[?2026h\x1b[?25l\x1b[37;2H\x1b[0m\x1b[49m\x1b[K\x1b[36;1H\x1b[1m›",
    "\x1b[36;4H\x1b[22mdot-agent-deck:\x1b[36;20Hmessage\x1b[36;28Hm-bztvmdjd-905c176602c7c2cd",
    "\x1b[36;56Hwaiting,\x1b[36;65Hrun:\x1b[36;70Hdot-agent-deck\x1b[36;85Hinbox\x1b[36;91Hfetch",
    "\x1b[37;1H \x1b[39m\x1b[49m\x1b[0m\x1b[37;3H\x1b[?25h\x1b[?2026l",
);

/// Scenario: Replay Codex 0.160.0's measured frame in which the doorbell's Enter became a newline in the composer: the line on the prompt row, the cursor on the blank row directly below it at the composer indent. The deck's own line must be recognised there so the next ring can flush it, and the same frame must still hold for a line naming another message.
#[test]
fn codex_line_then_newline_state_from_the_live_frame_is_recognised() {
    let ours = " dot-agent-deck: message m-bztvmdjd-905c176602c7c2cd waiting, run: dot-agent-deck inbox fetch";
    let (rows, row, through) = dot_agent_deck::pane_screen_text::visible_rows_and_cursor(
        LINE_THEN_NEWLINE_FRAME.as_bytes(),
        40,
        140,
    )
    .expect("the frame parses");
    assert_eq!(rows[35], format!("›  {}", ours.trim_start()), "{rows:#?}");
    assert_eq!(row, 36, "the cursor sits on the row below the line");
    assert!(
        codex_composer_holds_doorbell_line(&rows, (row, through), ours),
        "the deck's own line with Codex's Enter taken as a newline must be recognised: {rows:#?}"
    );
    let other = " dot-agent-deck: message m-bztvmdjd-905c176602c7c2ce waiting, run: dot-agent-deck inbox fetch";
    assert!(
        !codex_composer_holds_doorbell_line(&rows, (row, through), other),
        "a line naming another message must still hold"
    );
}
