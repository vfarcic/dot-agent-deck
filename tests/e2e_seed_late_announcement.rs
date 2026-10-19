#![cfg(all(feature = "e2e", unix))]

//! Lane-1 TUI seed coverage: late or missed SessionStart cannot lose an inbox
//! seed.

mod common;
#[cfg(all(feature = "e2e", unix))]
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[cfg(all(feature = "e2e", unix))]
#[path = "support/inbox_route.rs"]
mod route;

use spec::spec;

/// Scenario: Create a dispatcher through the real TUI with a stand-in that sends no SessionStart. Its seed is queued once before any hook, and after Idle the pane fetches and visibly renders that seed without typed seed bytes.
#[spec("prompt/new-pane/017")]
#[test]
fn new_pane_017_a_spawn_time_seed_waits_for_the_agent_to_announce_itself() {
    route::client_seed(17);
}

/// Scenario: Create a dispatcher while the TUI subscriber drops its session events. The daemon still binds the seed to the correct pane and its in-pane CLI fetch acknowledges and visibly renders the seed.
#[spec("prompt/pane-input/044")]
#[test]
fn pane_input_044_a_seed_whose_session_start_the_deck_missed_is_still_delivered() {
    route::client_seed(44);
}
