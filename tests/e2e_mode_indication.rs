#![cfg(all(feature = "e2e", feature = "e2e-live"))]

//! PTY-attached L2 coverage for PRD #341's command-versus-typing indicators.
//! Both scenarios drive the real `dot-agent-deck` binary through `TuiDeck` and
//! assert only what the user can observe on the outer vt100 terminal grid.
//! `mode/live/001` uses a deterministic stand-in pane to pin banner decay;
//! `mode/live/002` runs a genuine interactive Claude Haiku agent, with no `-p`,
//! and walks the complete visible typing → command → scroll → typing journey.

mod common;

use std::time::Duration;

use common::TuiDeck;
use spec::spec;

const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";
const AGENT_INPUT_READY: &str = "Screen Reader Mode: on via flag";
const COMMAND_BANNER_SUBTITLE: &str = "Ctrl+D to type";
const STANDIN_CONTENT: &str = "MODE_LIVE_STANDIN_CONTENT_85BC9576";
const INPUT_PROBE: &str = "INPUT_PROBE_85BC";
const SCROLL_PREFIX: &str = "MODE_LIVE_SCROLL_";
const FIRST_SCROLL_FILE: &str = "MODE_LIVE_SCROLL_000_85BC9576.txt";
const SENTINEL: &str = "MODE_LIVE_SCROLL_ZZZ_SENTINEL_85BC9576.md";
const SCROLL_FIXTURE_FILE_COUNT: usize = 43;
const DEMO_BEAT_DWELL: Duration = Duration::from_secs(2);

fn command_chip_is_left_anchored(grid: &str) -> bool {
    grid.lines().any(|line| line.starts_with(" COMMAND "))
}

fn typing_chip_is_left_anchored(grid: &str) -> bool {
    grid.lines().any(|line| line.starts_with(" TYPING "))
}

fn assert_visible_text_is_dimmed(deck: &TuiDeck, text: &str, context: &str) {
    let styles = deck.visible_text_cell_styles(text).unwrap_or_else(|| {
        panic!(
            "{context}: expected readable text {text:?} on the rendered grid\nFinal grid:\n{}",
            deck.snapshot_grid()
        )
    });
    assert!(
        styles.iter().all(|style| style.dim),
        "{context}: every visible cell of {text:?} must carry DIM, got {styles:?}\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}

fn assert_typing_cursor_is_visible(
    deck: &TuiDeck,
    context: &str,
) -> common::TerminalCursorSnapshot {
    let cursor = deck.terminal_cursor_snapshot();
    assert!(
        !cursor.hidden,
        "{context}: PaneInput must expose the terminal's hardware cursor; got {cursor:?}\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
    let cell = cursor
        .cell
        .unwrap_or_else(|| panic!("{context}: visible cursor must sit on a grid cell: {cursor:?}"));
    assert_ne!(
        cell.bgcolor,
        vt100::Color::Default,
        "{context}: the live cursor cell must carry the painted block background; got {cursor:?}"
    );
    cursor
}

/// Scenario: Launch the real deck with an auto-focused `printf; sleep` pane, enter command mode with Ctrl+D, and execute the bound `j` command. The pane content remains visibly dimmed, the large banner collapses while the COMMAND chip persists, and Ctrl+D restores the TYPING chip with no banner.
#[spec("mode/live/001")]
#[test]
fn mode_live_001_banner_collapses_but_chip_persists() {
    let deck = TuiDeck::builder()
        .with_pty_size(200, 40)
        .with_continue_session(
            "mode-live-standin",
            format!("printf '{STANDIN_CONTENT}\\n'; sleep 600"),
        )
        .launch_with_fixture("minimal");

    deck.wait_until_grid("stand-in pane live in PaneInput", |grid| {
        grid.contains(STANDIN_CONTENT) && typing_chip_is_left_anchored(grid)
    });

    deck.send_keys(b"\x04");
    deck.wait_until_grid("expanded command banner and persistent chip", |grid| {
        grid.contains(COMMAND_BANNER_SUBTITLE) && command_chip_is_left_anchored(grid)
    });
    assert_visible_text_is_dimmed(&deck, STANDIN_CONTENT, "fresh command-mode stand-in pane");

    deck.send_keys(b"j");
    deck.wait_until_grid(
        "bound command collapses only the transient banner",
        |grid| !grid.contains(COMMAND_BANNER_SUBTITLE) && command_chip_is_left_anchored(grid),
    );
    assert!(
        deck.snapshot_grid().contains(STANDIN_CONTENT),
        "the stand-in pane content must remain readable after banner collapse\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    deck.send_keys(b"\x04");
    deck.wait_until_grid("PaneInput restored with persistent typing chip", |grid| {
        typing_chip_is_left_anchored(grid) && !grid.contains(COMMAND_BANNER_SUBTITLE)
    });
}

/// Scenario: Launch the real deck with an auto-focused interactive Claude Haiku pane, type and submit a directive that lists unique fixture files, then switch to command mode and wheel through that real output. Command mode visibly removes both cursor channels, dims without erasing the sentinel, shows the banner and COMMAND chip, intercepts wheel input into deck scrollback, and Ctrl+D restores the live cursor and TYPING chip.
#[spec("mode/live/002")]
#[test]
fn mode_live_002_real_haiku_user_journey() {
    skip_unless!(common::check_claude_available());

    let agent_command =
        format!("claude --ax-screen-reader --model {HAIKU_MODEL} --allowedTools Bash Read");
    let deck = TuiDeck::builder()
        .with_pty_size(200, 45)
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .with_continue_session("mode-live-haiku", agent_command)
        .launch_with_fixture("minimal");

    // The 45-row deck leaves 42 inner pane rows. The sentinel plus Claude's
    // completion, mode, and prompt rows consume four, so 38 numbered filenames
    // fit at live output; 43 puts the first file five rows into real scrollback.
    for index in 0..SCROLL_FIXTURE_FILE_COUNT {
        std::fs::write(
            deck.workdir()
                .join(format!("MODE_LIVE_SCROLL_{index:03}_85BC9576.txt")),
            b"scroll fixture\n",
        )
        .expect("write deterministic scroll fixture");
    }
    std::fs::write(deck.workdir().join(SENTINEL), b"real Haiku sentinel\n")
        .expect("write unique real-agent sentinel");

    assert!(
        deck.wait_for_grid_string_within(AGENT_INPUT_READY, Duration::from_secs(120)),
        "the genuine interactive Claude prompt never became ready\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
    deck.wait_until_grid(
        "real agent live with TYPING chip",
        typing_chip_is_left_anchored,
    );
    let draft_cursor = assert_typing_cursor_is_visible(&deck, "fresh interactive Haiku prompt");

    let prompt = format!(
        "{INPUT_PROBE}. Use Bash to run ls -1 {SCROLL_PREFIX}*; then print every matching filename verbatim, one per line, with no commentary."
    );
    deck.send_keys(prompt.as_bytes());
    assert!(
        deck.wait_for_grid_string_within(INPUT_PROBE, Duration::from_secs(30)),
        "typed keystrokes never appeared in the real agent's prompt editor\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
    let typed_cursor = assert_typing_cursor_is_visible(&deck, "typed real-agent draft");
    assert_ne!(
        (typed_cursor.row, typed_cursor.col),
        (draft_cursor.row, draft_cursor.col),
        "typing the directive must visibly move the live agent cursor"
    );
    deck.send_keys(b"\r");

    assert!(
        deck.wait_for_grid_string_within(SENTINEL, Duration::from_secs(180)),
        "the real Haiku turn did not visibly list the unique sentinel {SENTINEL:?}; the prompt uses only a prefix glob so this exact filename can come only from inspecting the fixture\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        deck.wait_for_terminal_cursor_hidden_within(false, Duration::from_secs(30)),
        "the completed real Haiku turn did not return to an interactive cursor\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
    let live_cursor = assert_typing_cursor_is_visible(&deck, "completed real Haiku turn");

    deck.send_keys(b"\x04");
    deck.wait_until_grid_then_hold(
        "real agent in command mode with banner and chip",
        DEMO_BEAT_DWELL,
        |grid| {
            grid.contains(COMMAND_BANNER_SUBTITLE)
                && grid.contains(SENTINEL)
                && command_chip_is_left_anchored(grid)
        },
    );

    let command_cursor = deck.terminal_cursor_snapshot();
    assert!(
        command_cursor.hidden,
        "command mode must hide the terminal's hardware cursor; got {command_cursor:?}\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
    let former_cursor_cell = deck
        .grid_cell_style(live_cursor.row, live_cursor.col)
        .expect("the former live cursor position must remain within the grid");
    assert_ne!(
        former_cursor_cell.bgcolor,
        live_cursor.cell.expect("live cursor cell").bgcolor,
        "command mode must remove the painted cursor block at the last live cursor position"
    );
    assert_visible_text_is_dimmed(&deck, SENTINEL, "real Haiku output in command mode");

    assert!(
        !deck.snapshot_grid().contains(FIRST_SCROLL_FILE),
        "precondition: the oldest filename must begin outside the live viewport so wheel scroll can be observed"
    );
    deck.scroll_n(150, 10, false, 30);
    deck.wait_until_grid_then_hold(
        "command-mode wheel reveals old real-agent output",
        DEMO_BEAT_DWELL,
        |grid| grid.contains(FIRST_SCROLL_FILE) && command_chip_is_left_anchored(grid),
    );
    assert!(
        deck.terminal_cursor_snapshot().hidden,
        "scrolling in command mode must not restore an interactive cursor"
    );

    deck.send_keys(b"\x04");
    deck.wait_until_grid("return to the live Haiku prompt", |grid| {
        typing_chip_is_left_anchored(grid) && !grid.contains(COMMAND_BANNER_SUBTITLE)
    });
    assert!(
        deck.wait_for_terminal_cursor_hidden_within(false, Duration::from_secs(30)),
        "the real Haiku prompt did not restore its hardware cursor after the turn\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
    deck.wait_until_grid_then_hold(
        "returned live Haiku prompt with TYPING chip",
        DEMO_BEAT_DWELL,
        |grid| typing_chip_is_left_anchored(grid) && !grid.contains(COMMAND_BANNER_SUBTITLE),
    );
    let returned_cursor =
        assert_typing_cursor_is_visible(&deck, "returned interactive Haiku prompt");
    assert_eq!(
        returned_cursor.cell.expect("returned cursor cell").bgcolor,
        live_cursor.cell.expect("live cursor cell").bgcolor,
        "returning to PaneInput must restore the same painted cursor treatment"
    );
}
