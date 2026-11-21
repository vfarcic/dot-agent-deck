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

/// What OpenCode 2.3 draws for a running foreground shell at its default
/// transcript verbosity (`low`, the default since 2.3.0), measured on 2.3.2
/// (issue #2445): a spinner row, `⠇ Running command…`, and under it the hint
/// [`FOREGROUND_WORK_HINT`]; no `$ <command>` heading. The command is named
/// only once it ends (`↳ Shell finished · <command>`).
pub const RUNNING_COMMAND: &str = "Running command…";

/// The hint OpenCode 2.3 draws under a running foreground command.
pub const FOREGROUND_WORK_HINT: &str = "Press ctrl+b to move running work to the background";

/// Whether `screen` shows OpenCode 2.3's running foreground command: a row
/// that is a braille spinner and [`RUNNING_COMMAND`] alone (in a pane, or in
/// an attached deck's row, whose borders are ignored), and the whole
/// [`FOREGROUND_WORK_HINT`]. It names no command, so a caller pairs it with
/// the transcript's running call.
pub fn foreground_command_visible(screen: &str) -> bool {
    let running = screen.lines().any(|line| {
        let Some((before, after)) = line.rsplit_once(RUNNING_COMMAND) else {
            return false;
        };
        after
            .trim_matches(|c: char| c.is_whitespace() || c == '│')
            .is_empty()
            && before
                .trim_end()
                .chars()
                .next_back()
                .is_some_and(|marker| ('\u{2801}'..='\u{28ff}').contains(&marker))
    });
    running && screen.contains(FOREGROUND_WORK_HINT)
}

/// Scenario: Read OpenCode 2.3's running foreground command, a spinner row reading Running command… over the ctrl+b hint, in an independent pane and in an attached deck row. Either row alone, the row without its spinner, more text on it, or a hint cut short must not count.
#[test]
fn running_foreground_command_needs_its_spinner_row_and_whole_hint() {
    let pane = "     I'll run sleep 30 in the foreground as requested.\n\n     ⠇ Running command…\n\n     Press ctrl+b to move running work to the background\n";
    assert!(foreground_command_visible(pane));
    let deck = "┃Prmt: Run sleep 30 once…┃│     ⠇ Running command…            │\n┃  bash — sleep 30        ┃│     Press ctrl+b to move running work to the background   │\n";
    assert!(foreground_command_visible(deck));
    assert!(!foreground_command_visible("     ⠇ Running command…\n"));
    assert!(!foreground_command_visible(
        "     Press ctrl+b to move running work to the background\n"
    ));
    assert!(!foreground_command_visible(
        &pane.replace("⠇ Running", "Running")
    ));
    assert!(!foreground_command_visible(
        &pane.replace("Running command…", "Running command… sleep 300")
    ));
    assert!(!foreground_command_visible(
        &pane.replace("the background", "the backgro…")
    ));
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
