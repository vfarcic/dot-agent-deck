pub fn composer_has_prompt(screen: &str, prompt: &str) -> bool {
    let lines = screen.lines().collect::<Vec<_>>();
    let Some(mode) = lines.iter().rposition(|line| {
        line.trim_start()
            .strip_prefix('┃')
            .is_some_and(|row| row.trim_start().starts_with("Build auto"))
    }) else {
        return false;
    };
    let mut rows = Vec::new();
    for line in lines[..mode].iter().rev() {
        let Some(row) = line.trim_start().strip_prefix('┃') else {
            break;
        };
        rows.push(row);
    }
    // Compare the composer itself, retaining punctuation and ignoring whitespace
    // introduced by wrapping, including a line break inside a word.
    !prompt.is_empty()
        && rows
            .into_iter()
            .rev()
            .flat_map(str::chars)
            .filter(|ch| !ch.is_whitespace())
            .eq(prompt.chars().filter(|ch| !ch.is_whitespace()))
}

pub fn sleep_tool_visible(screen: &str) -> bool {
    screen.lines().any(|line| {
        let Some((_, row)) = line.rsplit_once('┃') else {
            return false;
        };
        let row = row.trim().trim_end_matches('│').trim();
        let mut chars = row.chars();
        let Some(marker) = chars.next() else {
            return false;
        };
        // OpenCode replaces the shell's `$` with a braille spinner while the
        // tool runs. Require its exact command heading, not the prompt text.
        (marker == '$' || ('\u{2801}'..='\u{28ff}').contains(&marker))
            && chars.as_str().trim() == "sleep 30"
    })
}

/// Scenario: Read the real OpenCode tool heading while bash runs sleep 30, with a spinner in place of its dollar sign, in both an independent pane and an attached deck row. The priming prompt, other commands and longer sleep commands must not count as the visible tool.
#[test]
fn running_sleep_heading_is_distinct_from_the_priming_prompt() {
    assert!(sleep_tool_visible("  ┃  ⠋ sleep 30"));
    assert!(sleep_tool_visible("  ┃  $ sleep 30"));
    assert!(sleep_tool_visible("┃ card │  ┃  ⠙ sleep 30         │"));
    assert!(!sleep_tool_visible("┃ Run sleep 30 once in the shell"));
    assert!(!sleep_tool_visible("┃ ⠋ sleep 300"));
    assert!(!sleep_tool_visible("┃ ⠋ sleep 30; echo done"));
    assert!(!sleep_tool_visible("┃ unrelated sleep 30"));
}

/// Scenario: Compare a complete six-row OpenCode composer draft with its original prompt before submitting it, including a wrap inside a word. Truncated or altered drafts and matching text outside the composer must fail readiness.
#[test]
fn wrapped_composer_requires_the_complete_prompt_before_submit() {
    let prompt = "Run sleep 30 once in the shell as a disposable, cancellable wait while I prepare your next task. This request lasts only until I interrupt you. An interrupt cancels the wait and completes this request: do not restart it, send a follow-up answer, or carry it into the next task. After interruption, handle the next instruction normally.";
    let screen = "┃  Run sleep 30 once in the shell as a disposable, cancellable wait\n┃  while I prepare your next task. This request lasts only until I\n┃  interrupt you. An interrupt cancels the wait and completes this\n┃  request: do not restart it, send a follow-up answer, or carry it into\n┃  the next task. After interruption, handle the next instruction\n┃  normally.\n┃\n┃  Build auto · GPT-5.6 Luna OpenAI";
    assert!(
        composer_has_prompt(screen, prompt),
        "the complete six-row composer draft must be ready before Enter"
    );
    assert!(composer_has_prompt(
        &screen.replace("normally.", "nor\n┃ mally."),
        prompt
    ));
    assert!(composer_has_prompt(
        &format!("┃ {prompt}\n┃\n┃ Build auto"),
        prompt
    ));
    assert!(!composer_has_prompt(
        &screen.replace("normally.", "normal"),
        prompt
    ));
    assert!(!composer_has_prompt(
        &screen.replace("do not restart it, ", ""),
        prompt
    ));
    assert!(!composer_has_prompt(
        &format!("{prompt}\n┃ unrelated draft\n┃\n┃ Build auto"),
        prompt
    ));
    assert!(!composer_has_prompt(&format!("┃ {prompt}"), prompt));
}
