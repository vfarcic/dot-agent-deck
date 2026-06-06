#![cfg(feature = "e2e")]

//! L2 chain-smoke test for the real Claude Code CLI path.
//! PRD #77 catalog `chain-smoke/claude/001`.
//!
//! Cost note (Decision 23): one Haiku-4.5 invocation with a small
//! Bash-tool prompt — ≲500 input + 200 output tokens, well under
//! the <$0.05/run bound. Local-only (Decision 8) — CI runs only
//! `cargo test-fast` and never compiles this file.

mod common;

use common::TuiDeck;
use spec::spec;

// Backticks in the prompt are deliberately avoided: the deck's
// daemon spawns the pane via `sh -c <command>`, and a backtick in
// the unquoted region would trigger shell command substitution
// before the prompt ever reaches Claude (M3.1 reviewer S3).
const CHAIN_SMOKE_PROMPT: &str = "Use the Bash tool to run the command pwd. Make exactly one tool call, \
     then stop without further analysis.";

const PINNED_MODEL: &str = "claude-haiku-4-5-20251001";

/// Scenario: Import the host's Claude Code credentials into a
/// per-test HOME, stage a saved session whose pane runs
/// `claude -p "…use the Bash tool to run pwd…" --model
/// claude-haiku-4-5-20251001 --allowedTools Bash`, then launch the
/// deck with `--continue` so the agent process auto-starts. As the
/// real Claude run unfolds, the deck's hook plugin posts events
/// that drive the card through Thinking → Working → Idle, with the
/// `Bash` tool name visible on the card during Working. Runs against
/// the real Anthropic API; cost is bounded at one Haiku invocation.
#[spec("chain-smoke/claude/001")]
#[test]
fn claude_001_thinking_working_idle() {
    // Decision 26 runtime-skip: missing CLI or credentials is an
    // environmental condition, not a broken test. Decision 8 forbids
    // silent fallback to a different model — we pass the pinned model
    // verbatim and let Claude Code's CLI surface a clear error if it
    // disappears (cost: zero LLM tokens on a startup-time rejection).
    skip_unless!(common::check_claude_available());

    // `--allowedTools Bash` whitelists the Bash tool so the agent
    // doesn't sit on an interactive permission prompt — the harness
    // can't drive a `y` answer without entangling with the deck's
    // permission-prompt rendering. We deliberately avoid
    // `--dangerously-skip-permissions`: Claude Code refuses it
    // under root/sudo (test environments are often containerized
    // and run as root); the explicit allow-list is the supported path.
    let agent_command = format!(
        "claude -p \"{prompt}\" --model {model} --allowedTools Bash",
        prompt = CHAIN_SMOKE_PROMPT.replace('"', "\\\""),
        model = PINNED_MODEL,
    );

    let deck = TuiDeck::builder()
        .with_imported_claude_credentials()
        .with_continue_session("claude-smoke", &agent_command)
        .launch_with_fixture("chain-smoke-claude");

    // The pane card appears as soon as the deck restores the saved
    // session — the agent's name `claude-smoke` is what's shown on
    // the card title row, so its presence is a reliable starting
    // anchor and uses no LLM tokens.
    deck.wait_for_string("claude-smoke");

    // Catalog assertion: status traverses Thinking → Working → Idle,
    // and the Bash tool name appears on the card during Working.
    //
    // M4.6 P1: `wait_for_strings_in_order` walks the rolling byte
    // history rather than the live vt100 grid, so two consecutive
    // status transitions rendered in the same polling window (a
    // realistic outcome on a fast Haiku response — Thinking →
    // Working can both land in the same ~20 ms window) both stay
    // matchable. The previous shape — four sequential
    // `wait_for_string` calls against the current grid — could spin
    // past `Thinking` if `Working` had already overwritten the card
    // before the first poll, and would then timeout (Decision 9:
    // flake = bug).
    deck.wait_for_strings_in_order(&["Thinking", "Working", "Bash", "Idle"]);
}
