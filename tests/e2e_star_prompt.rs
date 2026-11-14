#![cfg(feature = "e2e")]

//! PRD #2174 — the star reminder's state is shared by the TUI and the desktop
//! app. These L2 tests drive the real binary against a reminder state written
//! the way the desktop app writes it, and read back what the TUI's answer
//! leaves for the desktop app.
//!
//! The desktop app's half of the state is `config::StarPromptState` too (its
//! crate calls `load_from` / `increment_and_check` / `snooze` /
//! `dismiss_permanently` on the same file), so the desktop's launches and
//! choices are made here through that API, at the path the
//! `DOT_AGENT_DECK_STAR_PROMPT` override names for both clients.

dot_agent_deck_test_harness::link!();

use std::path::Path;

use common::{TuiDeck, wait_for_file_contains};
use dot_agent_deck::config::StarPromptState;
use spec::spec;

const TITLE: &str = "Enjoying dot-agent-deck?";
const DASHBOARD: &str = "No active agents";

/// `launches` desktop launches, none of them answered.
fn desktop_launches(path: &Path, launches: u64) {
    for _ in 0..launches {
        StarPromptState::load_from(path).increment_and_check();
    }
}

/// The TUI, sharing the reminder state at `path`.
fn tui(path: &Path) -> TuiDeck {
    TuiDeck::builder()
        .with_env("DOT_AGENT_DECK_STAR_PROMPT", path.to_string_lossy())
        .launch_with_fixture("minimal")
}

/// Scenario: the desktop app has been launched nine times, so the TUI's
/// launch is the tenth. The TUI shows the star prompt; the user presses `d`.
/// The shared state now says "Don't ask again", and the desktop app's next
/// thirty launches are not asked.
#[spec("prompt/star/002")]
#[test]
fn star_002_dont_ask_again_in_the_tui_stops_the_desktop_app() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("star-prompt-state.json");
    desktop_launches(&path, 9);

    let deck = tui(&path);
    deck.wait_for_string(TITLE);
    deck.send_keys(b"d");
    deck.wait_for_absence(TITLE);
    wait_for_file_contains(&path, "\"permanently_dismissed\": true");

    let state = StarPromptState::load_from(&path);
    assert_eq!(state.launch_count, 10, "the TUI counted its launch");
    for launch in 11..=40 {
        assert!(
            !StarPromptState::load_from(&path).increment_and_check(),
            "desktop launch {launch} asked after Don't ask again in the TUI"
        );
    }
}

/// Scenario: the desktop app was launched nine times and its tenth launch
/// was answered "Don't ask again". The TUI is launched next and shows the
/// dashboard with no star prompt.
#[spec("prompt/star/002")]
#[test]
fn star_002_dont_ask_again_in_the_desktop_app_stops_the_tui() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("star-prompt-state.json");
    desktop_launches(&path, 9);
    let mut desktop = StarPromptState::load_from(&path);
    assert!(
        desktop.increment_and_check(),
        "the desktop's tenth launch asks"
    );
    desktop.dismiss_permanently();

    let deck = tui(&path);
    deck.wait_for_string(DASHBOARD);
    let grid = deck.snapshot_grid();
    assert!(
        !grid.contains(TITLE),
        "the TUI asked after Don't ask again in the desktop app:\n{grid}"
    );
    wait_for_file_contains(&path, "\"launch_count\": 11");
}

/// Scenario: the TUI's launch is the tenth, after nine desktop launches, and
/// the user presses `l` (Later). The desktop app's next nine launches are not
/// asked, and the tenth after the answer is.
#[spec("prompt/star/002")]
#[test]
fn star_002_later_in_the_tui_delays_the_desktop_app_ten_launches() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("star-prompt-state.json");
    desktop_launches(&path, 9);

    let deck = tui(&path);
    deck.wait_for_string(TITLE);
    deck.send_keys(b"l");
    deck.wait_for_absence(TITLE);
    wait_for_file_contains(&path, "\"last_prompt_at_launch\": 10");

    for launch in 11..=19 {
        assert!(
            !StarPromptState::load_from(&path).increment_and_check(),
            "desktop launch {launch} asked within ten launches of Later"
        );
    }
    assert!(
        StarPromptState::load_from(&path).increment_and_check(),
        "desktop launch 20 did not ask"
    );
}
