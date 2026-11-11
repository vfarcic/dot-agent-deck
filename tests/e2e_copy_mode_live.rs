#![cfg(all(unix, feature = "e2e", feature = "e2e-live"))]

//! PRD #385 M5 — keyboard copy mode on a live, interactive Claude Code pane
//! (Haiku), the way a user copies something an agent printed.
//!
//! The agent lists a fixture directory whose only file has a uniquely named
//! sentinel name that the prompt never spells, so the name can reach the pane
//! only as the agent's own output. The user then enters copy mode with `v` in
//! command mode, selects the name and yanks it. The harness runs the deck with
//! the native clipboard off, so the copy is read back as the OSC 52 sequence in
//! the byte history.

dot_agent_deck_test_harness::link!();

use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::AgentType;
use spec::spec;

const SENTINEL: &str = "COPYMODE_LIVE_7F3A91C2_SENTINEL.txt";
const PANE_ID: &str = "copy-mode-live-pane";
const WAIT: Duration = Duration::from_secs(180);

fn base64(input: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(CHARS[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The OSC 52 set-clipboard sequence the deck writes for a copy of `text`.
fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x1b\\", base64(text.as_bytes()))
}

/// Where the copy-mode cursor is on screen: the one cell drawn on a yellow
/// background (indexed colour 3).
fn copy_cursor(screen: &vt100::Screen) -> Option<(u16, u16)> {
    let (rows, cols) = screen.size();
    (0..rows.get()).find_map(|row| {
        (0..cols.get()).find_map(|col| {
            screen
                .cell(row, col)
                .filter(|cell| cell.bgcolor() == vt100::Color::Idx(3))
                .map(|_| (col, row))
        })
    })
}

fn chip(grid: &str, label: &str) -> bool {
    grid.lines().any(|line| line.starts_with(label))
}

/// Move the copy-mode cursor to `target` with `h`/`j`/`k`/`l`, and wait until
/// the screen shows it there.
fn move_cursor_to(deck: &TuiDeck, target: (u16, u16)) {
    let (col, row) = deck.capture_screen_when("the copy-mode cursor", copy_cursor);
    let mut keys = Vec::new();
    let vertical = if target.1 >= row { b'j' } else { b'k' };
    keys.extend(std::iter::repeat_n(
        vertical,
        usize::from(target.1.abs_diff(row)),
    ));
    let horizontal = if target.0 >= col { b'l' } else { b'h' };
    keys.extend(std::iter::repeat_n(
        horizontal,
        usize::from(target.0.abs_diff(col)),
    ));
    deck.send_bytes(&keys);
    deck.capture_screen_when("the copy-mode cursor on its target", |screen| {
        (copy_cursor(screen) == Some(target)).then_some(())
    });
}

/// Scenario: An interactive Claude Code agent on Haiku lists a fixture directory and prints the one file name it finds, a sentinel the prompt never spells. The user switches to command mode, presses `v` for copy mode, moves the cursor onto the name the agent printed, selects it with `v` and yanks with `y`. The OSC 52 copy carries exactly that name and the status line reports the send.
#[spec("copymode/live/001")]
#[test]
fn copymode_live_001_copy_mode_yanks_what_a_live_agent_printed() {
    skip_unless!(common::check_claude_available());

    let scratch = common::harness_tempdir().expect("live copy-mode fixture scratch");
    let repo = scratch.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join(SENTINEL), "copy mode live fixture\n").unwrap();
    let repo = repo.canonicalize().expect("canonical fixture dir");

    let deck = TuiDeck::builder()
        .with_pty_size(160, 45)
        .with_imported_claude_credentials()
        .with_claude_project_trust(repo.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    let events = deck.subscribe_events();
    let response = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StartAgent {
            command: Some(
                "claude --ax-screen-reader --model claude-haiku-4-5-20251001 --allowedTools Bash Read"
                    .into(),
            ),
            cwd: Some(repo.to_string_lossy().into_owned()),
            rows: 36,
            cols: 156,
            env: vec![("DOT_AGENT_DECK_PANE_ID".into(), PANE_ID.into())],
            display_name: Some("Copy mode Haiku".into()),
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
    events.wait_for_session_start_on_pane(PANE_ID, &agent_id, WAIT);
    deck.wait_for_absence("No active agents");
    deck.send_keys(b"1");
    assert!(
        common::wait_until_panes_settled(
            deck.attach_socket_path(),
            std::slice::from_ref(&agent_id),
            Duration::from_millis(1500),
            Duration::from_secs(8),
            WAIT,
        ),
        "interactive Claude startup must settle before typing:\n{}",
        deck.snapshot_grid()
    );
    if chip(&deck.snapshot_grid(), " COMMAND ") {
        deck.send_keys(b"\x04");
    }
    deck.wait_until_grid("interactive Haiku typing mode", |grid| {
        chip(grid, " TYPING ")
    });
    deck.submit_claude_prompt(
        &events,
        &agent_id,
        "COPY_MODE_LIST_FILES. Use Bash to run ls -1 in the current directory, then print every filename verbatim, one per line, with no commentary.",
        "COPY_MODE_LIST_FILES",
    );
    assert!(
        deck.wait_for_grid_string_within(SENTINEL, WAIT),
        "the agent must visibly print the sentinel file name:\n{}",
        deck.snapshot_grid()
    );

    deck.send_keys(b"\x04"); // Ctrl+D → command mode
    deck.wait_until_grid("command mode", |g| chip(g, " COMMAND "));
    deck.send_keys(b"v");
    deck.wait_until_grid("copy mode", |g| chip(g, " COPY "));
    let (col, row) = deck.wait_for_in_grid(SENTINEL);
    move_cursor_to(&deck, (col, row));
    deck.send_keys(b"v");
    move_cursor_to(&deck, (col + SENTINEL.len() as u16 - 1, row));
    deck.send_keys(b"y");

    assert!(
        deck.wait_for_stream_string_within(&osc52(SENTINEL), WAIT),
        "the yank must copy exactly the name the agent printed:\n{}",
        deck.snapshot_grid()
    );
    deck.wait_until_grid(
        "the status reports the OSC 52 send and copy mode has closed",
        |g| g.contains("Sent to terminal clipboard (OSC 52)") && chip(g, " COMMAND "),
    );
}
