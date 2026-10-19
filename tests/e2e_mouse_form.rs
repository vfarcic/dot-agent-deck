#![cfg(feature = "e2e")]

//! PRD #80 M8 — L2 synthetic tests for new-pane-form mouse parity.
//!
//! Spawns the real `dot-agent-deck` binary inside an isolated PTY, opens the
//! new-pane form (Ctrl+N → Select Directory → Space-to-confirm), and drives
//! the mouse via SGR reports through `TuiDeck::click` / `wait_for_in_grid` /
//! `send_bytes`. Each interaction is asserted to equal the corresponding
//! keystroke. Decision 6: gated behind the `e2e` feature so `cargo test-fast`
//! never compiles it.
//!
//! The form's Mode chip selector exposes the built-in `schedule` choice.
//! The form opens
//! focused on the Mode field (NewPaneFormState::new).

mod common;

use common::TuiDeck;
use spec::spec;

/// Open the new-pane form: Ctrl+N → directory picker → Space confirms the
/// launch cwd → the form. Synchronizes on the form's ` New Agent ` title.
fn open_form(deck: &TuiDeck) {
    deck.wait_for_string("No active agents");
    deck.send_bytes(b"\x0e"); // Ctrl+N → directory picker
    deck.wait_for_string("Select Directory");
    deck.send_bytes(b" "); // Space → confirm current dir → new-pane form
    // The dashboard button bar also says "New Agent". Wait for the form's
    // bordered title so a following click cannot target the picker instead.
    deck.wait_for_string("┌ New Agent");
}

/// Click the button/affordance whose label text is `needle`.
fn click_target(deck: &TuiDeck, needle: &str) {
    let (col, row) = deck.wait_for_in_grid(needle);
    deck.click(col, row);
}

/// Scenario: Open the form (focused on the Mode field) and click the `Name:`
/// field. Focus must move to Name — the same as Tab landing on it — so typing
/// a sentinel lands in the Name field and is shown there. RED until M8 adds
/// click-to-focus (today a click doesn't move field focus, so the sentinel —
/// typed while focus is still on the arrow-cycling Mode field — is ignored).
#[spec("mouse/form/001")]
#[test]
fn form_001_click_field_moves_focus() {
    let deck = TuiDeck::launch_with_fixture("form");
    open_form(&deck);

    let (col, row) = deck.wait_for_in_grid("Name:");
    deck.click(col, row);

    deck.send_bytes(b"nm777");
    deck.wait_for_string("nm777");
}

/// Scenario: Open the form and click the built-in `schedule` chip. It must
/// become selected, as it would through keyboard cycling, and the form must
/// show the schedule authoring title.
#[spec("mouse/form/001")]
#[test]
fn form_001_click_mode_chip_selects() {
    let deck = TuiDeck::launch_with_fixture("form");
    open_form(&deck);

    click_target(&deck, "[schedule]");

    deck.wait_for_string("schedule mode");
}

/// Read the visible value, excluding the form's border and padding.
fn form_field_value(grid: &str, label: &str) -> Option<String> {
    grid.lines().find_map(|line| {
        line.split_once(label).map(|(_, value)| {
            value
                .split('│')
                .next()
                .unwrap_or_default()
                .trim()
                .to_string()
        })
    })
}

/// Wait for both edited values and the visible cursor in the blank Command field.
fn wait_for_edited_form(deck: &TuiDeck, name: &str) {
    deck.capture_screen_when("edited Name and blank, focused Command", |screen| {
        let grid = screen.contents();
        let (row, line) = grid
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("Command:"))?;
        let col = line.find("Command:")?;
        let value_col = line[..col].chars().count() + "Command: ".len();
        (form_field_value(&grid, "Name:").as_deref() == Some(name)
            && form_field_value(&grid, "Command:").as_deref() == Some("")
            && !screen.hide_cursor()
            && screen.cursor_position() == (row as u16, value_col as u16))
            .then_some(())
    });
}

/// Scenario: At 120x40 and 200x55, open the form with a configured command, replace the suggested Name and clear exactly the Command's length. Wait for the edited values and Command cursor, then click Submit once; the dialog must close and a shell pane named subm5 must appear.
#[spec("mouse/form/001")]
#[test]
fn form_001_click_submit_creates_pane() {
    // Mirrors the seed helper's pre-filled command without launching a real agent.
    let configured_command =
        "claude --model claude-haiku-4-5-20251001 --allowedTools Bash Read Write";
    let config = common::harness_tempdir().unwrap();
    std::fs::write(
        config.path().join("config.toml"),
        format!("default_command = {configured_command:?}\n"),
    )
    .unwrap();

    for (cols, rows) in [(120, 40), (200, 55)] {
        let deck = TuiDeck::builder()
            .with_pty_size(cols, rows)
            .with_env(
                "DOT_AGENT_DECK_CONFIG",
                config.path().join("config.toml").to_string_lossy(),
            )
            .launch_with_fixture("form");
        open_form(&deck);

        let initial_name = deck.capture_screen_when("initial Name value", |screen| {
            form_field_value(&screen.contents(), "Name:")
        });
        deck.send_bytes(b"\t"); // Mode → Name
        deck.send_bytes(&vec![0x7f; initial_name.chars().count()]);
        deck.send_bytes(b"subm5");
        deck.wait_until_grid("Name changed to subm5", |grid| {
            form_field_value(grid, "Name:").as_deref() == Some("subm5")
        });
        deck.send_bytes(b"\t"); // Name → Command
        deck.send_bytes(&vec![0x7f; configured_command.chars().count()]);
        wait_for_edited_form(&deck, "subm5");

        let (col, row) = deck.wait_for_in_grid("[Submit]");
        eprintln!(
            "single Submit click at {cols}x{rows}: cell=({col},{row}), cursor={:?}\n{}",
            deck.terminal_cursor_snapshot(),
            common::redact_credentials_for_output(&deck.snapshot_grid())
        );
        deck.click(col, row);
        deck.wait_until_grid(
            &format!(
                "one Submit click at ({col},{row}) closes form and creates subm5 at {cols}x{rows}"
            ),
            |grid| !grid.contains("┌ New Agent") && grid.contains("subm5"),
        );
    }
}

/// Scenario: Open the form, type a Name, then click `[Cancel]`. The form must
/// close without creating a pane — the same as Esc — returning to the empty
/// dashboard with the typed name discarded. RED until M8 renders the
/// `[Cancel]` button.
#[spec("mouse/form/001")]
#[test]
fn form_001_click_cancel_discards() {
    let deck = TuiDeck::launch_with_fixture("form");
    open_form(&deck);

    deck.send_bytes(b"\t"); // Mode → Name
    deck.send_bytes(b"canc9");

    click_target(&deck, "[Cancel]");

    // Cancelled like Esc: back to the empty dashboard, no pane created.
    deck.wait_for_string("No active agents");
    assert!(
        !deck.snapshot_grid().contains("canc9"),
        "cancel must not create a pane with the typed name:\n{}",
        deck.snapshot_grid()
    );
}
