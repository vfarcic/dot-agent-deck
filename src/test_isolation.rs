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
//!   [`pin_unreachable_endpoints`] (for a `SpawnOptions::env`) and
//!   [`unreachable_endpoints`] (for a `Command`'s `.envs(…)`) are that pin.
//!
//! **Enforced, narrowly, by linkage-check rule 17** (`unit-test-emitter-pins-endpoints`,
//! issue #688). A `fn` in `src/` test code that spawns an emitter the rule
//! recognises — a `SpawnOptions` literal declaring a Wrapper-strategy
//! `agent_type`, or a command literal naming a registered agent or the deck
//! binary — has to call one of those two helpers somewhere in its body. The
//! rule reads literals, so a command or type held in a variable walks past it;
//! `xtask/linkage-check/src/unit_test_endpoint_pin.rs` lists what else it
//! cannot see.

use std::sync::OnceLock;

/// The deck identity variables, in the order `tests/harness_isolation.rs` lists
/// them. Kept in step with `tests/common/mod.rs`'s `DECK_ENDPOINT_VARS`; the two
/// cannot share a constant because the lib target does not link that file.
///
/// Issue #1077 added `DOT_AGENT_DECK_PANE_CAPABILITY`. It is not an endpoint, but it
/// has the same failure mode as the pane and agent ids beside it, and a sharper
/// one: a test process started from inside a live deck pane inherits that pane's
/// real capability token, and a CLI it launches forwards it. Against the test's
/// own daemon that token was minted by a DIFFERENT daemon, so the message is
/// refused as `UnknownToken` — which, unlike a missing token, even
/// `DOT_AGENT_DECK_HOOK_PROVENANCE=warn` does not admit. Every test that stands
/// in for a pane by running the CLI would then fail only when run from a deck
/// pane, and pass everywhere else.
pub const DECK_ENDPOINT_VARS: [&str; 5] = [
    "DOT_AGENT_DECK_SOCKET",
    "DOT_AGENT_DECK_ATTACH_SOCKET",
    "DOT_AGENT_DECK_PANE_ID",
    "DOT_AGENT_DECK_AGENT_ID",
    "DOT_AGENT_DECK_PANE_CAPABILITY",
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

/// The two endpoint variables [`pin_unreachable_endpoints`] pins. Not the
/// pane/agent ids or the capability token: those are identity, not a route to
/// a daemon, and `agent_pty::spawn` already strips inherited copies of them.
const PINNED_ENDPOINT_VARS: [&str; 2] = ["DOT_AGENT_DECK_SOCKET", "DOT_AGENT_DECK_ATTACH_SOCKET"];

/// A per-process endpoint path nothing listens on, one per variable.
///
/// Never created: the point is that a `connect(2)` to it fails. The name
/// carries this process's pid so two concurrent test processes cannot collide
/// on a path one of them might later bind.
fn unreachable_endpoint(var: &str) -> String {
    let role = if var == "DOT_AGENT_DECK_ATTACH_SOCKET" {
        "attach"
    } else {
        "hook"
    };
    std::env::temp_dir()
        .join(format!(
            "dad-unit-no-listener-{}-{role}.sock",
            std::process::id()
        ))
        .to_string_lossy()
        .into_owned()
}

/// The hook and attach endpoints, pinned at paths with no listener, as
/// `(name, value)` pairs for a child's environment.
///
/// Why a pin rather than a scrub: with the variable ABSENT, a child that emits
/// resolves the endpoint itself — [`crate::platform::paths::socket_path`] falls
/// back to `$XDG_RUNTIME_DIR/dot-agent-deck.sock` when `XDG_RUNTIME_DIR` is set
/// — and on a developer's machine that is typically their live daemon. With it
/// PRESENT the resolver takes the override arm and never reaches the fallback,
/// so the emit fails closed. Issue #688 measured the difference: the scrub
/// alone still produced 3 foreign `SessionStart`s in 8 runs of one fixture.
///
/// For a `std::process::Command` / `tokio::process::Command`, pass this to
/// `.envs(…)`. For a `SpawnOptions`, use [`pin_unreachable_endpoints`], which
/// keeps any pin the caller already chose.
pub fn unreachable_endpoints() -> Vec<(String, String)> {
    PINNED_ENDPOINT_VARS
        .into_iter()
        .map(|var| (var.to_string(), unreachable_endpoint(var)))
        .collect()
}

/// `env` with [`unreachable_endpoints`] added for every endpoint variable it
/// does not already set. A value the caller supplied wins, so a fixture that
/// deliberately points its child at its OWN sandbox daemon keeps doing so.
///
/// Meant for `SpawnOptions::env`: `agent_pty::spawn` strips the inherited
/// endpoint variables and then applies `opts.env`, and
/// `AgentPtyRegistry::spawn_agent` injects the registry's own hook socket only
/// when `opts.env` names none — so a value placed here reaches the child.
///
/// Does not touch this process's environment; call
/// [`detach_from_any_live_deck`] for that half, before anything is spawned.
pub fn pin_unreachable_endpoints(mut env: Vec<(String, String)>) -> Vec<(String, String)> {
    for (var, value) in unreachable_endpoints() {
        if !env.iter().any(|(k, _)| *k == var) {
            env.push((var, value));
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Set every deck identity variable to a value that mimics a
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

    /// Scenario: Pin the endpoints onto an env that already names the pane id
    /// and a caller-chosen hook socket, and assert the pane id and the
    /// caller's socket survive untouched while the attach socket is added at a
    /// per-process path with no listener — so a child fails closed instead of
    /// resolving the developer's live daemon.
    #[test]
    fn pin_adds_missing_endpoints_and_keeps_the_callers_own() {
        let env = pin_unreachable_endpoints(vec![
            ("DOT_AGENT_DECK_PANE_ID".into(), "pane-1".into()),
            ("DOT_AGENT_DECK_SOCKET".into(), "/sandbox/own.sock".into()),
        ]);
        let get = |k: &str| {
            env.iter()
                .filter(|(name, _)| name == k)
                .map(|(_, v)| v.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(get("DOT_AGENT_DECK_PANE_ID"), ["pane-1"]);
        assert_eq!(
            get("DOT_AGENT_DECK_SOCKET"),
            ["/sandbox/own.sock"],
            "a pin the caller chose must win, and must not be duplicated"
        );
        let attach = get("DOT_AGENT_DECK_ATTACH_SOCKET");
        assert_eq!(attach.len(), 1, "the missing attach pin must be added once");
        assert!(
            attach[0].contains(&format!("dad-unit-no-listener-{}-", std::process::id())),
            "the pin must be this process's no-listener path, got {}",
            attach[0]
        );
        assert!(
            !std::path::Path::new(attach[0]).exists(),
            "the pinned endpoint must not exist, or a connect could succeed"
        );
    }

    /// Scenario: Ask for the bare pins and assert both endpoint variables are
    /// present, distinct, and not the fallback a child would otherwise resolve.
    #[test]
    fn unreachable_endpoints_pin_both_routes_away_from_the_fallback() {
        let pins = unreachable_endpoints();
        let names: Vec<&str> = pins.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, PINNED_ENDPOINT_VARS);
        assert_ne!(pins[0].1, pins[1].1, "hook and attach pins must differ");
        for (_, value) in &pins {
            assert!(
                !value.ends_with("/dot-agent-deck.sock"),
                "a pin must not be the default hook endpoint: {value}"
            );
        }
    }
}
