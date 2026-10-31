#![cfg(all(feature = "e2e", feature = "e2e-live"))]

//! L2 PTY-attached REAL-`pi` live-pane showcases (PRD #201, CLAUDE.md rule 4 +
//! demo-reel eligibility per PRD #180). These are the Pi feature's "AS A USER
//! ACTUALLY USES AND SEES IT" tests: the REAL `dot-agent-deck` binary driven
//! through the vt100/`TuiDeck` harness, with a REAL `pi` agent booting and
//! working LIVE in a pane.
//!
//! * `pi/live/001` — a single live Pi pane whose card renders the first-class Pi
//!   IDENTITY plus a REAL, extension-driven status TRANSITION on the vt100 grid,
//!   with NO Claude-Code hook installed.
//! * `pi/live/002` — GAP #1 / PRD #201: proves a REAL `pi` orchestrator receives
//!   its seed through the inbox (`StartAgent.seed` and the bundled extension's
//!   `pi.sendUserMessage`) and drives a full orchestration LIVE (pi → native
//!   `delegate` → real `claude` Haiku worker → sentinel + work-done). See its own
//!   section below.
//! * `pi/live/003` — issue #1385: the extension shells the deck that spawned it
//!   (`DOT_AGENT_DECK_EXE`), not a decoy `dot-agent-deck` placed first on PATH.
//!   No prompt, so no model call and no credential.
//!
//! ## Why this is separate from `e2e_pi_orchestrator.rs`
//! The `chain-smoke/pi/001` + `scheduler/pi/001` tests are HEADLESS: an
//! in-process daemon / `daemon serve` with real pi PTYs, asserting on
//! files/events/broadcasts. They are functionally correct but record NO cast
//! (only PTY-attached `TuiDeck` runs record a `full-stream.cast`, PRD #180) and
//! never exercise the live-in-the-pane, rendered-grid surface CLAUDE.md rule 4 requires.
//! This test closes that gap: it drives the real binary through the vt100
//! harness (mirroring `e2e_issue_dispatch_real.rs` / `e2e_chain_smoke_claude.rs`
//! and the catalog reference `scheduler/dispatch/013`), so the Pi surface a user
//! sees is what is asserted AND what the reel clip captures.
//!
//! ## `pi/live/001` scenario: a single live Pi pane (deterministic status)
//! A reliable single live Pi pane with a visible real status fully satisfies
//! CLAUDE.md rule 4 on its own. A single directive-prompted Pi pane's lifecycle
//! (`session_start`→Idle, `agent_start`→running/Thinking) is the deterministic
//! part — it fires regardless of what the model decides — so the status
//! transition there does not hinge on a multi-agent LLM chain. The heavier orchestrator
//! route is pinned headless by `chain-smoke/pi/001` and live through the inbox
//! by `pi/live/002` below.
//!
//! ## What makes the rendered assertion genuine (not a plumbing stand-in)
//! - **Experimental flag ON** (`DOT_AGENT_DECK_EXPERIMENTAL=1`): the Pi
//!   first-class identity is gated behind `features::show_pi_agent()` at the
//!   render seam (CLAUDE.md rule 9). With the flag ON the card title reads
//!   `Pi · <id>`; without it the Pi identity would be suppressed and the reel
//!   clip would show no Pi surface. The deck reads the flag from its env.
//! - **Bundled extension auto-materialized at daemon startup** (PRD #201):
//!   the per-test HOME starts WITHOUT the extension; because this drives the REAL
//!   binary, the deck's lazy-spawned daemon runs the `daemon serve` entry, whose
//!   daemon-startup auto-materialize writes the bundled extension into the
//!   daemon's HOME (this per-test HOME, which the pi child inherits) BEFORE pi
//!   boots, so pi auto-discovers `~/.pi/agent/extensions/dot-agent-deck/` and
//!   loads it — the SAME production seam a real user hits, with no manual
//!   `with_pi_extension` staging.
//! - **Status via `agent-event`, NO hook**: the Pi pane's card status is driven
//!   ONLY by the extension shelling `dot-agent-deck agent-event` (mapped
//!   `running`→Thinking / `finished`→Idle). No `~/.claude/settings.json`
//!   is touched — a Pi pane is hook-safe by construction.
//!
//! ## Credentials (Design Decision #5, harness caveat)
//! pi runs on `common::pi_test_model()` — by default `openai-codex/gpt-5.6-luna`
//! (PRD #1666), overridable with `DOT_AGENT_DECK_PI_TEST_MODEL` — and
//! authenticates from the host's stored login for that provider, which
//! `with_imported_pi_credentials` copies (that entry only) into the per-test
//! HOME's `.pi/agent`; the deck's lazy-spawned daemon and the pi child inherit
//! that HOME. The freshly-built binary dir on PATH is threaded in explicitly via
//! `with_env`. No credential is printed.
//!
//! Tier: lane 2 — spawns a real agent, hits a real model. Flaky-tolerant (real
//! LLM), run once, not looped (CLAUDE.md rules 4/5). Runtime-skipped (Decision 26) when
//! `pi` or its credential is absent; a model the credential refuses fails
//! instead (`common::check_pi_available`).

use std::process::Stdio;
use std::time::Duration;

mod common;

use common::TuiDeck;
use dot_agent_deck::config;
use spec::spec;

/// The sentinel the pi pane is directed to create. Distinctive + lands in the
/// per-test fixture cwd, so it is unique by construction — a concrete
/// secondary signal that the real model actually ran the directed work (the
/// load-bearing assertion is the rendered-grid identity + status transition,
/// which does not depend on the sentinel).
const SENTINEL_NAME: &str = "pi_live_sentinel_4b1a.txt";
const SENTINEL_CONTENT: &str = "PI_LIVE_SENTINEL_OK";

/// The text of the Pi card's own rectangle on `grid` — every row from the one
/// carrying its `Pi ·` title down to its bottom border, cut at the card's
/// right edge — or `None` while no complete card is drawn. Issue #622: the
/// live pi pane renders to the right of the card on the same rows and prints
/// its own tool calls, so a whole-grid search could not tell the card's tool
/// line from pi's.
fn pi_card_text(grid: &str) -> Option<String> {
    let rows: Vec<Vec<char>> = grid.lines().map(|l| l.chars().collect()).collect();
    let (top, title_col) = rows.iter().enumerate().find_map(|(i, row)| {
        let text: String = row.iter().collect();
        let byte = text.find("Pi ·")?;
        Some((i, text[..byte].chars().count()))
    })?;
    let border = common::BORDER_WEIGHTS
        .iter()
        .find(|w| rows[top][..title_col].contains(&w.top_left))?;
    let right = (title_col..rows[top].len()).find(|&c| rows[top][c] == border.top_right)?;
    let mut card = Vec::new();
    for row in &rows[top..] {
        let edge = *row.get(right)?;
        card.push(row[..=right].iter().collect::<String>());
        if edge == border.bottom_right {
            return Some(card.join("\n"));
        }
        if edge != border.vertical && edge != border.top_right {
            return None;
        }
    }
    None
}

/// The completed-tool count on a card's bottom border (`Tools: N`).
fn card_tool_count(card: &str) -> Option<u32> {
    let (_, rest) = card.split_once("Tools: ")?;
    rest.chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

/// Scenario: Launch the REAL `dot-agent-deck` binary (via the vt100 `TuiDeck`
/// harness) with `DOT_AGENT_DECK_EXPERIMENTAL=1` and a restored saved session
/// whose one pane runs a REAL interactive `pi` — the per-test HOME starts
/// WITHOUT the extension and the deck's daemon-startup auto-materialize (the real
/// `daemon serve` entry the lazy-spawned daemon runs) puts the bundled Pi
/// orchestrator extension into that HOME before pi boots. The pane runs `pi`
/// (`common::pi_test_model()`, `--approve`) with a directive initial
/// prompt to create the uniquely-named sentinel
/// `pi_live_sentinel_4b1a.txt`. Pi's login is seeded into the per-test HOME and
/// the built-binary PATH is threaded into the deck (inherited by the lazy-spawned daemon and the pi
/// child). The pane auto-focuses on restore (the cast captures pi booting +
/// working live in the pane); detach to the dashboard with Ctrl+D so the Pi
/// pane's CARD renders. Assert on the rendered vt100 grid that, driven ONLY by
/// the extension's `agent-event` reports (NO hook installed): the card shows a
/// real, extension-driven status TRANSITION — `Thinking` (extension
/// `agent_start`→running), an Idle → running transition the daemon never
/// produces on its own for a hook-less Pi pane, then a settle back to `Idle`
/// (extension `agent_settled`→finished, the mapping this fix changed — a
/// regression to "Needs Input" would show here) — and its title carries the
/// experimental-gated first-class Pi identity (`Pi ·`). The settled card also
/// carries the detail every other native integration's card does (issue #622):
/// `Dir:`, the submitted prompt (`Prmt:`), the `bash — <command>` call Pi ran,
/// and `Tools: N` with N > 0. Best-effort (logged, not
/// gating): the directed sentinel file appears in the pane cwd. PTY-attached, so
/// it records a `full-stream.cast` (reel-eligible, PRD #180); flaky-tolerant
/// (real LLM) — run once, not looped.
#[spec("pi/live/001")]
#[test]
fn pi_live_001_live_pane_shows_identity_and_status() {
    // Decision 26 runtime-skip: a missing CLI / credential is an environmental
    // condition, not a broken test.
    skip_unless!(common::check_pi_available());

    // The pi child inherits the deck's env. Directive prompt: instruct pi to do
    // ONE simple, reliable task (create the distinctive sentinel) so the real
    // `agent_start`→running status fires. Single-quoted in the shell-wrapped
    // command; contains no apostrophes, double-quotes, or backticks (a backtick
    // would trigger shell command substitution before pi ever saw it).
    let directive = format!(
        "Use the shell tool to run pwd, then create a file named {SENTINEL_NAME} in exactly the \
         directory it prints, whose entire contents are exactly the text {SENTINEL_CONTENT}. Do \
         that one task, then stop."
    );
    let pi_command = format!(
        "pi {} --approve '{directive}'",
        common::pi_test_model().cli_args()
    );

    let deck = TuiDeck::builder()
        // 200 cols so the dashboard renders the card (left) AND the live pi pane
        // (right) side by side — the reel clip shows both — and the Normal-mode
        // bar keeps full labels (PRD #127).
        .with_pty_size(200, 50)
        // Gate ON so the first-class Pi identity renders (features::show_pi_agent).
        .with_env("DOT_AGENT_DECK_EXPERIMENTAL", "1")
        // pi's login for `common::pi_test_model()`'s provider, seeded into the
        // per-test HOME the deck's daemon and the pi child inherit (never
        // printed).
        .with_imported_pi_credentials()
        // Put the freshly-built binary's dir on PATH so the extension resolves
        // `dot-agent-deck agent-event`; preserves the host PATH so `pi` still
        // resolves. Wins over the harness env scrub. (This PATH — inherited by
        // the deck's lazy-spawned daemon — is also where the daemon-startup
        // auto-materialize finds `pi`, so the detector fires; no manual
        // `with_pi_extension`.)
        .with_env("PATH", common::path_with_binary_dir())
        // NOTE: no `with_pi_extension()` — the per-test HOME starts WITHOUT the
        // extension. The deck's lazy-spawned daemon runs the real `daemon serve`
        // entry, whose daemon-startup auto-materialize (PRD #201) writes the
        // bundled extension into the daemon's HOME (this per-test HOME, which the
        // pi child inherits) before pi boots — the real production flow.
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

    // Load-bearing: `Thinking` — the extension's `agent_start`→running report
    // once the directive turn begins. `Thinking` is a deck-specific status label
    // that the daemon NEVER produces on its own for a hook-less Pi pane (a
    // freshly-spawned pane defaults to Idle), so seeing it on the grid proves the
    // extension shelled `dot-agent-deck agent-event --type running` and the card
    // status followed — with NO hook. This is the Idle → RUNNING transition;
    // `session_start` now reports Idle for parity with the other backends
    // (Claude/OpenCode/Codex show Idle, not "Needs Input", on turn-end and
    // session-start), so `Thinking` — not the retired `Needs Input` — is the
    // extension-only proof.
    if !deck.wait_for_stream_string_within("Thinking", Duration::from_secs(150)) {
        let grid = deck.snapshot_grid();
        panic!(
            "the Pi pane's card never showed the `Thinking` status within 150s — the extension's \
             `agent-event --type running` (agent_start) report never rendered, so the real \
             Idle → RUNNING transition was not observed. api_error_on_grid={} (if true, an \
             account/quota is the blocker, not the status path).\nFinal grid:\n{grid}",
            api_errored(&grid)
        );
    }

    // Load-bearing: post-settlement `Idle` — the extension's `agent_settled`
    // →finished report once the directive turn ("create the sentinel, then
    // stop") completes. THIS is the mapping this fix changed (`agent_settled`
    // used to report `waiting`→"Needs Input"); the `Thinking` assertion above
    // only exercises the unchanged `agent_start`→running path, so without this a
    // regression back to `waiting` would slip through. Scanned on the CURRENT
    // reconstructed grid (NOT the from-offset-0 byte history): by the time
    // `Thinking` was observed the card has already advanced past the initial
    // session-start frame, so a `Idle` seen on the live grid now can ONLY be the
    // settled state — correct by construction, no false-pass on the boot frame.
    // If the mapping regressed, the settled card would read "Needs Input" and
    // this would (correctly) fail. Generous ceiling: the model turn must finish.
    if !deck.wait_for_grid_string_within("Idle", Duration::from_secs(150)) {
        let grid = deck.snapshot_grid();
        panic!(
            "the Pi pane's card never settled back to `Idle` within 150s after the turn began — \
             the extension's `agent-event --type finished` (agent_settled) report never rendered, \
             so the changed turn-end → Idle mapping was not observed live (a regression to \
             `waiting`/\"Needs Input\" would show here). api_error_on_grid={} (if true, an \
             account/quota is the blocker, not the status path).\nFinal grid:\n{grid}",
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

    // --- Issue #622: the SETTLED card carries the same detail every other
    // native integration's card does — its directory, the submitted prompt
    // (`Prmt:`), the tool Pi ran (`bash — <command>`), and a completed-tool
    // count above zero — beside the Pi identity and the final `Idle`. Before
    // the fix the extension reported lifecycle only, so the card kept `Dir:`
    // from the spawn placeholder, had no `Prmt:` row and read `Tools: 0` even
    // though Pi visibly ran its shell tool in the pane. Read off the CARD's
    // own rectangle, not the whole grid, because the live pi pane renders on
    // the right of the same grid and prints its own tool calls. The directive
    // names the shell tool, so a turn that never calls one is the model
    // ignoring a directive, not a deck defect — the panic says which.
    let settled = std::cell::RefCell::new(String::new());
    let complete = deck.wait_for_grid_predicate_within(Duration::from_secs(30), |grid| {
        let Some(card) = pi_card_text(grid) else {
            return false;
        };
        let ok = card.contains("Dir:")
            && card.contains("Prmt:")
            && card.contains("bash —")
            && card.contains("Idle")
            && card_tool_count(&card).is_some_and(|n| n > 0);
        settled.replace(card);
        ok
    });
    assert!(
        complete,
        "the settled Pi card does not carry the prompt and tool detail every other native \
         integration's card does: expected `Dir:`, `Prmt:`, a `bash — …` tool line, `Idle` and \
         `Tools: N` with N > 0 on the card. sentinel_created={} (false means the model never ran \
         the directed shell tool, so a missing tool line is not evidence against the extension).\n\
         Card:\n{}\nFinal grid:\n{}",
        deck.workdir().join(SENTINEL_NAME).exists(),
        settled.borrow(),
        deck.snapshot_grid()
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
// pi/live/002 — Pi receives its restored inbox seed, driven live.
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
/// through the daemon inbox. `clear = false` on the worker so the
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
/// re-resolves the config from `project_dir` + `config_name` and spawns both
/// role panes IDLE. The deck posts `orchestrator_prompt` as the first inbox
/// message through `StartAgent.seed`; Pi's extension receives it through its
/// native stream. Built from the real `config` types the deck deserializes.
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
        // PRD #196: `SavedSession` carries the global `last_command`; a restored
        // session with no prior command persists `None` (matches every other
        // `SavedSession` literal — src/config.rs).
        last_command: None,
        // Issue #949: this staged snapshot is about the orchestration rebuild,
        // not about where the user was looking.
        focus: None,
    };
    toml::to_string_pretty(&session).expect("serialize orchestration session.toml")
}

/// Scenario: Restore a real Pi orchestrator and Claude Haiku worker into a PTY-attached deck, with the directive posted to Pi's inbox. Pi must delegate natively, the worker card must visibly label its inbox task with the envelope, and the worker must create the sentinel and signal work-done.
#[spec("pi/live/002")]
#[test]
fn pi_live_002_native_seeded_orchestration_delegates_live() {
    // Decision 26 runtime-skip: a missing CLI / credential is an environmental
    // condition, not a broken test. Needs BOTH a real pi (orchestrator) and a
    // real claude (worker).
    skip_unless!(common::check_pi_available());
    skip_unless!(common::check_claude_available());

    // A held tempdir carrying the orchestration project dir (`.dot-agent-deck.toml`
    // + the sentinel / work-done land here) and the staged `session.toml`. A fresh
    // tree → the sentinel name is unique by construction. Canonicalized so the
    // restore path's `project_path == saved_dir` guard passes and the claude
    // worker's pre-seeded project-trust key matches its actual cwd exactly.
    let orch_root = common::harness_tempdir().expect("orchestration root tempdir");
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
    let _ = common::fixture_git(&project_dir, &project_dir)
        .arg("init")
        .arg("--quiet")
        .status();

    // Idle role commands leave delivery to the inbox. Same real-agent tiers as
    // `chain-smoke/pi/001`.
    let pi_command = format!("pi {} --approve", common::pi_test_model().cli_args());
    let worker_command = format!("claude --model {WORKER_MODEL} --allowedTools Bash Read Write");

    // The inbox directive carries literal sentinel tokens so the assertion
    // survives variation in the model's phrasing.
    let directive = format!(
        "You are the orchestrator in a dot-agent-deck orchestration. A worker role named \
         {ORCH_WORKER_ROLE} is available. Call the delegate tool EXACTLY ONCE with role \
         {ORCH_WORKER_ROLE} and this task text: Create the file {project_str}/{ORCH_SENTINEL_NAME} whose entire contents are exactly the text \
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
        // pi's login, as for `pi_live_001` above.
        .with_imported_pi_credentials()
        // Put the freshly-built binary's dir on PATH so the pi extension resolves
        // `dot-agent-deck agent-event`/`delegate`/`inbox wait` and the claude worker
        // resolves `dot-agent-deck work-done`; preserves the host PATH so `pi` /
        // `claude` still resolve. Wins over the harness env scrub.
        .with_env("PATH", common::path_with_binary_dir())
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
        // Claude clears its first-run onboarding and trust gates before its
        // inbox task arrives.
        .with_claude_project_trust(project_str.clone())
        // NOTE: no `with_pi_extension()` — the per-test HOME starts WITHOUT the
        // extension. The deck's lazy-spawned daemon runs the real `daemon serve`
        // entry, whose daemon-startup auto-materialize (PRD #201) writes the
        // bundled extension into the daemon's HOME (this per-test HOME, which the
        // pi child inherits) before pi boots — the real production flow.
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

    // Receipt is observable in the worker pane as the framed task, rather
    // than the retired fixed worker-task-coder.md file.
    assert!(
        deck.wait_for_grid_string_within(ORCH_SENTINEL_NAME, Duration::from_secs(150)),
        "Pi never delegated its inbox directive to the worker; api_error_on_grid={}. Final grid:\n{}",
        api_errored(&deck.snapshot_grid()),
        deck.snapshot_grid()
    );

    // The worker card labels the framed inbox task while its real agent works.
    assert!(
        deck.wait_for_grid_string_within(
            "✉ task from the orchestrator for coder",
            Duration::from_secs(60)
        ),
        "the worker card never labelled its inbox delegation. Final grid:\n{}",
        deck.snapshot_grid()
    );

    // Full chain — the delegated claude worker created the sentinel (proves the
    // real worker ran the delegated task via the daemon route). Polled on
    // CONTENT, not existence: a shell redirect creates the file before writing
    // into it, so waiting only for the path to appear can read an empty string
    // (PRD #225). Same `contains` semantics as before.
    let sentinel = project_dir.join(ORCH_SENTINEL_NAME);
    if let Err(observed) =
        common::wait_for_file_containing(&sentinel, ORCH_SENTINEL_CONTENT, Duration::from_secs(90))
    {
        panic!(
            "the delegated claude worker never created the sentinel {ORCH_SENTINEL_NAME:?} with \
             {ORCH_SENTINEL_CONTENT:?} within 90s. The inbox task was visible to the worker, so the worker ran but did not complete the task. observed: {observed}; \
             api_error_on_grid={}.\nFinal grid:\n{}",
            api_errored(&deck.snapshot_grid()),
            deck.snapshot_grid()
        );
    }

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

// ---------------------------------------------------------------------------
// pi/live/003 — the extension shells the deck that spawned it, not whatever
// `dot-agent-deck` Pi's own PATH resolves (issue #1385, following #549).
// ---------------------------------------------------------------------------

/// `pi` on PATH, nothing else: `pi/live/003` sends no prompt, so pi makes no
/// model call and needs no credential — its `session_start` fires at boot
/// whether or not it is authenticated. Unix-only, like the one test using it.
#[cfg(unix)]
fn check_pi_installed() -> Result<(), String> {
    let ok = std::process::Command::new("pi")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err("pi CLI not installed (could not invoke `pi --version`)".into())
    }
}

/// Scenario: Put a DECOY `dot-agent-deck` first on the deck's PATH — a script
/// that only appends its arguments to a log and exits 0, standing in for an
/// older or unrelated install an agent's shell could find first — and leave
/// the freshly built binary OFF the PATH. Launch the real deck with
/// `DOT_AGENT_DECK_EXPERIMENTAL=1` and a restored pane running a bare `pi`
/// with no prompt, then detach to the dashboard. Pi's `session_start` makes the
/// bundled extension report status and open its inbox stream; because the deck exports
/// its own absolute path in `DOT_AGENT_DECK_EXE`, those calls reach the real
/// deck, so the card takes the `Pi ·` identity that only an `agent-event` from
/// the extension can give it, and the decoy is never run.
#[spec("pi/live/003")]
#[cfg(unix)]
#[test]
fn pi_live_003_extension_shells_the_spawning_deck_not_the_path() {
    use std::os::unix::fs::PermissionsExt;

    skip_unless!(check_pi_installed());

    let decoy_root = common::harness_tempdir().expect("create decoy dir");
    let decoy_bin = decoy_root.path().join("bin");
    std::fs::create_dir_all(&decoy_bin).expect("create decoy bin dir");
    let decoy_log = decoy_root.path().join("decoy-invocations.log");
    let decoy = decoy_bin.join("dot-agent-deck");
    std::fs::write(
        &decoy,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
            decoy_log.display()
        ),
    )
    .expect("write decoy dot-agent-deck");
    std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o755))
        .expect("make decoy executable");

    // The decoy first, then the host PATH (so `pi` resolves) with the built
    // binary's directory deliberately NOT added — unlike `pi/live/001`, which
    // prepends it and so could not tell the two lookups apart.
    let path = format!(
        "{}:{}",
        decoy_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let deck = TuiDeck::builder()
        .with_pty_size(200, 50)
        .with_env("DOT_AGENT_DECK_EXPERIMENTAL", "1")
        .with_env("PATH", path)
        // Empty pane name, so the card title is the agent-type identity.
        .with_continue_session("", "pi")
        .launch_with_fixture("minimal");

    deck.wait_for_string("[Command Mode Ctrl+D]");
    // Subscribe as soon as the pane exists (so the daemon's attach socket does
    // too) and before pi has finished booting — its Node start and extension
    // load take seconds — so the extension's first report is not missed.
    let events = deck.subscribe_events();
    deck.send_bytes(b"\x04"); // Ctrl+D → dashboard
    deck.wait_for_string("Dir:");

    // What only the extension's `agent-event` can put on the daemon's wire: a
    // Pi frame carrying the daemon-provided agent id under the CLI's `<pane>-session`
    // key. The card-surfacing `SessionStart` the daemon emits at spawn is also
    // typed Pi (from the command), but keys on the bare pane id and carries no
    // agent id, so it cannot satisfy this.
    let from_extension = events.try_wait_for(
        |e| {
            e.agent_type == dot_agent_deck::event::AgentType::Pi
                && e.agent_id.is_some()
                && e.session_id.ends_with("-session")
        },
        Duration::from_secs(120),
    );
    let decoy_calls = std::fs::read_to_string(&decoy_log).unwrap_or_default();
    assert!(
        decoy_calls.is_empty(),
        "the Pi extension ran the `dot-agent-deck` on PATH instead of the deck that spawned it \
         (DOT_AGENT_DECK_EXE); the decoy received:\n{decoy_calls}"
    );
    assert!(
        from_extension.is_some(),
        "no `agent-event` from the Pi extension reached the deck within 120s, so it did not \
         shell the deck named by DOT_AGENT_DECK_EXE. Broadcast so far: {:?}\nFinal grid:\n{}",
        events.snapshot(),
        deck.snapshot_grid()
    );
    // The user-visible half: the card carries the first-class Pi identity.
    assert!(
        deck.wait_for_grid_string_within("Pi ·", Duration::from_secs(30)),
        "the Pi pane's card never showed the `Pi ·` identity.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}
