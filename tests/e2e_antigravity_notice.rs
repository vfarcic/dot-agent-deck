#![cfg(all(feature = "e2e", unix))]

//! User-started Antigravity's experimental notice, driven through the real TUI.
//! Both reminder files live in scratch space; the agent is the captured agy stand-in.

dot_agent_deck_test_harness::link!();

use common::TuiDeck;
use serde_json::{Value, json};
use spec::spec;
use std::path::Path;
use std::time::Duration;

const TITLE: &str = "Antigravity support is experimental";
const BODY: &str =
    "Tell us how it works for you: report a problem, or let us know it works as expected.";

// Wire-free shared state, seeded just as the other client would save it.
// Keep these names aligned with the star reminder's shape, counting starts.
fn seed(path: &Path, starts: u64, last_prompt: u64, dismissed: bool) {
    std::fs::write(
        path,
        json!({
            "start_count": starts,
            "last_prompt_at_start": last_prompt,
            "permanently_dismissed": dismissed,
        })
        .to_string(),
    )
    .expect("seed shared Antigravity notice state");
}

fn read_state(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).expect("notice state exists"))
        .expect("notice state is JSON")
}

fn builder(path: &Path) -> common::TuiDeckBuilder {
    TuiDeck::builder()
        .without_agent_credentials()
        .with_pty_size(180, 45)
        .with_env("DOT_AGENT_DECK_ANTIGRAVITY_NOTICE", path.to_string_lossy())
        .with_env(
            "AGY_TEST_DECK_BIN",
            common::contained_deck_binary().display().to_string(),
        )
}

fn tui(path: &Path) -> TuiDeck {
    let deck = builder(path).launch_with_fixture("antigravity");
    deck.wait_for_string("No active agents");
    deck
}

fn submit_start(deck: &TuiDeck, command: &str) {
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("┌ New Agent");
    deck.send_keys(b"\t");
    deck.send_keys(b"notice-fixture");
    deck.send_keys(b"\t");
    deck.send_keys(command.as_bytes());
    let (col, row) = deck.wait_for_in_grid("[Submit]");
    deck.click(col, row);
}

fn stand_in_started(deck: &TuiDeck) {
    assert!(
        common::wait_for_path(
            &deck.workdir().join("replay-pid"),
            common::child_boot_budget()
        ),
        "Antigravity stand-in never started:\n{}",
        deck.snapshot_grid()
    );
    deck.wait_for_string("Antigravity CLI 1.5.9");
}

fn start(deck: &TuiDeck, command: &str) {
    submit_start(deck, command);
    stand_in_started(deck);
}

fn notice(deck: &TuiDeck) {
    deck.wait_for_string(TITLE);
}

fn no_notice(deck: &TuiDeck) {
    deck.wait_until_grid_then_hold(
        "agent without experimental notice",
        Duration::from_millis(500),
        |grid| grid.contains("[Command Mode Ctrl+D]") && !grid.contains(TITLE),
    );
}

/// Scenario: Start Antigravity through New Agent with fresh reminder state. Its experimental dialog names the feedback request, issues address and three choices.
#[spec("antigravity/notice/001")]
#[test]
fn antigravity_notice_001_first_start_explains_experimental_support() {
    let dir = common::harness_tempdir().expect("tempdir");
    let path = dir.path().join("notice.json");
    let deck = tui(&path);
    start(&deck, "./agy");
    notice(&deck);
    deck.wait_until_grid("notice feedback request", |grid| {
        grid.replace('│', " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .contains(BODY)
    });
    deck.wait_for_string(&format!(
        "{}/issues",
        dot_agent_deck::repo_identity::DISPLAY
    ));
    for choice in ["Open issues", "Later", "Don't show again"] {
        deck.wait_for_string(choice);
    }
}

/// Scenario: Start Antigravity, type an unrelated letter while its notice is visible, then dismiss with Escape. Neither key reaches the agent; a subsequent marker reaches its composer through the restored focus.
#[spec("antigravity/notice/002")]
#[test]
fn antigravity_notice_002_escape_and_modal_keys_stay_out_of_the_pane() {
    let dir = common::harness_tempdir().expect("tempdir");
    let path = dir.path().join("notice.json");
    let deck = tui(&path);
    start(&deck, "./agy");
    notice(&deck);
    let input = deck.workdir().join("input.bytes");
    let before = std::fs::read(&input).unwrap_or_default();
    deck.send_keys(b"z");
    deck.wait_until_grid_then_hold(
        "notice absorbs an unrelated key",
        Duration::from_millis(300),
        |grid| grid.contains(TITLE),
    );
    deck.send_keys(b"\x1b");
    deck.wait_for_absence(TITLE);
    deck.wait_until_grid_then_hold("dismissed notice", Duration::from_millis(300), |grid| {
        grid.contains("[Command Mode Ctrl+D]") && !grid.contains(TITLE)
    });
    assert_eq!(
        std::fs::read(&input).unwrap_or_default(),
        before,
        "notice keys, including Escape, leaked into the agent"
    );
    deck.send_keys(b"NOTICE-FOCUS-7e39");
    common::wait_for_file_contains(&input, "NOTICE-FOCUS-7e39");
    deck.wait_for_string("NOTICE-FOCUS-7e39");
}

/// Scenario: Dismiss the first Antigravity notice with Later, then start it again using the same state and see no notice. Seed the next start as the hundredth since that showing and start again; the dialog returns.
#[spec("antigravity/notice/003")]
#[test]
fn antigravity_notice_003_later_repeats_on_the_hundredth_subsequent_start() {
    let dir = common::harness_tempdir().expect("tempdir");
    let path = dir.path().join("notice.json");
    {
        let deck = tui(&path);
        start(&deck, "./agy");
        notice(&deck);
        deck.send_keys(b"l");
        deck.wait_for_absence(TITLE);
        common::wait_for_file_contains(&path, "\"last_prompt_at_start\": 1");
    }
    {
        let deck = tui(&path);
        start(&deck, "./agy");
        no_notice(&deck);
        assert_eq!(read_state(&path)["start_count"], 2);
    }
    seed(&path, 99, 1, false);
    {
        let deck = tui(&path);
        start(&deck, "./agy");
        no_notice(&deck);
    }
    let deck = tui(&path);
    start(&deck, "./agy");
    notice(&deck);
}

/// Scenario: Start Antigravity and choose Don't show again in its first notice. Preserve that stored choice while seeding a later due start; starting Antigravity stays in the pane without a dialog.
#[spec("antigravity/notice/004")]
#[test]
fn antigravity_notice_004_dont_show_again_survives_a_later_due_start() {
    let dir = common::harness_tempdir().expect("tempdir");
    let path = dir.path().join("notice.json");
    {
        let deck = tui(&path);
        start(&deck, "./agy");
        notice(&deck);
        deck.send_keys(b"d");
        deck.wait_for_absence(TITLE);
        common::wait_for_file_contains(&path, "\"permanently_dismissed\": true");
    }
    let mut saved = read_state(&path);
    saved["start_count"] = json!(1000);
    std::fs::write(&path, saved.to_string()).unwrap();
    let deck = tui(&path);
    start(&deck, "./agy");
    no_notice(&deck);
    assert_eq!(read_state(&path)["permanently_dismissed"], true);
}

/// Scenario: Start the Claude-labelled stand-in through New Agent with fresh Antigravity notice state. The pane starts without an Antigravity dialog and does not consume an Antigravity start.
#[spec("antigravity/notice/005")]
#[test]
fn antigravity_notice_005_other_agents_do_not_show_or_advance_the_notice() {
    let dir = common::harness_tempdir().expect("tempdir");
    let path = dir.path().join("notice.json");
    seed(&path, 0, 0, false);
    let deck = tui(&path);
    start(&deck, "./claude");
    no_notice(&deck);
    assert_eq!(read_state(&path)["start_count"], 0);
}

/// Scenario: Seed a Don't show again choice in the shared file, as the desktop client would, then start Antigravity through the TUI. The pane appears without the experimental notice even when its counter is due.
#[spec("antigravity/notice/006")]
#[test]
fn antigravity_notice_006_a_shared_dismissal_is_honoured_by_the_tui() {
    let dir = common::harness_tempdir().expect("tempdir");
    let path = dir.path().join("notice.json");
    seed(&path, 100, 1, true);
    let deck = tui(&path);
    start(&deck, "./agy");
    no_notice(&deck);
}

/// Scenario: Submit a New Agent command naming an absent Antigravity executable. The TUI reports that creation failed, shows no experimental notice and leaves the due start counter unchanged.
#[spec("antigravity/notice/007")]
#[test]
fn antigravity_notice_007_failed_creation_does_not_count() {
    let dir = common::harness_tempdir().expect("tempdir");
    let path = dir.path().join("notice.json");
    seed(&path, 100, 1, false);
    let deck = tui(&path);
    let missing = deck.workdir().join("absent-command/agy");
    submit_start(&deck, missing.to_str().unwrap());
    deck.wait_for_string("New agent failed:");
    deck.wait_until_grid_then_hold(
        "failed start has no notice",
        Duration::from_millis(500),
        |grid| grid.contains("No active agents") && !grid.contains(TITLE),
    );
    assert_eq!(read_state(&path)["start_count"], 100);
}

/// Scenario: Restore a plain shell pane, then type the captured agy command at its prompt and replay its SessionStart hook. The card identifies Antigravity without an experimental notice and its due start counter stays unchanged.
#[spec("antigravity/notice/008")]
#[test]
fn antigravity_notice_008_agy_typed_in_a_shell_does_not_count() {
    let dir = common::harness_tempdir().expect("tempdir");
    let path = dir.path().join("notice.json");
    seed(&path, 100, 1, false);
    let deck = builder(&path)
        .with_continue_session("notice-shell", "sh")
        .launch_with_fixture("antigravity");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.send_bytes(b"./agy\r");
    stand_in_started(&deck);
    let request = deck.workdir().join("request-0.json");
    let temporary = request.with_extension("tmp");
    std::fs::write(
        &temporary,
        json!([{"hook": "session-start.json"}]).to_string(),
    )
    .expect("write shell-started Antigravity hook");
    std::fs::rename(temporary, request).expect("publish complete replay request");
    assert!(common::wait_for_path(
        &deck.workdir().join("replayed-0"),
        common::child_boot_budget()
    ));
    deck.send_bytes(b"\x04");
    deck.wait_until_grid_then_hold(
        "shell-started Antigravity is identified without a notice",
        Duration::from_millis(500),
        |grid| grid.contains("Antigravity ·") && !grid.contains(TITLE),
    );
    assert_eq!(read_state(&path)["start_count"], 100);
}

/// Scenario: Open an orchestration whose starting role is Antigravity running the captured agy executable. Its composer appears without an experimental notice and its due start counter stays unchanged.
#[spec("antigravity/notice/009")]
#[test]
fn antigravity_notice_009_orchestration_role_does_not_count() {
    let dir = common::harness_tempdir().expect("tempdir");
    let path = dir.path().join("notice.json");
    seed(&path, 100, 1, false);
    let deck = tui(&path);
    std::fs::write(
        deck.workdir().join(".dot-agent-deck.toml"),
        "[[orchestrations]]\nname = 'notice-team'\n[[orchestrations.roles]]\nname = 'orchestrator'\nagent = 'antigravity'\ncommand = './agy'\nstart = true\n",
    ).expect("write Antigravity orchestration");
    deck.approve_project_trust(deck.workdir());
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("Orch: notice-team");
    deck.send_keys(b"\x1b[C\r\r");
    stand_in_started(&deck);
    no_notice(&deck);
    assert_eq!(read_state(&path)["start_count"], 100);
}

/// Scenario: Launch the TUI with a saved Antigravity pane running the captured agy executable. Restore opens its composer without an experimental notice and leaves the due start counter unchanged.
#[spec("antigravity/notice/010")]
#[test]
fn antigravity_notice_010_restored_antigravity_does_not_count() {
    let dir = common::harness_tempdir().expect("tempdir");
    let path = dir.path().join("notice.json");
    seed(&path, 100, 1, false);
    let deck = builder(&path)
        .with_continue_session("restored-antigravity", "./agy")
        .launch_with_fixture("antigravity");
    stand_in_started(&deck);
    no_notice(&deck);
    assert_eq!(read_state(&path)["start_count"], 100);
}

/// Scenario: Configure agy as the default command, select Schedule in New Agent and clear its prefilled Command field. Submitting resolves the empty field to the captured Antigravity executable, increments the start counter and shows its first experimental notice.
#[spec("antigravity/notice/011")]
#[test]
fn antigravity_notice_011_empty_authoring_command_counts_the_resolved_default() {
    let dir = common::harness_tempdir().expect("tempdir");
    let path = dir.path().join("notice.json");
    seed(&path, 0, 0, false);
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "default_command = './agy'\n").expect("write default command");
    let deck = builder(&path)
        .with_env("DOT_AGENT_DECK_CONFIG", config.to_string_lossy())
        .launch_with_fixture("antigravity");
    deck.wait_for_string("No active agents");
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("┌ New Agent");
    deck.send_keys(b"\x1b[C");
    deck.wait_for_string("schedule mode");
    deck.send_keys(b"\t\t");
    deck.send_keys(&[0x7f; 32]);
    deck.wait_until_grid("authoring Command field is empty", |grid| {
        grid.lines().any(|line| {
            line.split_once("Command:")
                .and_then(|(_, value)| value.split_once('│'))
                .is_some_and(|(value, _)| value.trim().is_empty())
        })
    });
    // The ordinary empty Command starts a shell; Schedule resolves the
    // empty field to the configured default before creating the pane.
    let (col, row) = deck.wait_for_in_grid("[Submit]");
    deck.click(col, row);
    stand_in_started(&deck);
    notice(&deck);
    assert_eq!(read_state(&path)["start_count"], 1);
}
