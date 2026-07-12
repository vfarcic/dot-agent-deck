#![cfg(feature = "e2e")]

//! L2 PTY-attached REAL-`pi` live-pane showcases (PRD #201, CLAUDE.md rule 4 +
//! demo-reel eligibility per PRD #180). These are the Pi feature's "AS A USER
//! ACTUALLY USES AND SEES IT" tests: the REAL `dot-agent-deck` binary driven
//! through the vt100/`TuiDeck` harness, with a REAL `pi` agent booting and
//! working LIVE in a pane.
//!
//! * `pi/live/001` — a single live Pi pane whose card renders the first-class Pi
//!   IDENTITY plus a REAL, extension-driven status TRANSITION on the vt100 grid,
//!   with NO Claude-Code hook installed.
//! * `pi/live/002` — GAP #1: proves a REAL `pi` orchestrator AUTO-SUBMITS a
//!   daemon-INJECTED seed (the restore-path `write_and_submit_to_pane` replay,
//!   NOT a CLI arg) and drives a full orchestration LIVE (pi → native `delegate`
//!   → real `claude` Haiku worker → sentinel + work-done). See its own section
//!   below.
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
//! ## `pi/live/001` scenario: a single live Pi pane (deterministic status)
//! A reliable single live Pi pane with a visible real status fully satisfies
//! rule 4 on its own. A single directive-prompted Pi pane's lifecycle
//! (`session_start`→waiting, `agent_start`→running) is the deterministic part —
//! it fires regardless of what the model decides — so the status transition
//! there does not hinge on a multi-agent LLM chain. The heavier orchestrator
//! route is pinned headless by `chain-smoke/pi/001` and, LIVE + injection-seeded,
//! by `pi/live/002` below.
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
use dot_agent_deck::config;
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

// ---------------------------------------------------------------------------
// pi/live/002 — GAP #1: pi AUTO-SUBMITS a daemon-INJECTED seed, driven LIVE
// through a full orchestration reel clip (PRD #201, phase-5 parity gap).
// ---------------------------------------------------------------------------

/// Pinned cheap Claude model for the worker role (same tier `chain-smoke/pi/001`
/// uses — proven directive-following + cheap).
const WORKER_MODEL: &str = "claude-haiku-4-5-20251001";
/// The `[[orchestrations]] name` in the staged `.dot-agent-deck.toml` — the
/// re-resolution key the restore path matches against the saved snapshot.
const ORCH_CONFIG_NAME: &str = "pi-parity";
/// The worker role the pi orchestrator delegates to.
const ORCH_WORKER_ROLE: &str = "coder";
/// The sentinel the delegated worker must create. Distinctive + lands in a
/// fresh tempdir cwd, so it is unique by construction. DISTINCT from the
/// `pi/live/001` and `chain-smoke/pi/001` sentinels.
const ORCH_SENTINEL_NAME: &str = "pi_inject_orch_sentinel_5e8c.txt";
const ORCH_SENTINEL_CONTENT: &str = "PI_INJECT_ORCH_OK";

/// Build the `.dot-agent-deck.toml` declaring the two-role orchestration the
/// restore path re-resolves: `orchestrator` (the START role, a REAL idle `pi`)
/// and `coder` (a REAL idle `claude` Haiku worker). Neither role command
/// carries the seed as a CLI arg — the orchestrator is spawned IDLE and seeded
/// only via the daemon injection path. `clear = false` on the worker so the
/// pre-booted (already-trusted) claude receives the delegated task in place
/// rather than being respawned mid-orchestration.
fn orchestration_config_toml(pi_command: &str, worker_command: &str) -> String {
    format!(
        "[[orchestrations]]\n\
         name = \"{ORCH_CONFIG_NAME}\"\n\n\
         [[orchestrations.roles]]\n\
         name = \"orchestrator\"\n\
         command = \"{pi_command}\"\n\
         start = true\n\n\
         [[orchestrations.roles]]\n\
         name = \"{ORCH_WORKER_ROLE}\"\n\
         command = \"{worker_command}\"\n\
         clear = false\n"
    )
}

/// Serialize a `session.toml` carrying an `OrchestrationSnapshot` for the
/// two-role orchestration rooted at `project_dir`, with `directive` as the
/// snapshot's `orchestrator_prompt`. On the daemon-empty restore path the deck
/// re-resolves the config from `project_dir` + `config_name`, spawns both role
/// panes IDLE, and REPLAYS `orchestrator_prompt` into the START role via
/// `write_and_submit_to_pane` — the exact PRODUCTION injection primitive
/// (single-line write, SUBMIT_DELAY, then a `\r`) whose auto-submit is under
/// test. Built from the real `config` types (not hand-rolled TOML) so it can
/// never drift from the struct the deck deserializes.
fn orchestration_session_toml(project_dir: &str, pi_command: &str, directive: &str) -> String {
    let session = config::SavedSession {
        panes: vec![config::SavedPane {
            dir: project_dir.to_string(),
            name: "orchestrator".to_string(),
            command: pi_command.to_string(),
            mode: None,
            orchestration: Some(config::OrchestrationSnapshot {
                version: 1,
                roles: vec!["orchestrator".to_string(), ORCH_WORKER_ROLE.to_string()],
                start_role_index: 0,
                orchestrator_prompt: directive.to_string(),
                config_name: ORCH_CONFIG_NAME.to_string(),
                project_path: project_dir.to_string(),
                started_role_indices: vec![0],
                display_title: None,
            }),
        }],
    };
    toml::to_string_pretty(&session).expect("serialize orchestration session.toml")
}

/// Scenario: Stage a two-role orchestration (`.dot-agent-deck.toml`: an
/// `orchestrator` START role running a REAL idle `pi`
/// (`--provider openrouter --model openai/gpt-5-nano --approve`, NO CLI-arg
/// prompt) + a `coder` role running a REAL idle `claude` Haiku worker) and a
/// `session.toml` whose `OrchestrationSnapshot.orchestrator_prompt` is a
/// directive telling pi to call the native `delegate` tool once, handing the
/// `coder` role a task to create the uniquely-named sentinel
/// `pi_inject_orch_sentinel_5e8c.txt`. Launch the REAL `dot-agent-deck` binary
/// through the vt100 `TuiDeck` harness with `DOT_AGENT_DECK_EXPERIMENTAL=1`, the
/// bundled Pi extension materialized into the per-test HOME, imported Claude
/// credentials + project-trust for the orchestration cwd, and
/// `OPENROUTER_API_KEY` + the built-binary PATH threaded in (the key is never
/// printed). On the daemon-empty restore the deck spawns both role panes IDLE
/// and REPLAYS the directive into the pi START role via the PRODUCTION
/// `write_and_submit_to_pane` injection primitive — NOT a CLI arg. AUTO-SUBMIT
/// CHECKPOINT (load-bearing): the daemon writes
/// `.dot-agent-deck/worker-task-coder.md` only inside `handle_delegate`, so its
/// appearance is the isolated proof that pi AUTO-SUBMITTED the daemon-INJECTED
/// seed and called `delegate`. Then assert the user-visible reality on the
/// rendered grid (the delegate pointer `worker-task-coder` rendered live in the
/// worker pane) and confirm the full chain landed: the worker created the
/// sentinel (contents `PI_INJECT_ORCH_OK`) and signalled work-done
/// (`.dot-agent-deck/work-done-coder.md`). PTY-attached, so it records a
/// `full-stream.cast` (reel-eligible, PRD #180); flaky-tolerant (real LLM) —
/// run once, not looped.
#[spec("pi/live/002")]
#[test]
fn pi_live_002_injection_seeded_orchestration_delegates_live() {
    // Decision 26 runtime-skip: a missing CLI / credential is an environmental
    // condition, not a broken test. Needs BOTH a real pi (orchestrator) and a
    // real claude (worker).
    skip_unless!(check_pi_available());
    skip_unless!(common::check_claude_available());

    // A held tempdir carrying the orchestration project dir (`.dot-agent-deck.toml`
    // + the sentinel / work-done land here) and the staged `session.toml`. A fresh
    // tree → the sentinel name is unique by construction. Canonicalized so the
    // restore path's `project_path == saved_dir` guard passes and the claude
    // worker's pre-seeded project-trust key matches its actual cwd exactly.
    let orch_root = tempfile::tempdir().expect("orchestration root tempdir");
    let project_dir = orch_root.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("create orchestration project dir");
    let project_dir = project_dir
        .canonicalize()
        .expect("canonicalize orchestration project dir");
    let project_str = project_dir
        .to_str()
        .expect("orchestration project dir is UTF-8")
        .to_string();
    // Some agent paths probe `.git`; a git repo makes the cwd look like a real
    // project. Best-effort — the orchestration does not depend on it.
    let _ = std::process::Command::new("git")
        .arg("init")
        .arg("--quiet")
        .current_dir(&project_dir)
        .status();

    // The idle role commands — NO CLI-arg prompt on the orchestrator (the whole
    // point: pi is seeded ONLY by injection). Same real-agent tiers as
    // `chain-smoke/pi/001`.
    let pi_command = format!("pi --provider openrouter --model {PI_MODEL} --approve");
    let worker_command = format!("claude --model {WORKER_MODEL} --allowedTools Bash Read Write");

    // The injected seed. Single line (the injection encodes single-line text +
    // SUBMIT_DELAY + CR, no bracketed paste). Directive + literal sentinel
    // tokens so the assertion survives LLM phrasing variance; no quotes /
    // backslashes / apostrophes to keep it TOML- and shell-safe.
    let directive = format!(
        "You are the orchestrator in a dot-agent-deck orchestration. A worker role named \
         {ORCH_WORKER_ROLE} is available. Call the delegate tool EXACTLY ONCE with role \
         {ORCH_WORKER_ROLE} and this task text: Create a file named {ORCH_SENTINEL_NAME} in the \
         current working directory whose entire contents are exactly the text \
         {ORCH_SENTINEL_CONTENT}. Use the Bash tool to create it. Do NOT do the work yourself - \
         only call the delegate tool, then stop."
    );

    std::fs::write(
        project_dir.join(".dot-agent-deck.toml"),
        orchestration_config_toml(&pi_command, &worker_command),
    )
    .expect("write orchestration .dot-agent-deck.toml");
    let session_path = orch_root.path().join("session.toml");
    std::fs::write(
        &session_path,
        orchestration_session_toml(&project_str, &pi_command, &directive),
    )
    .expect("write staged session.toml");

    let deck = TuiDeck::builder()
        // 200 cols so the orchestration tab renders BOTH role panes (pi
        // orchestrator + claude worker) side by side — the reel clip shows the
        // delegation happening live.
        .with_pty_size(200, 50)
        // Gate ON so the Pi first-class identity renders (features::show_pi_agent).
        .with_env("DOT_AGENT_DECK_EXPERIMENTAL", "1")
        // pi authenticates to OpenRouter with this (never printed); the deck's
        // daemon and the pi child inherit it.
        .with_env(
            "OPENROUTER_API_KEY",
            std::env::var("OPENROUTER_API_KEY").expect("checked non-empty by check_pi_available"),
        )
        // Put the freshly-built binary's dir on PATH so the pi extension resolves
        // `dot-agent-deck agent-event`/`delegate` and the claude worker resolves
        // `dot-agent-deck work-done`; preserves the host PATH so `pi` / `claude`
        // still resolve. Wins over the harness env scrub.
        .with_env("PATH", path_with_binary_dir())
        // Point the deck's saved-session reader at our staged orchestration
        // snapshot so the daemon-empty restore rebuilds exactly this
        // orchestration (and nothing from the developer's real session.toml).
        .with_env(
            "DOT_AGENT_DECK_SESSION",
            session_path.to_str().expect("session.toml path is UTF-8"),
        )
        // Real Claude credentials so the daemon-spawned interactive worker
        // authenticates.
        .with_imported_claude_credentials()
        // Pre-trust the orchestration cwd (shared by both roles) so the worker's
        // claude clears its first-run onboarding + trust gates and auto-submits
        // the daemon-injected task pointer without a human keystroke.
        .with_claude_project_trust(project_str.clone())
        // Stage the bundled Pi extension into the per-test HOME BEFORE launch so
        // the startup-restored pi orchestrator pane loads it at boot.
        .with_pi_extension()
        .launch_with_fixture("minimal");

    // The restored orchestration surfaces as its own tab: the tab strip shows
    // the `pi-parity` label the moment the daemon-empty restore rebuilds it —
    // BEFORE the agents boot. Scanned on the RECONSTRUCTED grid (the styled tab
    // label is written as separate runs, so the raw byte stream never carries it
    // contiguously — see `wait_for_grid_string_within`). If this never appears
    // the orchestration failed to surface (a restore/config problem — NOT
    // auto-submit).
    assert!(
        deck.wait_for_grid_string_within(ORCH_CONFIG_NAME, Duration::from_secs(45)),
        "the restored orchestration never surfaced within 45s — expected the daemon-empty restore \
         to rebuild the `{ORCH_CONFIG_NAME}` orchestration tab. This is a restore/config failure, \
         not an auto-submit result.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    // Self-diagnosing helper: distinguish "the model/account is the blocker"
    // from "the auto-submit / delegate path is broken".
    let api_errored = |grid: &str| {
        let lower = grid.to_lowercase();
        ["quota", "exceeded", "billing", "unauthorized", "rate limit"]
            .iter()
            .any(|k| lower.contains(k))
    };

    let worker_task = project_dir
        .join(".dot-agent-deck")
        .join(format!("worker-task-{ORCH_WORKER_ROLE}.md"));

    // ===================== AUTO-SUBMIT CHECKPOINT (GAP #1) =====================
    // The daemon writes `worker-task-coder.md` ONLY inside `handle_delegate` —
    // i.e. only AFTER the pi orchestrator has AUTO-SUBMITTED the daemon-INJECTED
    // seed (the restore-path `write_and_submit_to_pane` replay, NOT a CLI arg)
    // and called the native `delegate` tool. So this file appearing is the
    // isolated proof of GAP #1: pi auto-submits an injected seed and acts on it.
    // If it never appears, pi did NOT auto-submit the injected seed (or did not
    // delegate) — a production-flagship bug, NOT to be worked around with a CLI
    // arg. Ceiling sized to confidence (Design Decision #7): pi boot + the
    // restore readiness gate + a model round-trip + the delegate tool call.
    let delegated = common::wait_for_path(&worker_task, Duration::from_secs(150));
    if !delegated {
        let grid = deck.snapshot_grid();
        let seed_on_grid = grid.contains(ORCH_SENTINEL_NAME)
            || grid.to_lowercase().contains("delegate")
            || grid.contains(ORCH_WORKER_ROLE);
        panic!(
            "AUTO-SUBMIT CHECKPOINT FAILED (PRD #201 GAP #1): the daemon never wrote \
             {worker_task:?} within 150s — the pi orchestrator did not AUTO-SUBMIT the \
             daemon-INJECTED seed and delegate. injected_seed_visible_on_grid={seed_on_grid} (if \
             true, the seed reached pi's pane but was not submitted — an auto-submit failure — or \
             pi chose not to delegate). api_error_on_grid={} (if true, an account/quota is the \
             blocker, not auto-submit).\nFinal grid:\n{grid}",
            api_errored(&grid)
        );
    }

    // The delegation is VISIBLE on the rendered orchestration grid: the daemon
    // injected the single-line pointer `Read .dot-agent-deck/worker-task-coder.md
    // for your task.` into the worker (coder) role pane, which renders it live
    // (and it persists on the worker card's `Prmt:` field). This is the
    // user-visible "delegation happening + worker" reality. Scanned on the
    // RECONSTRUCTED grid so styled card chrome is matched on its rendered text.
    assert!(
        deck.wait_for_grid_string_within("worker-task-coder", Duration::from_secs(60)),
        "the delegate pointer `worker-task-coder` never rendered in the worker pane/card on the \
         orchestration grid, even though the daemon wrote the task file — the delegation did not \
         reach the worker pane visibly. api_error_on_grid={}.\nFinal grid:\n{}",
        api_errored(&deck.snapshot_grid()),
        deck.snapshot_grid()
    );

    // Full chain — the delegated claude worker created the sentinel (proves the
    // real worker ran the delegated task via the daemon route).
    let sentinel = project_dir.join(ORCH_SENTINEL_NAME);
    assert!(
        common::wait_for_path(&sentinel, Duration::from_secs(90)),
        "the delegated claude worker never created the sentinel {ORCH_SENTINEL_NAME:?} within \
         90s. The delegation reached the worker (task file written), so the worker ran but did \
         not complete the task. api_error_on_grid={}.\nFinal grid:\n{}",
        api_errored(&deck.snapshot_grid()),
        deck.snapshot_grid()
    );
    let contents = std::fs::read_to_string(&sentinel).expect("read sentinel file");
    assert!(
        contents.contains(ORCH_SENTINEL_CONTENT),
        "sentinel {ORCH_SENTINEL_NAME:?} exists but does not contain {ORCH_SENTINEL_CONTENT:?}; \
         got:\n{contents}"
    );

    // Full chain — work-done returned to the orchestrator: the daemon wrote the
    // per-role summary file (`handle_work_done`), proving the worker ran
    // `dot-agent-deck work-done` from its task-file footer and the daemon
    // ingested it over the socket.
    let work_done = project_dir
        .join(".dot-agent-deck")
        .join(format!("work-done-{ORCH_WORKER_ROLE}.md"));
    assert!(
        common::wait_for_path(&work_done, Duration::from_secs(60)),
        "the worker never signalled work-done (no {work_done:?}). The sentinel was created, so \
         the worker ran, but the work-done CLI did not reach the daemon.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    // Best-effort (logged, not gating): the sentinel filename surfaced live in
    // the worker pane on the grid — the reel narrative of the worker doing the
    // delegated work. The load-bearing proof is the file assertions above.
    let saw_sentinel_grid =
        deck.wait_for_stream_string_within(ORCH_SENTINEL_NAME, Duration::from_secs(20));
    eprintln!(
        "pi-inject-orch soft signal: sentinel {ORCH_SENTINEL_NAME:?} seen live on grid = {saw_sentinel_grid}"
    );
}
