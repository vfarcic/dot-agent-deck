#![cfg(all(feature = "e2e", unix))]

//! Lane-1 TUI seed coverage: late or missed SessionStart cannot lose an inbox
//! seed. The legacy fallback-only /019 remains for S12 to delete with that path.

mod common;
#[cfg(all(feature = "e2e", unix))]
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[cfg(all(feature = "e2e", unix))]
#[path = "support/inbox_route.rs"]
mod route;

use std::time::{Duration, Instant};

use common::TuiDeck;
use spec::spec;

/// The first line of the built-in dispatcher seed, submitted as bracketed paste.
const SEED_MARKER: &str = "You are an ordinary assistant";

/// Drive the production New Agent form to spawn a dispatcher running `command`.
fn spawn_seeded_dispatcher(deck: &TuiDeck, command: &str) {
    deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
    deck.send_keys(b" "); // Space → confirm current dir → new-pane form
    deck.wait_for_string("No mode"); // form up, Mode field focused
    deck.send_keys(b"\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C"); // saturate at dispatcher
    deck.send_keys(b"\r"); // Mode → Name
    deck.send_keys(b"\r"); // Name (default) → Command
    // PRD #196 pre-fills the Command field with the last command spawned, so
    // clear it before typing (the form has no clear-line key).
    deck.send_keys(&[0x7fu8; 64]);
    deck.send_keys(command.as_bytes());
    deck.send_keys(b"\r"); // submit
}

/// Scenario: Create a dispatcher through the real TUI with a stand-in that sends no SessionStart. Its seed is queued once before any hook, and after Idle the pane fetches and visibly renders that seed without typed seed bytes.
#[spec("prompt/new-pane/017")]
#[test]
fn new_pane_017_a_spawn_time_seed_waits_for_the_agent_to_announce_itself() {
    route::client_seed(17);
}

/// Scenario: Spawn the built-in dispatcher with a stand-in that never sends a
/// `SessionStart`, and watch the first line of its seeded prompt. The pane must
/// receive one copy only after the 10-second readiness fallback and its buffer.
#[spec("prompt/new-pane/019")]
#[test]
#[ignore = "quarantined: vfarcic, #1666"]
fn new_pane_019_a_silent_agent_receives_one_seed_after_the_readiness_fallback() {
    let deck = TuiDeck::launch_with_fixture("dispatcher-seed");
    deck.wait_for_string("No active agents");
    let script = deck.workdir().join("claude");
    std::fs::write(
        &script,
        "#!/bin/sh\n\
         printf 'started\\n' >> silent-seed.log\n\
         while IFS= read -r line; do\n\
         \x20 printf 'received|%s\\n' \"$line\" >> silent-seed.log\n\
         done\n",
    )
    .expect("write silent stand-in");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
        .expect("chmod silent stand-in");

    // This starts before submission and therefore before the seed's created_at.
    let submitted = Instant::now();
    spawn_seeded_dispatcher(&deck, "./claude");
    let log_path = deck.workdir().join("silent-seed.log");
    assert!(
        common::wait_for_file_substr_count(&log_path, "started", 1, Duration::from_secs(10)),
        "the silent stand-in must have started before timing seed delivery"
    );

    // A seed observed before this point escaped through a readiness path other
    // than the buffered fallback. The stand-in never announces a conversation.
    const FALLBACK_WITH_BUFFER: Duration = Duration::from_millis(10_500);
    assert!(
        common::wait_until(FALLBACK_WITH_BUFFER + Duration::from_secs(1), || {
            submitted.elapsed() >= FALLBACK_WITH_BUFFER
        }),
        "the readiness fallback observation window did not elapse"
    );
    let before_fallback = std::fs::read_to_string(&log_path).expect("stand-in log exists");
    assert!(
        !before_fallback.contains(SEED_MARKER),
        "the dispatcher seed arrived before the 10-second fallback plus readiness buffer.\nLog:\n{before_fallback}"
    );

    assert!(
        common::wait_for_file_substr_count(&log_path, SEED_MARKER, 1, Duration::from_secs(30)),
        "the silent stand-in did not receive the dispatcher seed through the readiness fallback.\nLog:\n{}",
        std::fs::read_to_string(&log_path).unwrap_or_default()
    );
    // Give a replacement attempt time to surface before counting copies.
    let delivered = Instant::now();
    assert!(
        common::wait_until(Duration::from_secs(4), || {
            delivered.elapsed() >= Duration::from_secs(3)
        }),
        "the replacement observation window did not elapse"
    );
    let log = std::fs::read_to_string(&log_path).expect("read stand-in log");
    let copies = log
        .lines()
        .filter(|line| line.contains(SEED_MARKER))
        .count();
    assert_eq!(copies, 1, "the seed must arrive exactly once.\nLog:\n{log}");
}

/// Scenario: Create a dispatcher while the TUI subscriber drops its session events. The daemon still binds the seed to the correct pane and its in-pane CLI fetch acknowledges and visibly renders the seed.
#[spec("prompt/pane-input/044")]
#[test]
fn pane_input_044_a_seed_whose_session_start_the_deck_missed_is_still_delivered() {
    route::client_seed(44);
}
