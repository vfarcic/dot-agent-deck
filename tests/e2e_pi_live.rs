#![cfg(feature = "e2e")]

//! L2 PTY-attached REAL-`pi` live-pane showcase (PRD #201, CLAUDE.md rule 4 +
//! demo-reel eligibility per PRD #180). This is the Pi feature's "AS A USER
//! ACTUALLY USES AND SEES IT" test: the REAL `dot-agent-deck` binary driven
//! through the vt100/`TuiDeck` harness, with a REAL `pi` agent booting and
//! working LIVE in a pane, and the Pi pane's card rendering its first-class Pi
//! IDENTITY plus a REAL, extension-driven status TRANSITION on the vt100 grid —
//! with NO Claude-Code hook installed.
//!
//! ## Why this is separate from `e2e_pi_orchestrator.rs`
//! The `chain-smoke/pi/001` + `scheduler/pi/001` tests are HEADLESS: an
//! in-process daemon / `daemon serve` with real pi PTYs, asserting on
//! files/events/broadcasts. They are functionally correct but record NO cast
//! (only PTY-attached `TuiDeck` runs record a `full-stream.cast`, PRD #180) and
//! never exercise the live-in-the-pane, rendered-grid surface rule 4 requires.
//! This test closes that gap: it drives the real binary through the vt100
//! harness (mirroring `e2e_issue_dispatch_real.rs` / `e2e_chain_smoke_claude.rs`
//! and the catalog reference `scheduler/dispatch/013`), so the Pi surface a user
//! sees is what is asserted AND what the reel clip captures.
//!
//! ## Scenario chosen: a single live Pi pane (not orchestrator→worker)
//! Per the phase-5 brief, a reliable single live Pi pane with a visible real
//! status fully satisfies rule 4; the orchestrator→worker delegation is the more
//! elaborate reel but must not be shown at the cost of reliability. A single
//! directive-prompted Pi pane's lifecycle (`session_start`→waiting,
//! `agent_start`→running) is the deterministic part — it fires regardless of
//! what the model decides — so the status transition here does not hinge on a
//! multi-agent LLM chain. The heavier orchestrator route is already pinned
//! headless by `chain-smoke/pi/001`.
//!
//! ## What makes the rendered assertion genuine (not a plumbing stand-in)
//! - **Experimental flag ON** (`DOT_AGENT_DECK_EXPERIMENTAL=1`): the Pi
//!   first-class identity is gated behind `features::show_pi_agent()` at the
//!   render seam (CLAUDE.md rule 9). With the flag ON the card title reads
//!   `Pi · <id>`; without it the Pi identity would be suppressed and the reel
//!   clip would show no Pi surface. The deck reads the flag from its env.
//! - **Bundled extension materialized into the per-test HOME** (via
//!   `TuiDeckBuilder::with_pi_extension`, before launch): the daemon the deck
//!   lazy-spawns inherits that HOME, so the pi child it spawns auto-discovers
//!   `~/.pi/agent/extensions/dot-agent-deck/` and loads it at boot — the same
//!   seam `e2e_pi_orchestrator.rs` uses, staged pre-launch so a startup-restored
//!   pane finds it in time.
//! - **Status via `agent-event`, NO hook**: the Pi pane's card status is driven
//!   ONLY by the extension shelling `dot-agent-deck agent-event` (mapped
//!   `waiting`→Needs Input / `running`→Thinking). No `~/.claude/settings.json`
//!   is touched — a Pi pane is hook-safe by construction.
//!
//! ## Credentials (Design Decision #5, harness caveat)
//! pi authenticates to OpenRouter via `OPENROUTER_API_KEY`. The harness scrubs
//! the spawned deck's env to a pinned set, so the key + the freshly-built
//! binary dir on PATH are threaded in explicitly via `with_env`; the deck's
//! lazy-spawned daemon inherits them, and so does the pi child. The key is NEVER
//! printed (only checked non-empty for the runtime-skip).
//!
//! Tier: e2e (`#[cfg(feature = "e2e")]`) — spawns a real agent, hits a real
//! model. Flaky-tolerant pre-PR tier (real LLM), run once, not looped (rule
//! 4/5). Runtime-skipped (Decision 26) when `pi` / `OPENROUTER_API_KEY` is
//! absent.

use std::process::Stdio;
use std::time::Duration;

mod common;

use common::TuiDeck;
use spec::spec;

/// Cheapest GPT-5.x tier on OpenRouter that reliably runs a directive turn
/// (same model `chain-smoke/pi/001` / `scheduler/pi/001` pin).
const PI_MODEL: &str = "openai/gpt-5-nano";
/// The sentinel the pi pane is directed to create. Distinctive + lands in the
/// per-test fixture cwd, so it is unique by construction — a concrete
/// secondary signal that the real model actually ran the directed work (the
/// load-bearing assertion is the rendered-grid identity + status transition,
/// which does not depend on the sentinel).
const SENTINEL_NAME: &str = "pi_live_sentinel_4b1a.txt";
const SENTINEL_CONTENT: &str = "PI_LIVE_SENTINEL_OK";

/// `pi` on PATH AND a non-empty `OPENROUTER_API_KEY`. The key is only checked
/// for presence — never printed or logged (it is a secret). Mirrors
/// `e2e_pi_orchestrator.rs::check_pi_available`.
fn check_pi_available() -> Result<(), String> {
    let ok = std::process::Command::new("pi")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return Err("pi CLI not installed (could not invoke `pi --version`)".into());
    }
    match std::env::var("OPENROUTER_API_KEY") {
        Ok(k) if !k.trim().is_empty() => Ok(()),
        _ => Err("OPENROUTER_API_KEY not set — real-pi live-pane e2e needs OpenRouter auth".into()),
    }
}

/// PATH for the spawned deck (→ daemon → pi child) with the freshly-built
/// `dot-agent-deck` binary's dir prepended to the host PATH, so the extension's
/// `dot-agent-deck agent-event` resolves. `CARGO_BIN_EXE_dot-agent-deck` is set
/// by Cargo at integration-test build time to the binary under test.
fn path_with_binary_dir() -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bin_dir = std::path::Path::new(bin)
        .parent()
        .expect("test binary has a parent dir")
        .to_str()
        .expect("bin dir is UTF-8");
    format!("{bin_dir}:{}", std::env::var("PATH").unwrap_or_default())
}

/// Scenario: Materialize the bundled Pi orchestrator extension into the
/// per-test HOME, then launch the REAL `dot-agent-deck` binary (via the vt100
/// `TuiDeck` harness) with `DOT_AGENT_DECK_EXPERIMENTAL=1` and a restored saved
/// session whose one pane runs a REAL interactive `pi`
/// (`--provider openrouter --model openai/gpt-5-nano --approve`) with a
/// directive initial prompt to create the uniquely-named sentinel
/// `pi_live_sentinel_4b1a.txt`. The `OPENROUTER_API_KEY` + the built-binary PATH
/// are threaded into the deck (inherited by the lazy-spawned daemon and the pi
/// child). The pane auto-focuses on restore (the cast captures pi booting +
/// working live in the pane); detach to the dashboard with Ctrl+D so the Pi
/// pane's CARD renders. Assert on the rendered vt100 grid that, driven ONLY by
/// the extension's `agent-event` reports (NO hook installed): the card shows a
/// real status TRANSITION — `Needs Input` (extension `session_start`→waiting)
/// then `Thinking` (extension `agent_start`→running) — and its title carries the
/// experimental-gated first-class Pi identity (`Pi ·`). Best-effort (logged, not
/// gating): the directed sentinel file appears in the pane cwd. PTY-attached, so
/// it records a `full-stream.cast` (reel-eligible, PRD #180); flaky-tolerant
/// (real LLM) — run once, not looped.
#[spec("pi/live/001")]
#[test]
fn pi_live_001_live_pane_shows_identity_and_status() {
    // Decision 26 runtime-skip: a missing CLI / credential is an environmental
    // condition, not a broken test.
    skip_unless!(check_pi_available());

    // The pi child inherits the deck's env. Directive prompt: instruct pi to do
    // ONE simple, reliable task (create the distinctive sentinel) so the real
    // `agent_start`→running status fires. Single-quoted in the shell-wrapped
    // command; contains no apostrophes, double-quotes, or backticks (a backtick
    // would trigger shell command substitution before pi ever saw it).
    let directive = format!(
        "Use the shell tool to create a file named {SENTINEL_NAME} in the current directory \
         whose entire contents are exactly the text {SENTINEL_CONTENT}. Do that one task, then \
         stop."
    );
    let pi_command = format!("pi --provider openrouter --model {PI_MODEL} --approve '{directive}'");

    let deck = TuiDeck::builder()
        // 200 cols so the dashboard renders the card (left) AND the live pi pane
        // (right) side by side — the reel clip shows both — and the Normal-mode
        // bar keeps full labels (PRD #127).
        .with_pty_size(200, 50)
        // Gate ON so the first-class Pi identity renders (features::show_pi_agent).
        .with_env("DOT_AGENT_DECK_EXPERIMENTAL", "1")
        // pi authenticates to OpenRouter with this (never printed); the deck's
        // daemon and the pi child inherit it.
        .with_env(
            "OPENROUTER_API_KEY",
            std::env::var("OPENROUTER_API_KEY").expect("checked non-empty by check_pi_available"),
        )
        // Put the freshly-built binary's dir on PATH so the extension resolves
        // `dot-agent-deck agent-event`; preserves the host PATH so `pi` still
        // resolves. Wins over the harness env scrub.
        .with_env("PATH", path_with_binary_dir())
        // Stage the bundled extension into the per-test HOME BEFORE launch so the
        // startup-restored pi pane loads it at boot.
        .with_pi_extension()
        // Auto-open one pane running the real interactive pi against the fixture
        // cwd (the deck restores it on launch; PRD #89 no `--continue` flag). An
        // EMPTY pane name is deliberate: a user who opens a Pi pane without
        // naming it sees the AGENT-TYPE identity as the card title, which is
        // exactly the gated `Pi · <id>` surface under test. A non-empty name
        // would flow into `ui.display_names` and title the card with that name
        // instead, hiding the identity.
        .with_continue_session("", &pi_command)
        .launch_with_fixture("minimal");

    // The restored pane auto-focuses: the bottom bar shows the focused-pane
    // Command-Mode affordance. This is the reel's "live pi in the pane" moment
    // (pi boots + processes the directive in the focused terminal).
    deck.wait_for_string("[Command Mode Ctrl+D]");

    // Detach to the dashboard so the Pi pane's CARD renders. Done early (before
    // pi finishes booting) so every status transition re-renders on the card and
    // lands in the rolling byte history. In dashboard mode at 200 cols the live
    // pi pane keeps rendering on the right while its card shows on the left.
    deck.send_bytes(b"\x04"); // Ctrl+D → dashboard / Normal mode
    deck.wait_for_string("Dir:"); // a dashboard card is now rendered

    // --- Load-bearing assertion: a REAL, extension-driven status TRANSITION on
    // the rendered grid, no hook. Scanned over the rolling byte history (from
    // offset 0) so a transient status frame that the next one overwrites still
    // matches. Generous ceilings (Design Decision #7): pi's Node/Bun boot +
    // extension load + a model round-trip, sized to confidence not token cost.
    // Each ceiling stays under nextest's default-profile 180s terminate-after so
    // a failing wait still surfaces its diagnostic panic below rather than being
    // SIGKILL'd mid-wait; on success (the expected path) both return in seconds.

    // Self-diagnosing helper: distinguish "the model/account is the blocker"
    // from "the extension status path is broken".
    let api_errored = |grid: &str| {
        let lower = grid.to_lowercase();
        ["quota", "exceeded", "billing", "unauthorized", "rate limit"]
            .iter()
            .any(|k| lower.contains(k))
    };

    // 1. `Needs Input` — the extension's `session_start`→waiting report. This is
    //    a deck-specific status label (not something pi prints), so seeing it on
    //    the grid proves the extension shelled `dot-agent-deck agent-event
    //    --type waiting` and the card status followed — with NO hook. Fail fast
    //    (assert before starting wait #2) so a single failing wait surfaces its
    //    diagnostic under nextest's 180s terminate rather than being SIGKILL'd.
    if !deck.wait_for_stream_string_within("Needs Input", Duration::from_secs(150)) {
        let grid = deck.snapshot_grid();
        panic!(
            "the Pi pane's card never showed the `Needs Input` status within 150s — the \
             extension's `agent-event --type waiting` (session_start) status path did not reach \
             the card. api_error_on_grid={} (if true, an account/quota is the blocker, not the \
             status path).\nFinal grid:\n{grid}",
            api_errored(&grid)
        );
    }

    // 2. `Thinking` — the extension's `agent_start`→running report once the
    //    directive turn begins. Together with (1) this is the real WAITING →
    //    RUNNING transition.
    if !deck.wait_for_stream_string_within("Thinking", Duration::from_secs(150)) {
        let grid = deck.snapshot_grid();
        panic!(
            "the Pi pane's card never showed the `Thinking` status within 150s — the extension's \
             `agent-event --type running` (agent_start) report never rendered, so the real \
             WAITING → RUNNING transition was not observed. api_error_on_grid={}.\n\
             Final grid:\n{grid}",
            api_errored(&grid)
        );
    }

    // --- Pi first-class IDENTITY on the card (experimental-flag-gated render
    // seam). By now the extension's agent-events have upgraded the card's
    // agent_type to Pi, and with the flag ON the card title reads `Pi · <id>`.
    // The `Pi ·` marker is the AgentType Display identity — the lowercase `pi`
    // command never produces a capital-`Pi` title, so this pins the gated Pi
    // surface specifically.
    let grid = deck.snapshot_grid();
    assert!(
        grid.contains("Pi ·"),
        "the Pi pane's card title never showed the experimental-gated Pi identity (`Pi ·`) on \
         the rendered grid, even though the extension drove real status transitions — the \
         gated `features::show_pi_agent` render seam did not surface the Pi identity.\n\
         Final grid:\n{grid}"
    );

    // --- Best-effort (logged, not gating): the directed sentinel appears in the
    // pane cwd (the fixture root the continue-session pane runs in). Too
    // LLM/tool-variance-dependent to hard-gate; the rendered status transition
    // above already proves the pi turn ran.
    let sentinel = deck.workdir().join(SENTINEL_NAME);
    let saw_sentinel = common::wait_for_path(&sentinel, Duration::from_secs(60));
    eprintln!("live-pi soft signal: sentinel {SENTINEL_NAME:?} created = {saw_sentinel}");
}
