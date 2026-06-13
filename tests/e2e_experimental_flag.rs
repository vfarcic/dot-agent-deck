#![cfg(feature = "e2e")]

//! L2 end-to-end test for the experimental feature-flag gated surface
//! (PRD #139).
//!
//! Spawns the real `dot-agent-deck` binary inside an isolated PTY against
//! the `minimal` fixture and asserts on the rendered vt100 grid. The gated
//! surface is the throwaway dashboard footer label `experimental: on` (PRD
//! #139 Open Question 5): visible only when the experimental flag is
//! enabled. This test injects the flag via the env override
//! `DOT_AGENT_DECK_EXPERIMENTAL=1` (PRD #139 M4.2: PTY/E2E forces the flag
//! via env), and a control run WITHOUT the env var confirms the surface
//! stays hidden at the default (OFF).
//!
//! The harness `env_clear()`s the spawned binary's environment and only
//! re-injects its pinned vars plus explicit `with_env` overrides, so the
//! control run is a clean OFF regardless of the test runner's own env.
//!
//! Decision 6: gated behind the `e2e` feature so CI's `cargo test-fast`
//! never compiles it. Authored RED — the flag plumbing and the gated
//! footer do not exist yet — and NOT run per-task (rule 5: the L2 suite is
//! the pre-PR gate, not the per-task gate).

mod common;

use common::TuiDeck;
use spec::spec;

/// Exact label the gated footer renders when the experimental flag is on
/// (PRD #139 Open Question 5 / M4.1). Mirrors the L1 constant in
/// `tests/experimental_flag.rs`.
const EXPERIMENTAL_FOOTER_TEXT: &str = "experimental: on";

/// Scenario: Launch the deck twice against the `minimal` fixture. The first
/// launch sets `DOT_AGENT_DECK_EXPERIMENTAL=1` in the spawned binary's env;
/// once the dashboard is up (`No active sessions`), the rendered grid must
/// show the `experimental: on` footer — proving the env override turns the
/// gated surface on end-to-end with no config-file edit. The second
/// (control) launch sets NO env var, so after the dashboard is up and
/// quiescent the grid must NOT contain `experimental: on` — proving the
/// surface stays hidden at the default (OFF).
#[spec("features/gating/003")]
#[test]
fn gating_003_env_var_toggles_footer_e2e() {
    // PRD #139 catalog: features/gating/003 — DOT_AGENT_DECK_EXPERIMENTAL=1
    // surfaces the gated footer end-to-end; without it the footer stays
    // hidden. RED today: the deck does not read the flag and never renders
    // the footer, so the ON wait times out.

    // ON: env override enables the experimental surface.
    let on = TuiDeck::builder()
        .with_env("DOT_AGENT_DECK_EXPERIMENTAL", "1")
        .launch_with_fixture("minimal");
    on.wait_for_string("No active sessions");
    on.wait_for_string(EXPERIMENTAL_FOOTER_TEXT);

    // OFF (control): no env var, default flag state -> surface hidden.
    // The gated footer is rendered in the SAME `render_dashboard` frame as
    // the "No active sessions" message, so once that string is on the grid
    // the footer region for that frame has already been written — if the
    // flag were on, `experimental: on` would be present in the very same
    // frame. We therefore assert absence right after the positive sentinel.
    // (We must NOT use `wait_until_quiescent` here: the harness documents
    // quiescence as unreliable because the deck redraws often enough that a
    // 50ms idle window may never occur — so it times out on the always-
    // redrawing dashboard. Rule 21 forbids sleeps, so a positive sentinel +
    // same-frame absence check is the deterministic primitive.)
    let off = TuiDeck::launch_with_fixture("minimal");
    off.wait_for_string("No active sessions");
    assert!(
        !off.snapshot_grid().contains(EXPERIMENTAL_FOOTER_TEXT),
        "without DOT_AGENT_DECK_EXPERIMENTAL the gated footer must stay \
         hidden at the default (OFF); grid was:\n{}",
        off.snapshot_grid()
    );
}
