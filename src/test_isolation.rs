//! Detach a **unit-test** process from any real deck — issue #666 follow-up.
//!
//! `tests/harness_isolation.rs` states the rule and `tests/common/mod.rs`
//! enforces it for the integration suite: running the tests from inside a deck
//! pane means this process inherits that pane's `DOT_AGENT_DECK_SOCKET` /
//! `_ATTACH_SOCKET` / `_PANE_ID` / `_AGENT_ID`, anything spawned inherits them
//! too, and its hooks then post into the developer's LIVE dashboard — a card
//! appears under a fixture pane id and vanishes again.
//!
//! That enforcement is `common::init_test_env()`, which lives under `tests/`.
//! The lib target's own `#[cfg(test)]` unit tests do not link `tests/common/`,
//! so nothing scrubbed the four variables for them. This module is that same
//! scrub for this side of the wall. Same safety argument as the harness's:
//! nextest gives every test its own process, so mutating this process's
//! environment cannot affect another test.
//!
//! **Necessary, not sufficient, and the difference matters.** Scrubbing THIS
//! process only stops a child from *inheriting* an endpoint. A child that emits
//! hook events resolves the endpoint itself when the variable is absent, and
//! [`crate::platform::paths::socket_path`]'s fallback is
//! `$XDG_RUNTIME_DIR/dot-agent-deck.sock` — the developer's live daemon. So an
//! unpinned emitter reaches a real deck whether or not the variable was
//! inherited, and `agent_pty::spawn`'s `env_remove` of the same four variables
//! cannot help either. Two things actually close that path, and a fixture that
//! spawns an emitter needs one of them:
//!
//! * do not spawn a process that emits (a bare `/bin/cat` byte sink emits
//!   nothing — this is what `scheduler/dispatch/016` does since #666), or
//! * pin `DOT_AGENT_DECK_SOCKET` in the CHILD's environment at a path with no
//!   listener, so the emit fails closed instead of finding a stranger's daemon.

use std::sync::OnceLock;

/// The four endpoint variables, in the order `tests/harness_isolation.rs` lists
/// them. Kept in step with `tests/common/mod.rs`'s `DECK_ENDPOINT_VARS`; the two
/// cannot share a constant because the lib target does not link that file.
pub const DECK_ENDPOINT_VARS: [&str; 4] = [
    "DOT_AGENT_DECK_SOCKET",
    "DOT_AGENT_DECK_ATTACH_SOCKET",
    "DOT_AGENT_DECK_PANE_ID",
    "DOT_AGENT_DECK_AGENT_ID",
];

/// Clear every inherited deck endpoint from this test process. Idempotent, and
/// safe to call from any unit test that spawns a pane or posts synthetic hook
/// events.
pub fn detach_from_any_live_deck() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let leaked: Vec<&str> = DECK_ENDPOINT_VARS
            .into_iter()
            .filter(|v| std::env::var_os(v).is_some())
            .collect();
        if !leaked.is_empty() {
            // Loud on purpose, matching the harness: the run is now safe, but
            // the contributor should know their shell was pointed at a live
            // deck.
            eprintln!(
                "note: detaching this test process from a live deck — cleared {}. \
                 The inherited values would have sent fixture hook events into \
                 your running dashboard.",
                leaked.join(", ")
            );
        }
        for var in DECK_ENDPOINT_VARS {
            // SAFETY: nextest runs one test per process and this is called from
            // the test body before it spawns anything, via a `OnceLock` so it
            // happens exactly once per process.
            unsafe { std::env::remove_var(var) };
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Set all four deck endpoint variables to values that mimic a
    /// live deck, call the unit-test detach hook, and assert every one of them
    /// is gone — the `src/` half of `harness_clears_inherited_deck_endpoints`.
    #[test]
    fn detach_clears_inherited_deck_endpoints() {
        for var in DECK_ENDPOINT_VARS {
            // SAFETY: single-threaded test body in its own nextest process,
            // before anything is spawned.
            unsafe { std::env::set_var(var, "/run/user/1000/pretend-live-deck") };
        }

        detach_from_any_live_deck();

        for var in DECK_ENDPOINT_VARS {
            assert!(
                std::env::var_os(var).is_none(),
                "{var} survived the unit-test detach — a spawned child would \
                 inherit it and could post hook events into a live deck"
            );
        }
    }
}
