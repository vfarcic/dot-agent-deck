#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Issue #1673 — lane-2 proof, per agent, of the route the desktop's voice
//! "scroll up" / "scroll down" take over an open agent pane.
//!
//! The desktop decides by the pane's state, not by the agent's name
//! (`desktop/src/lib/paneScroll.ts`): a pane on the alternate screen is sent
//! PageUp / PageDown, and a pane on the main screen scrolls the terminal's own
//! history. Each test here starts a REAL interactive agent on its cheap test
//! model in the isolated `TuiDeck` harness, has it write a numbered list longer
//! than the pane, and checks the route its pane takes:
//!
//! - Claude Code with its full-screen renderer, OpenCode, Codex and Pi: the
//!   PageUp / PageDown bytes are written on an attach stream, the way the
//!   desktop writes what its terminal sends (`KIND_STREAM_IN`,
//!   `desktop/src-tauri/src/terminal.rs`), and the agent's own transcript
//!   moves back and then forward again.
//! - Claude Code with its default renderer writes inline, so its output is in
//!   the terminal's history instead: PageUp in the TUI's command mode moves the
//!   deck's copy of that history, which is the TUI's half of the same route.
//!
//! What a pane shows is read off the rendered TUI grid, as the numbers the list
//! puts at the start of a line.

dot_agent_deck_test_harness::link!();

use std::time::{Duration, Instant};

use common::TuiDeck;
use dot_agent_deck::agent_pty::AgentRecord;
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::AgentType;
use spec::spec;

const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";
/// The list's last number. Far more lines than the pane is tall, so the end of
/// the list and its first rows are never on screen together.
const LAST: u32 = 150;
const WAIT: Duration = Duration::from_secs(100);
/// How long typed text must sit before Enter: an Enter in the same burst as the
/// text is taken as part of a paste.
const BEFORE_SUBMIT: Duration = Duration::from_millis(750);
const PAGE_UP: &str = "\x1b[5~";
const PAGE_DOWN: &str = "\x1b[6~";
/// Words of the prompt that show it reached the input box.
const MARKER: &str = "how my terminal scrolls";

/// A plain request a cheap model answers without tools. A directive-style
/// prompt ("do not call any tool…", with a tag in front) was refused as an
/// instruction to distrust by Haiku in Claude Code and in OpenCode.
fn prompt() -> String {
    format!(
        "I am checking {MARKER} with a long answer. Please reply with the numbers from 1 to {LAST}, \
         one number per line, and no other text. You do not need any tools for this."
    )
}

/// Every number that is a whole line of a pane: the cells between the deck's
/// pane borders, trimmed of spaces and of the bullet an agent puts before its
/// reply.
fn listed_numbers(grid: &str) -> Vec<u32> {
    grid.lines()
        .flat_map(|line| line.split(['│', '┃']))
        .filter_map(|cell| {
            let cell = cell.trim_matches(|c: char| c.is_whitespace() || c == '⏺' || c == '●');
            (!cell.is_empty() && cell.len() <= 4 && cell.chars().all(|c| c.is_ascii_digit()))
                .then(|| cell.parse().ok())
                .flatten()
        })
        .collect()
}

fn highest_listed(grid: &str) -> Option<u32> {
    listed_numbers(grid).into_iter().max()
}

fn record_named(deck: &TuiDeck, suffix: &str) -> AgentRecord {
    let records = common::agent_records_on(deck.attach_socket_path());
    records
        .iter()
        .find(|record| {
            record
                .display_name
                .as_deref()
                .is_some_and(|name| name.ends_with(suffix))
        })
        .cloned()
        .unwrap_or_else(|| panic!("no agent whose name ends with {suffix:?}: {records:?}"))
}

/// Type the list prompt on an attach stream and press Enter as a write of its
/// own; with `resubmit_ready`, repeat Enter until the agent's empty input box is
/// back (Codex drops a submit that lands while it is still initialising). Then
/// wait for the whole list, and for the screen to settle.
fn write_the_list(deck: &TuiDeck, input: &mut common::AttachInput, resubmit_ready: Option<&str>) {
    input.write(&prompt());
    deck.wait_until_grid_then_hold(
        "the prompt settled in the input box",
        BEFORE_SUBMIT,
        |grid| grid.contains(MARKER),
    );
    input.write("\r");
    if let Some(ready) = resubmit_ready {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !deck.wait_for_grid_string_within(ready, Duration::from_secs(2)) {
            assert!(
                Instant::now() < deadline,
                "the prompt never left the input box:\n{}",
                deck.snapshot_grid()
            );
            input.write("\r");
        }
    }
    assert!(
        deck.wait_for_grid_predicate_within(WAIT, |grid| highest_listed(grid) == Some(LAST)),
        "the agent never visibly wrote the list to {LAST}:\n{}",
        deck.snapshot_grid()
    );
    // Settled: the same numbers on screen for three seconds running, so a late
    // repaint of the finished turn is not mistaken for a scroll. The numbers,
    // not the whole grid: the deck's own card keeps redrawing its timers.
    let last = std::cell::RefCell::new(listed_numbers(&deck.snapshot_grid()));
    let since = std::cell::Cell::new(Instant::now());
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(60), |grid| {
            let now = listed_numbers(grid);
            if *last.borrow() != now {
                *last.borrow_mut() = now;
                since.set(Instant::now());
            }
            since.get().elapsed() >= Duration::from_secs(3)
        }),
        "the pane never settled after the list:\n{}",
        deck.snapshot_grid()
    );
}

/// PageUp on the attach stream moves the agent's transcript back, so the end
/// of the list leaves the screen and earlier numbers show; PageDown brings the
/// end back.
fn page_up_and_back(deck: &TuiDeck, input: &mut common::AttachInput, agent: &str) {
    input.write(PAGE_UP);
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(15), |grid| {
            highest_listed(grid).is_some_and(|top| top < LAST)
        }),
        "PageUp written to {agent} did not scroll its transcript back:\n{}",
        deck.snapshot_grid()
    );
    let scrolled = highest_listed(&deck.snapshot_grid()).expect("numbers on screen");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        input.write(PAGE_DOWN);
        if deck.wait_for_grid_predicate_within(Duration::from_secs(3), |grid| {
            highest_listed(grid) == Some(LAST)
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "PageDown written to {agent} never brought the end of the list back (it was at {scrolled}):\n{}",
            deck.snapshot_grid()
        );
    }
}

fn assert_running(deck: &TuiDeck, agent_id: &str, after: &str) {
    let records = common::agent_records_on(deck.attach_socket_path());
    let record = records
        .iter()
        .find(|record| record.id == agent_id)
        .unwrap_or_else(|| panic!("the agent is gone after {after}: {records:?}"));
    assert_ne!(
        record.crashed,
        Some(true),
        "crashed after {after}: {record:?}"
    );
}

/// Start Claude Code on Haiku with `--allowedTools` and the work directory's
/// trust seeded, through `StartAgent`, and wait until its startup settles.
fn launch_claude(name: &str, fullscreen: bool) -> (TuiDeck, String, tempfile::TempDir) {
    let scratch = common::harness_tempdir().expect("live scroll fixture scratch");
    let repo = scratch.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let repo = repo.canonicalize().expect("canonical fixture dir");
    let deck = TuiDeck::builder()
        .with_pty_size(160, 45)
        .with_imported_claude_credentials()
        .with_claude_project_trust(repo.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    let events = deck.subscribe_events();
    let pane_id = format!("{name}-pane");
    let mut env = vec![("DOT_AGENT_DECK_PANE_ID".to_string(), pane_id.clone())];
    // The renderer is pinned either way, from a settings FILE rather than an
    // inline JSON argument, so no quoting stands between it and claude: the
    // harness copies the host's `~/.claude/settings.json` into the test HOME,
    // and a host that sets `"tui"` would otherwise choose it. The file sits in
    // claude's working directory and is named relative to it, so the command
    // line carries no path that a space in the harness's temp root could split. As in
    // `mode/live/003`, the setting alone does not force the full-screen
    // renderer after a launch claude thinks crashed; the variable does.
    let renderer = if fullscreen { "fullscreen" } else { "default" };
    let settings = format!("scroll-live-{renderer}.json");
    std::fs::write(repo.join(&settings), format!("{{\"tui\":\"{renderer}\"}}"))
        .expect("write the renderer settings");
    if fullscreen {
        env.push(("CLAUDE_CODE_NO_FLICKER".into(), "1".into()));
    }
    let command =
        format!("claude --model {HAIKU_MODEL} --settings {settings} --allowedTools Bash Read");
    let response = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StartAgent {
            command: Some(command),
            cwd: Some(repo.to_string_lossy().into_owned()),
            rows: 36,
            cols: 156,
            env,
            display_name: Some(name.into()),
            tab_membership: None,
            agent_type: Some(AgentType::ClaudeCode),
            seed: None,
            authoring_kind: None,
            client_seeded_kind: None,
            remember_command: false,
        },
    )
    .expect("start interactive Haiku");
    assert!(response.ok, "StartAgent: {response:?}");
    let agent_id = response.id.expect("started Haiku agent id");
    deck.wait_for_absence("No active agents");
    deck.send_keys(b"1");
    assert!(
        deck.wait_for_grid_string_within("Claude Code v", Duration::from_secs(60)),
        "the interactive Claude UI never rendered:\n{}",
        deck.snapshot_grid()
    );
    events.wait_for_session_start_on_pane(&pane_id, &agent_id, Duration::from_secs(60));
    // Its input row, held a moment so a keystroke does not land mid-startup.
    // (Not `wait_until_panes_settled`: the full-screen renderer keeps
    // repainting, so its pane never goes quiet.)
    deck.wait_until_grid_then_hold("Claude's input row", Duration::from_secs(2), |grid| {
        grid.contains('❯')
    });
    // The scratch directory is the agent's cwd, so the caller keeps it.
    (deck, agent_id, scratch)
}

/// Start `command` through the New agent dialog and wait for `ready`.
fn launch_from_dialog(
    builder: common::TuiDeckBuilder,
    fixture: &str,
    name: &str,
    command: &str,
    ready: &str,
) -> (TuiDeck, String) {
    let deck = builder.with_pty_size(180, 45).launch_with_fixture(fixture);
    deck.wait_for_string("No active agents");
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("┌ New Agent");
    deck.send_keys(b"\t");
    deck.send_keys(name.as_bytes());
    deck.send_keys(b"\t");
    deck.send_keys(command.as_bytes());
    let (submit_col, submit_row) = deck.wait_for_in_grid("[Submit]");
    deck.click(submit_col, submit_row);
    deck.wait_for_string("[Command Mode Ctrl+D]");
    assert!(
        deck.wait_for_grid_string_within(ready, Duration::from_secs(90)),
        "{name} never painted {ready:?}:\n{}",
        deck.snapshot_grid()
    );
    let id = record_named(&deck, name).id;
    (deck, id)
}

/// Scenario: Start a real interactive Claude Code on Haiku with its full-screen renderer and have it type the numbers 1 to 150. Write PageUp on an attach stream, as the desktop's "scroll up" over the pane does: the end of the list leaves the screen and earlier numbers show. PageDown brings 150 back, and the agent is still running.
#[spec("mode/scroll/009")]
#[test]
fn mode_scroll_009_fullscreen_claude_scrolls_its_transcript_on_page_keys() {
    skip_unless!(common::check_claude_available());
    let (deck, agent_id, _cwd) = launch_claude("scroll-claude-fullscreen", true);
    let mut input = common::AttachInput::attach(deck.attach_socket_path(), &agent_id);
    write_the_list(&deck, &mut input, None);
    page_up_and_back(&deck, &mut input, "full-screen Claude Code");
    assert_running(&deck, &agent_id, "the page keys");
}

/// Scenario: Start a real interactive Claude Code on Haiku with its default renderer, which writes inline, and have it type the numbers 1 to 150. Switch the deck to command mode with Ctrl+D and press PageUp: the deck's own copy of the pane's history scrolls back, so the end of the list leaves the screen and earlier numbers show, and no `Nothing to scroll` notice appears.
#[spec("mode/scroll/010")]
#[test]
fn mode_scroll_010_default_claude_leaves_history_the_deck_scrolls() {
    skip_unless!(common::check_claude_available());
    let (deck, agent_id, _cwd) = launch_claude("scroll-claude-default", false);
    let mut input = common::AttachInput::attach(deck.attach_socket_path(), &agent_id);
    write_the_list(&deck, &mut input, None);
    deck.send_keys(b"\x04"); // Ctrl+D -> command mode
    deck.wait_until_grid("command mode", |grid| {
        grid.lines().any(|line| line.starts_with(" COMMAND "))
    });
    // Pressed until the view moves: the first PageUp after Ctrl+D can be lost
    // (observed while writing this test, with a plain `seq` pane as well).
    // That is #2477, a separate TUI defect; this test is about whether default
    // Claude leaves history to scroll, so it does not pin the first press.
    // The fix for #2477 owns the single-press regression test, and should
    // turn this loop back into one press.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        deck.send_keys(PAGE_UP.as_bytes());
        if deck.wait_for_grid_predicate_within(Duration::from_secs(2), |grid| {
            highest_listed(grid).is_some_and(|top| top < LAST)
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "command-mode PageUp did not scroll the deck's copy of default Claude's history:\n{}",
            deck.snapshot_grid()
        );
    }
    assert!(
        !deck.snapshot_grid().contains("Nothing to scroll"),
        "a pane with history must not claim there is nothing to scroll:\n{}",
        deck.snapshot_grid()
    );
    assert_running(&deck, &agent_id, "the scroll");
}

/// Scenario: Start a real interactive OpenCode on its cheap test model from the New agent dialog and have it type the numbers 1 to 150. Write PageUp on an attach stream, as the desktop's "scroll up" over the pane does: earlier numbers show and 150 leaves the screen; PageDown brings 150 back, with the agent still running.
#[spec("mode/scroll/011")]
#[test]
fn mode_scroll_011_opencode_scrolls_its_transcript_on_page_keys() {
    skip_unless!(common::check_opencode_available());
    let command = common::opencode_interactive_command(false, false, "");
    let builder = TuiDeck::builder().with_imported_opencode_credentials();
    let (deck, agent_id) = launch_from_dialog(
        builder,
        "minimal",
        "scroll-opencode",
        &command,
        common::OPENCODE_SUBMIT_READY,
    );
    let mut input = common::AttachInput::attach(deck.attach_socket_path(), &agent_id);
    write_the_list(&deck, &mut input, None);
    page_up_and_back(&deck, &mut input, "OpenCode");
    assert_running(&deck, &agent_id, "the page keys");
}

/// Scenario: Start a real interactive Codex on its cheap test model from the New agent dialog and have it type the numbers 1 to 150. Write PageUp on an attach stream, as the desktop's "scroll up" over the pane does: earlier numbers show and 150 leaves the screen; PageDown brings 150 back, with the agent still running.
#[spec("mode/scroll/012")]
#[test]
fn mode_scroll_012_codex_scrolls_its_transcript_on_page_keys() {
    skip_unless!(common::check_codex_available());
    const READY: &str = "Ask Codex to do anything";
    let command = format!(
        "codex --model {} --sandbox workspace-write --ask-for-approval never -c 'model_reasoning_effort=\"low\"'",
        common::codex_test_model()
    );
    let builder = TuiDeck::builder()
        .with_env("PATH", common::path_with_binary_dir())
        .with_imported_codex_credentials();
    let (deck, agent_id) =
        launch_from_dialog(builder, "codex-live", "scroll-codex", &command, READY);
    let mut input = common::AttachInput::attach(deck.attach_socket_path(), &agent_id);
    write_the_list(&deck, &mut input, Some(READY));
    page_up_and_back(&deck, &mut input, "Codex");
    assert_running(&deck, &agent_id, "the page keys");
}

/// Scenario: Start a real interactive Pi on its cheap test model with its built-in tools off, restored from a saved session as `pi/live/001` does, and have it type the numbers 1 to 150. Write PageUp on an attach stream, as the desktop's "scroll up" over the pane does: earlier numbers show and 150 leaves the screen; PageDown brings 150 back, with the agent still running.
#[spec("mode/scroll/013")]
#[test]
fn mode_scroll_013_pi_scrolls_its_transcript_on_page_keys() {
    skip_unless!(common::check_pi_available());
    const NAME: &str = "scroll-pi";
    let deck = TuiDeck::builder()
        .with_pty_size(180, 45)
        .with_env("DOT_AGENT_DECK_EXPERIMENTAL", "1")
        .with_env("PATH", common::path_with_binary_dir())
        .with_imported_pi_credentials()
        .with_continue_session(
            NAME,
            format!(
                "pi {} --approve --no-builtin-tools --thinking off",
                common::pi_test_model().cli_args()
            ),
        )
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    // Pi's footer names the model it runs.
    assert!(
        deck.wait_for_grid_string_within(
            common::pi_test_model().model.as_str(),
            Duration::from_secs(90)
        ),
        "the interactive Pi UI never rendered:\n{}",
        deck.snapshot_grid()
    );
    let agent_id = record_named(&deck, NAME).id;
    let mut input = common::AttachInput::attach(deck.attach_socket_path(), &agent_id);
    write_the_list(&deck, &mut input, None);
    page_up_and_back(&deck, &mut input, "Pi");
    assert_running(&deck, &agent_id, "the page keys");
}
