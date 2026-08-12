#![cfg(feature = "e2e")]

//! L2 PTY-attached reel test for PRD #220 dispatcher mode.
//!
//! Exercises the full user-visible path: launch a default deck (no experimental
//! flag), open the new-pane form, select the "dispatcher" option, submit, give
//! the seeded agent a goal, and verify it really dispatches — the daemon creates
//! the sibling git worktree the feature promises.
//!
//! Marked [reel] — this is the genuine spawn → agent → work path (CLAUDE.md
//! rule 4). The agent receives the dispatcher seed prompt via gated delivery,
//! acts on the goal, and invokes `dot-agent-deck dispatch` itself; the assertion
//! is on the resulting worktree, so nothing here is a stand-in.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::TabMembership;
use dot_agent_deck::state::SessionStatus;
use spec::spec;

/// Removes a dispatch worktree on drop, including on panic.
///
/// Dispatch worktrees are SIBLINGS of the fixture dir, so they land outside the
/// harness tempdir and its `TempDir` drop never touches them — without this every
/// run of this test leaves a `/tmp/.tmpXXXX-dispatch-probe-unit` behind forever.
struct SiblingWorktreeGuard(PathBuf);

impl Drop for SiblingWorktreeGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// PATH for the spawned deck (→ daemon → agents) with the freshly-built
/// `dot-agent-deck` binary's dir prepended to the host PATH.
///
/// Without this the seeded dispatcher agent runs whatever `dot-agent-deck`
/// happens to be installed on the host, which predates the `dispatch` verb — the
/// agent then reports "dispatch doesn't exist as a subcommand" and the test
/// silently proves nothing about the feature. The rest of the host PATH is kept
/// so `git` and `claude` still resolve. Mirrors
/// `e2e_issue_dispatch_real.rs::path_with_binary_dir`.
fn path_with_binary_dir() -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bindir = Path::new(bin).parent().expect("binary path has a parent");
    format!(
        "{}:{}",
        bindir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// Give the fixture repo an initial commit.
///
/// The harness `git init`s the copied fixture but never commits, leaving an
/// unborn HEAD — and `git worktree add` cannot create a worktree from that. A
/// dispatch in such a repo fails on worktree creation, so without this the
/// dispatch path is unreachable no matter what the agent does.
fn commit_fixture_repo(dir: &Path) {
    let run = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git available");
        assert!(out.status.success(), "git {args:?} failed: {out:?}");
    };
    run(&["config", "user.email", "deck-test@example.com"]);
    run(&["config", "user.name", "Deck Test"]);
    run(&["add", "-A"]);
    run(&["commit", "-qm", "fixture baseline"]);
}

/// The sibling worktree a dispatch of `unit` must create — `../<repo>-dispatch-<unit>`.
fn dispatch_worktree_of(deck: &TuiDeck, unit: &str) -> PathBuf {
    deck.workdir()
        .parent()
        .expect("fixture dir has a parent")
        .join(format!(
            "{}-dispatch-{unit}",
            deck.workdir()
                .file_name()
                .expect("fixture dir has a name")
                .to_string_lossy()
        ))
}

/// Open ONE ordinary `cat` pane and return its `DOT_AGENT_DECK_PANE_ID`.
///
/// A dispatch resolves its working dir from the CALLING pane's `AgentRecord.cwd`,
/// so the daemon needs a registered pane to attribute the dispatch to — the same
/// lookup a real dispatcher pane goes through. `cat` is deliberate here and only
/// here: this pane is the *caller*, never the thing under test, it stays alive on
/// stdin, and it costs no tokens.
///
/// The Command field is CLEARED before `cat` is typed. When the deck config sets a
/// `default_command` the form seeds that field from it, so typing would APPEND
/// (`cat` onto a seeded `claude …`), the spawn would fail, and no pane would ever
/// register — which reads exactly like a dispatch bug two assertions later.
/// Backspaces on an already-empty field are harmless, so this is unconditional
/// rather than a flag the caller has to get right.
fn open_cat_caller_pane(deck: &TuiDeck) -> String {
    deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
    deck.send_keys(b" "); // Space → confirm dir → new-pane form
    deck.wait_for_string("New Agent");
    deck.send_keys(b"\t");
    deck.send_keys(b"caller");
    deck.send_keys(b"\t");
    deck.send_keys(&[0x7f; 96]); // clear whatever the config seeded
    deck.send_keys(b"cat");
    let (col, row) = deck
        .find_in_grid("[Submit]")
        .expect("the new-pane form should render a [Submit] button");
    deck.click(col, row);
    deck.wait_for_absence("[Submit]");

    const PANE_WAIT: Duration = Duration::from_secs(60);
    let find_caller = || {
        common::agent_records_on(deck.attach_socket_path())
            .into_iter()
            .find_map(|r| r.pane_id_env.filter(|_| r.cwd.is_some()))
    };
    assert!(
        common::wait_until(PANE_WAIT, || find_caller().is_some()),
        "no registered pane with a cwd appeared within {}s — the dispatch has no \
         caller to resolve.\nRecords: {:?}\nFinal grid:\n{}",
        PANE_WAIT.as_secs(),
        common::agent_records_on(deck.attach_socket_path())
            .iter()
            .map(|r| (r.id.clone(), r.pane_id_env.clone(), r.cwd.clone()))
            .collect::<Vec<_>>(),
        deck.snapshot_grid()
    );
    find_caller().expect("checked above")
}

/// What the daemon knows about ONE role pane of a dispatched orchestration.
///
/// An entry existing means a PANE was spawned and registered — true as soon as
/// `spawn_agent` returns, and it says nothing about whether the process inside
/// that PTY ever became an agent.
#[derive(Debug)]
struct RoleState {
    /// Daemon registry id — the handle `AttachRequest::Snapshot` takes.
    agent_id: String,
    /// The daemon's EVENT-DERIVED live session for that pane. `Some` only once
    /// the thing inside the PTY emitted a real agent event (`SessionStart`), so
    /// this — not the pane's existence — is the "an agent actually started here"
    /// signal.
    live: Option<SessionStatus>,
}

/// Every role pane of orchestration `orch` the daemon currently holds, by role name.
///
/// Reads `ListAgents` rather than the grid because the question is per-ROLE and
/// a card whose agent is still booting renders the same chrome as one whose agent
/// never will.
fn role_states(socket: &Path, orch: &str) -> BTreeMap<String, RoleState> {
    let mut out: BTreeMap<String, RoleState> = BTreeMap::new();
    for record in common::agent_records_on(socket) {
        let Some(TabMembership::Orchestration {
            name, role_name, ..
        }) = record.tab_membership.clone()
        else {
            continue;
        };
        if name != orch {
            continue;
        }
        out.insert(
            role_name,
            RoleState {
                agent_id: record.id.clone(),
                live: record.live.as_ref().map(|l| l.status.clone()),
            },
        );
    }
    out
}

/// Per-role failure diagnostics: for every EXPECTED role, whether it has a pane,
/// whether an agent ever came alive in it, and the tail of what that PTY actually
/// printed.
///
/// The PTY tail is the load-bearing part. "no agent started" has several very
/// different causes — the command was never found, a first-run trust prompt is
/// waiting for a keystroke, the agent crashed on boot — and they are
/// indistinguishable from the daemon's record alone. They are all plainly visible
/// in the pane's own bytes.
fn role_diagnostics(deck: &TuiDeck, orch: &str, expected: &[&str]) -> String {
    let socket = deck.attach_socket_path();
    let found = role_states(socket, orch);
    let mut out = String::new();
    for role in expected {
        match found.get(*role) {
            None => out.push_str(&format!("\n- {role}: NO PANE — never spawned at all\n")),
            Some(state) => {
                out.push_str(&format!(
                    "\n- {role}: pane {}, live={:?}\n",
                    state.agent_id, state.live
                ));
                let text = common::strip_ansi(&common::pane_snapshot_on(socket, &state.agent_id));
                let tail: Vec<&str> = text
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .rev()
                    .take(12)
                    .collect();
                for line in tail.into_iter().rev() {
                    out.push_str(&format!("    | {line}\n"));
                }
            }
        }
    }
    out
}

/// Scenario: Launch the deck in the minimal fixture with real Claude credentials
/// imported and NO experimental flag set. Open the new-pane form (Ctrl+N →
/// Space confirms the dir), cycle the Mode field to the "dispatcher" option
/// (the last cycler slot after `schedule: issues`), and click [Submit] —
/// the dispatcher must surface live as a dashboard card. Then type a goal asking
/// for one unit named `probe-unit`; the seeded agent runs
/// `dot-agent-deck dispatch probe-unit` itself and the daemon creates the sibling
/// worktree `../<repo>-dispatch-probe-unit`, which the test waits for on disk.
#[spec("prompt/new-pane/016")]
#[test]
fn new_pane_016_dispatcher_opens_dashboard_card_with_real_agent() {
    skip_unless!(common::check_claude_available());

    let deck = TuiDeck::builder()
        .with_imported_claude_credentials()
        // Deliberately NO `DOT_AGENT_DECK_EXPERIMENTAL`: the dispatcher option has
        // graduated out of the flag, so reaching it from a default deck is part of
        // what this test pins. Setting the flag here would hide a regression that
        // put the option back behind it.
        // The branch build must win over any host-installed `dot-agent-deck`, or
        // the agent cannot see the `dispatch` verb at all.
        .with_env("PATH", path_with_binary_dir())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    // `git worktree add` needs a real commit to branch from.
    commit_fixture_repo(deck.workdir());

    // Trust the fixture working directory so the daemon-spawned interactive
    // claude clears its first-run onboarding + per-folder trust gates without a
    // human keystroke and the injected dispatcher seed prompt is received.
    //
    // Seeded via the harness helper rather than hand-editing `.claude.json`:
    // `with_imported_claude_credentials` imports CREDENTIALS only, so there is no
    // `~/.claude.json` in the per-test HOME to read — the helper is what creates
    // it (starting from the host's, to preserve `hasCompletedOnboarding`) and then
    // marks each path trusted. Trust both the raw and canonicalized forms, since
    // the agent's own cwd may arrive either way (on macOS the tempdir is a
    // `/var` → `/private/var` symlink).
    let mut trust_paths = vec![deck.workdir().to_string_lossy().into_owned()];
    if let Ok(canonical) = deck.workdir().canonicalize() {
        let canonical = canonical.to_string_lossy().into_owned();
        if !trust_paths.contains(&canonical) {
            trust_paths.push(canonical);
        }
    }
    common::seed_claude_trust_in_home(deck.home_dir(), &trust_paths)
        .expect("seed Claude onboarding and project trust");

    // Open the new-pane form: Ctrl+N → directory picker → Space confirms.
    deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
    deck.send_keys(b" "); // Space → confirm current dir → new-pane form
    deck.wait_for_string("No mode");

    // Cycle to the dispatcher option — it is the LAST slot (after No mode,
    // schedule, schedule: issues). Saturate with enough Rights to reach the
    // end (the cycler caps), so this is robust against future additions.
    deck.send_keys(b"\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C"); // Right ×8
    deck.wait_for_string("dispatcher mode");

    // Submit via the [Submit] button — deterministic, no fragile Enter count.
    let (scol, srow) = deck
        .find_in_grid("[Submit]")
        .expect("the new-pane form should render a [Submit] button");
    deck.click(scol, srow);

    // Submitting closes the form and spawns the dispatcher CARD.
    deck.wait_for_absence("[Submit]");

    // The dispatcher must surface live as a DASHBOARD CARD, not a mode tab.
    //
    // This is the PRD #127 card shape (`mode_config: None` + `seed_prompt`), and
    // asserting it is the point: a mode tab routes through `render_mode_tab`'s
    // 50/50 split, so the dispatcher — which declares no side panes — rendered at
    // half width beside an empty column. `1 session(s)` with no tab strip is what
    // distinguishes the fixed shape from the broken one.
    // Asserted on the GRID, not the raw stream: this is redrawn dashboard chrome,
    // so the bytes carrying it are interleaved with cursor-positioning escapes and
    // the text never appears contiguously in the stream. The rendered grid is the
    // only surface where "what the user sees" is actually a substring.
    //
    // `common::wait_until` rather than `deck.wait_until_grid`, because the latter is
    // hard-capped at the harness `WAIT_TIMEOUT` (10s) — far too short for a real
    // claude cold boot plus SessionStart, the readiness buffer, and the seed
    // round-trip. Using it here silently cut an intended 60s wait to 10s and made
    // this test flaky by construction on a slower or busier host. The polling still
    // lives in `common` (Decision 21).
    const SURFACE_WAIT: Duration = Duration::from_secs(60);
    assert!(
        common::wait_until(SURFACE_WAIT, || deck
            .snapshot_grid()
            .contains("1 session(s)")),
        "the dispatcher never surfaced a LIVE dashboard card within {}s — expected a \
         single-agent card on the dashboard (NOT a mode tab, which would split the pane \
         50/50 with an empty side column).\n\
         Final grid:\n{}",
        SURFACE_WAIT.as_secs(),
        deck.snapshot_grid()
    );

    // The seed really reached the pane — the delivery that makes this a
    // *dispatcher* rather than a bare agent. The card's `Prmt:` line echoes the
    // seed's opening words, so an unseeded pane cannot pass this.
    assert!(
        common::wait_until(SURFACE_WAIT, || deck
            .snapshot_grid()
            .contains("You are an ordinary assistant")),
        "the dispatcher seed never appeared on the card within {}s — without it the agent \
         has not been taught the `dispatch` verb at all.\n\
         Final grid:\n{}",
        SURFACE_WAIT.as_secs(),
        deck.snapshot_grid()
    );

    // Give the seeded agent an actual goal. Without one it correctly stalls
    // asking for a task, which is why an earlier version of this test observed
    // no work at all. The instruction is deliberately directive and names the
    // unit, so the assertion below survives LLM phrasing and tool variance.
    //
    // The seed tells the agent to ask which shape the user wants — but this
    // fixture defines no `[[orchestrations]]`, so `--list-targets` offers only
    // `single` and the seed's own "nothing to ask" branch applies. The explicit
    // "do not ask me anything first" keeps that deterministic either way.
    deck.send_keys(
        b"Dispatch exactly one unit named probe-unit as a SINGLE AGENT (pass --single), \
          with the task \"list the files here\". Call the dispatch command now. \
          Do not ask me anything first.\r",
    );

    // The real-agent proof, end to end: the agent decomposed the goal, invoked
    // `dot-agent-deck dispatch`, the daemon created the git worktree, and it
    // landed at the SIBLING path the feature promises (`../<repo>-dispatch-<slug>`,
    // never nested inside the checkout). Asserting on the worktree rather than on
    // a status word avoids depending on claude's randomized spinner gerunds
    // ("Undulating…", "Thinking…"), which is what made the previous check vacuous.
    let expected_worktree = deck
        .workdir()
        .parent()
        .expect("fixture tempdir has a parent")
        .join(format!(
            "{}-dispatch-probe-unit",
            deck.workdir()
                .file_name()
                .expect("fixture dir has a name")
                .to_string_lossy()
        ));
    // Armed BEFORE the wait, so the worktree is reclaimed even if the assertion
    // below fails or the agent creates it late.
    let _worktree_guard = SiblingWorktreeGuard(expected_worktree.clone());

    // A real agent sometimes answers instead of acting — it acknowledges the
    // instruction, or asks a clarifying question, and then sits idle. Re-nudge on
    // a fixed cadence rather than waiting out one long silence, so a single
    // conversational detour doesn't fail the run. Bounded: NUDGES × NUDGE_EVERY
    // is the whole budget, and it stays inside this test's nextest kill window
    // (see `.config/nextest.toml`) so the assertion below — and its grid dump —
    // actually runs instead of the process being killed mid-wait.
    // The per-round wait is `common::wait_for_path`, the harness's bounded
    // path-appearance poll — Decision 21 keeps all sleeping/polling in `common`
    // rather than in a test body.
    const NUDGE_EVERY: Duration = Duration::from_secs(70);
    const NUDGES: u32 = 3;
    let mut dispatched = false;
    for round in 0..NUDGES {
        if round > 0 {
            deck.send_keys(
                b"You have not called the dispatch command yet. \
                  Run `dot-agent-deck dispatch probe-unit --task \"list the files here\" --single` \
                  now, with no further questions.\r",
            );
        }
        if common::wait_for_path(&expected_worktree, NUDGE_EVERY) {
            dispatched = true;
            break;
        }
    }

    assert!(
        dispatched,
        "the dispatcher agent never produced a dispatch worktree at {} after {} nudges \
         over {}s — expected it to call `dot-agent-deck dispatch probe-unit` and the \
         daemon to create the sibling worktree.\n\
         Final grid:\n{}",
        expected_worktree.display(),
        NUDGES,
        NUDGE_EVERY.as_secs() * NUDGES as u64,
        deck.snapshot_grid()
    );

    // The dispatched unit must be a real AGENT, not a shell.
    //
    // Asserting the worktree alone is what let a genuine bug ship: `dispatch`
    // passed `SpawnRequest.command: None`, which the spawn path reads as `$SHELL`,
    // so the unit came up as a bash prompt with the `--task` text typed into it.
    // The worktree appeared, a pane appeared, and the test was green.
    //
    // A second live session with an agent type on its card is what distinguishes
    // the two: the dispatcher card plus the dispatched unit's card, each labelled
    // with the agent that is actually running. A shell has no agent-type label.
    assert!(
        common::wait_until(SURFACE_WAIT, || {
            let g = deck.snapshot_grid();
            g.contains("2 session(s)") && g.matches("ClaudeCode").count() >= 2
        }),
        "the dispatched unit never came up as a real AGENT within {}s — a second live \
         session with an agent type on its card. `SpawnRequest.command: None` reads as \
         $SHELL in the spawn path, so this is what distinguishes an agent from a bash \
         prompt with the task typed into it.\n\
         Final grid:\n{}",
        SURFACE_WAIT.as_secs(),
        deck.snapshot_grid()
    );
}

/// Scenario: Launch the deck on the two-role `orch-deck` fixture, open one ordinary
/// `cat` pane so a registered pane exists to dispatch from, then run the REAL
/// `dot-agent-deck dispatch <name> --orchestration demo-orch` CLI against the deck's
/// own hook socket exactly as an agent in that pane would. A full orchestration tab
/// labelled `demo-orch` must surface live on the tab strip, with the sibling worktree
/// and the orchestrator's delegation context on disk.
#[spec("orchestration/dispatch/001")]
#[test]
fn orchestration_dispatch_001_tab_surfaces_with_role_cards() {
    const UNIT: &str = "team-probe";

    let deck = TuiDeck::builder()
        .with_env("PATH", path_with_binary_dir())
        .launch_with_fixture("orch-deck");
    deck.wait_for_string("No active sessions");

    // `git worktree add` needs a commit to branch from.
    commit_fixture_repo(deck.workdir());

    // One ordinary pane, so the daemon has a registered pane (with a cwd) to
    // resolve the dispatch's caller from.
    let caller_pane = open_cat_caller_pane(&deck);

    let expected_worktree = dispatch_worktree_of(&deck, UNIT);
    // Armed before the dispatch, so the sibling is reclaimed even on failure.
    let _guard = SiblingWorktreeGuard(expected_worktree.clone());

    // The REAL CLI, against the deck's own socket — the path an agent takes.
    // `--orchestration=demo-orch` names the fixture's orchestration explicitly:
    // this test is about the ORCHESTRATION shape, and the flag is what a
    // dispatcher agent that picked a shape off `--list-targets` actually sends.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args([
            "dispatch",
            UNIT,
            "--task",
            "Say hello, then stop.",
            "--orchestration=demo-orch",
        ])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", &caller_pane)
        .output()
        .expect("the dispatch CLI should run");
    assert!(
        out.status.success(),
        "`dispatch --orchestration demo-orch` failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // THE ASSERTION THAT WAS MISSING: the orchestration TAB surfaces live.
    //
    // Everything else this suite checks — the worktree on disk, the context file —
    // can be right while no tab ever appears, which is precisely the state a user
    // reported. The tab strip renders only with 2+ tabs (Dashboard + this one), so
    // the orchestration's name on the grid means the tab was built mid-session with
    // no reconnect.
    const TAB_WAIT: Duration = Duration::from_secs(90);

    // Structural proof first: BOTH roles spawned, carrying orchestration membership
    // named `demo-orch`. Unambiguous, and it cannot be satisfied by chrome.
    assert!(
        common::wait_until(TAB_WAIT, || {
            common::agent_records_on(deck.attach_socket_path())
                .iter()
                .filter(|r| {
                    matches!(
                        &r.tab_membership,
                        Some(dot_agent_deck::agent_pty::TabMembership::Orchestration { name, .. })
                            if name == "demo-orch"
                    )
                })
                .count()
                >= 2
        }),
        "the dispatch did not start BOTH roles of `demo-orch` within {}s — a \
         dispatched orchestration must spawn every role, not just one.\n\
         Records: {:?}\nFinal grid:\n{}",
        TAB_WAIT.as_secs(),
        common::agent_records_on(deck.attach_socket_path())
            .iter()
            .map(|r| (r.id.clone(), r.tab_membership.clone()))
            .collect::<Vec<_>>(),
        deck.snapshot_grid()
    );

    // Then the user-visible half: a TAB exists. The tab strip renders only with 2+
    // tabs, so the "Dashboard" label is present ONLY once a second tab was built —
    // it is absent on a single-tab deck. Deliberately NOT asserting the string
    // "demo-orch": the fixture is named that, and the new-pane form paints an
    // `[Orch: demo-orch]` chip, so that assertion passed before any dispatch ran at
    // all (caught by re-checking that this test can fail — the `reproduce-first` skill).
    assert!(
        common::wait_until(TAB_WAIT, || deck.snapshot_grid().contains("Dashboard")),
        "no tab strip appeared within {}s, so the dispatched orchestration never \
         became a TAB on the deck — which is the symptom a user reported.\n\
         Final grid:\n{}",
        TAB_WAIT.as_secs(),
        deck.snapshot_grid()
    );

    // And the unit is a real orchestration on disk, with its orchestrator told what
    // it is (PRD #222 parity) rather than handed the bare task.
    assert!(
        common::wait_for_path(&expected_worktree, Duration::from_secs(30)),
        "the dispatched worktree never appeared at {}",
        expected_worktree.display()
    );
    let context = expected_worktree.join(".dot-agent-deck/orchestrator-context.md");
    let content = std::fs::read_to_string(&context).unwrap_or_else(|e| {
        panic!(
            "the dispatched orchestration must get an orchestrator-context.md at {} \
             — without it the orchestrator never learns it is one, and every worker \
             sits idle: {e}",
            context.display()
        )
    });
    assert!(
        content.contains("Delegation protocol"),
        "the orchestrator must be told how to delegate:\n{content}"
    );
    assert!(
        content.contains("## Your task") && content.contains("Say hello"),
        "the caller's task must ride inside the context file:\n{content}"
    );
}

fn card_label(role: &str) -> String {
    format!("ClaudeCode · {role}")
}

/// Match a role label only within one same-weight card top-border span.
///
/// Matched on the CARD TITLE ROW, not anywhere on the grid: the tab's right half
/// renders the focused role's live terminal, so a bare `grid.contains` could be
/// satisfied by the agent's own output echoing the text back. Cropping to the
/// span between one weight's matching corners is what makes this specific to a
/// card title — the predicate itself lives in the harness so the fast tier can
/// guard it (`tests/grid_box_helpers.rs`; review of #465, S2/S5).
fn card_titled(grid: &str, role: &str) -> bool {
    common::label_in_box_top_border(grid, &card_label(role))
}

/// Scenario: Launch the deck on the `dispatch-orch-real` fixture, whose `real-team`
/// orchestration defines THREE roles that are all real interactive Claude Haiku
/// agents, open one `cat` caller pane, and run the real `dot-agent-deck dispatch
/// <name> --orchestration real-team` CLI against the deck's own hook socket. Every
/// role named in the toml must reach LIVE-AGENT state in the dispatched worktree —
/// the daemon holding an event-derived live session for each — not merely have a
/// pane spawned for it.
#[spec("orchestration/dispatch/002")]
#[test]
fn orchestration_dispatch_002_every_real_agent_role_comes_alive() {
    // Decision 26 runtime-skip: missing CLI / credentials is an environmental
    // condition, not a broken test.
    skip_unless!(common::check_claude_available());

    const UNIT: &str = "real-probe";
    const ORCH: &str = "real-team";
    /// Exactly the role names in `tests/fixtures/dispatch-orch-real/.dot-agent-deck.toml`.
    /// EVERY one of them has to come alive: "some roles started" is the shape of
    /// the reported failure, not a pass.
    const ROLES: [&str; 3] = ["orchestrator", "coder", "reviewer"];

    let deck = TuiDeck::builder()
        // Three live agent panes in one tab; a roomy deck keeps each role card's
        // PTY wide enough for a real claude TUI to render (and for the failure
        // diagnostics to be readable).
        .with_pty_size(200, 55)
        .with_imported_claude_credentials()
        // The branch build must win over any host-installed `dot-agent-deck`.
        .with_env("PATH", path_with_binary_dir())
        .launch_with_fixture("dispatch-orch-real");
    deck.wait_for_string("No active sessions");

    // `git worktree add` needs a commit to branch from — and the worktree is a
    // HEAD checkout, so this is also what puts `.dot-agent-deck.toml` (and its
    // three roles) inside the dispatched worktree at all.
    commit_fixture_repo(deck.workdir());

    // Trust BOTH the fixture dir and the dispatched WORKTREE for the interactive
    // `claude` panes, so no first-run onboarding / per-folder trust dialog can
    // swallow the spawn.
    //
    // The worktree matters and the fixture dir does not: the roles run in the
    // worktree, which does not exist yet and is not covered by trusting its
    // parent. Seeding it models the user whose agent config is already in order —
    // so that if a role still fails to come alive, the cause is the dispatch path
    // and not an unanswered dialog. Both the literal and canonical forms of the
    // fixture dir are seeded because the deck picks its cwd up from the process,
    // not from this path value; the worktree can only be named literally (it is
    // not on disk yet, so it cannot be canonicalized).
    let expected_worktree = dispatch_worktree_of(&deck, UNIT);
    let mut trust_paths = vec![
        deck.workdir().to_string_lossy().into_owned(),
        expected_worktree.to_string_lossy().into_owned(),
    ];
    if let Ok(canonical) = deck.workdir().canonicalize() {
        let canonical = canonical.to_string_lossy().into_owned();
        if !trust_paths.contains(&canonical) {
            trust_paths.push(canonical);
        }
    }
    common::seed_claude_trust_in_home(deck.home_dir(), &trust_paths)
        .expect("seed Claude onboarding and project trust");

    // The pane the dispatch is attributed to. `cat` is fine HERE — it is the
    // caller, not the thing under test.
    let caller_pane = open_cat_caller_pane(&deck);

    // Armed before the dispatch, so the sibling is reclaimed even on failure.
    let _guard = SiblingWorktreeGuard(expected_worktree.clone());

    // The REAL CLI, against the deck's own socket — the path an agent takes.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args([
            "dispatch",
            UNIT,
            "--task",
            "Reply with the single word READY and then stop. Do not delegate anything.",
            &format!("--orchestration={ORCH}"),
        ])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", &caller_pane)
        .output()
        .expect("the dispatch CLI should run");
    assert!(
        out.status.success(),
        "`dispatch --orchestration {ORCH}` failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // First the weak claim `orchestration/dispatch/001` already makes with `cat`
    // roles: a PANE exists for every role. Kept as a separate, earlier assertion
    // so a failure says which of the two halves broke — "no panes at all" is a
    // different bug from "panes but no agents".
    const PANE_WAIT: Duration = Duration::from_secs(60);
    assert!(
        common::wait_until(PANE_WAIT, || {
            let found = role_states(deck.attach_socket_path(), ORCH);
            ROLES.iter().all(|r| found.contains_key(*r))
        }),
        "the dispatch did not spawn a pane for every role of `{ORCH}` within {}s — \
         a dispatched orchestration must start EVERY role in its toml.{}\n\
         Final grid:\n{}",
        PANE_WAIT.as_secs(),
        role_diagnostics(&deck, ORCH, &ROLES),
        deck.snapshot_grid()
    );

    // Every role's AGENT really started — proven from the role's OWN PTY.
    //
    // Deliberately NOT `AgentRecord.live`: that is `Some(Idle)` for all three
    // roles within ~1.5s of the dispatch, before a single byte has been written
    // to any of those PTYs, so asserting on it is vacuous (measured while
    // building this test). The daemon seeds a session for a surfaced role pane;
    // it is a pane-level fact, not an agent-level one.
    //
    // The Claude Code banner is agent-level: a `cat` role, a `$SHELL`, or a
    // command that failed to launch can never print it. That is precisely the
    // class `orchestration/dispatch/001`'s `cat` roles cannot distinguish.
    //
    // The budget is generous because three real claude cold boots contend for the
    // same machine, and a slow boot must not read as a broken one. `common::wait_until`
    // rather than `deck.wait_until_grid`, which is hard-capped at the harness's 10s
    // `WAIT_TIMEOUT` — far too short here, and using it would silently shorten the
    // wait and make the test flaky by construction (Decision 21 keeps the polling
    // in `common`).
    const LIVE_WAIT: Duration = Duration::from_secs(240);
    let banner = common::search_key("Claude Code");
    assert!(
        common::wait_until(LIVE_WAIT, || {
            let found = role_states(deck.attach_socket_path(), ORCH);
            ROLES.iter().all(|role| {
                found.get(*role).is_some_and(|s| {
                    common::pane_search_key_on(deck.attach_socket_path(), &s.agent_id)
                        .contains(&banner)
                })
            })
        }),
        "not every role of `{ORCH}` started a REAL AGENT within {}s — each role pane must \
         show the agent its toml names, not an empty PTY or a shell. A pane per role is \
         NOT the promise: a dispatched orchestration whose panes exist but whose agents \
         never start looks identical on the dashboard until you try to use it.{}\n\
         Final grid:\n{}",
        LIVE_WAIT.as_secs(),
        role_diagnostics(&deck, ORCH, &ROLES),
        deck.snapshot_grid()
    );

    // ===== And now the half a user actually looks at: the CARDS ==============
    //
    // Everything above is daemon-side truth. The reported class of failure is a
    // deck that looks fine, so the test has to read what is on the screen: switch
    // to the dispatched orchestration's tab and require that EVERY role named in
    // the toml is on a card, beside the agent badge for the agent that is running
    // in it (`<AgentType> · <role>` — the card title shape).
    //
    // Ctrl+PageDown, not `l`/`h`: on an orchestration tab a role pane holds
    // keyboard focus and swallows plain keys as text.
    //
    // Without this assertion the test passes on a deck whose cards are labelled
    // with claude's session UUIDs (`ClaudeCode · 6134822e-f2`), which is what a
    // dispatched orchestration actually rendered when this test was written: the
    // daemon knew every role name and the user could not see any of them.
    deck.send_keys(b"\x1b[6;5~"); // Ctrl+PageDown → the dispatched orchestration tab
    const CARD_WAIT: Duration = Duration::from_secs(60);
    assert!(
        common::wait_until(CARD_WAIT, || {
            let grid = deck.snapshot_grid();
            ROLES.iter().all(|role| card_titled(&grid, role))
        }),
        "the dispatched orchestration's cards do not name the roles its toml defines. \
         Every role must appear as `{}` on its own card, so the user can tell which card \
         is which agent — the interactive `Ctrl+N` path already does this \
         (`tabs/orchestration/006`). Missing: {:?}{}\n\
         Final grid:\n{}",
        card_label("<role>"),
        ROLES
            .iter()
            .filter(|role| !card_titled(&deck.snapshot_grid(), role))
            .collect::<Vec<_>>(),
        role_diagnostics(&deck, ORCH, &ROLES),
        deck.snapshot_grid()
    );
}

// ---------------------------------------------------------------------------
// Closing a dispatched card (PRD #220 follow-up)
// ---------------------------------------------------------------------------

/// A `git` that is deliberately SLOW on `status` and ordinary for everything else.
///
/// Stands in for the one property of a real dispatched worktree this fixture
/// cannot cheaply have: `git status --porcelain` taking real time. In the repo
/// this feature is used in, the dispatched worktree is a full checkout that an
/// agent has been working in — often with a multi-GB build dir — and the status
/// walk is seconds, not milliseconds. Everything else about the close path is
/// genuine: the real binary, the real daemon, the real `git worktree remove`.
///
/// Narrowed to `status --porcelain` — EXACTLY the invocation `remove_worktree`'s
/// dirty check makes. Sleeping on every `status` also hit the deck's own git
/// calls during pane creation, which has its own 5s budget, so the pane never
/// came up and the test failed before reaching what it is about. Everything
/// else, including the dispatch's `git worktree add`, runs at full speed.
/// The real `git` path and the sleep are BAKED IN rather than read from the
/// environment: the harness scrubs the spawned deck's env to a pinned set, and a
/// stub whose `exec` target arrives empty in some descendant breaks every git
/// call instead of just the slow one.
const SLOW_GIT_STATUS_STUB: &str = r#"#!/bin/sh
saw_status=0
saw_porcelain=0
for a in "$@"; do
    [ "$a" = "status" ] && saw_status=1
    [ "$a" = "--porcelain" ] && saw_porcelain=1
done
if [ "$saw_status" = 1 ] && [ "$saw_porcelain" = 1 ]; then
    sleep __SLEEP__
fi
exec __REAL_GIT__ "$@"
"#;

/// Absolute path of the real `git`, resolved before any stub shadows it.
fn real_git_path() -> String {
    let out = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("resolve the real git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Install [`SLOW_GIT_STATUS_STUB`] as `git` in a fresh dir and return that dir.
fn install_slow_git(dir: &Path, sleep_secs: u32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bindir = dir.join("slow-git-bin");
    std::fs::create_dir_all(&bindir).expect("create the stub bin dir");
    let git = bindir.join("git");
    let script = SLOW_GIT_STATUS_STUB
        .replace("__SLEEP__", &sleep_secs.to_string())
        .replace("__REAL_GIT__", &real_git_path());
    std::fs::write(&git, script).expect("write the git stub");
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755))
        .expect("make the git stub executable");
    bindir
}

/// Press Ctrl+W on the currently-selected card and confirm Close.
///
/// `Down`/`j` and a single click all fail to move the dashboard selection in this
/// deck state (verified separately — `?` opens Help from the same keystroke
/// stream, so keys ARE being delivered), so this test never navigates: it closes
/// the default-selected card first and then the one that is left. A close aimed
/// at the wrong card would make the assertion meaningless.
fn confirm_close_selected(deck: &TuiDeck) {
    deck.send_keys(b"\x17"); // Ctrl+W → close confirmation
    deck.wait_for_string("Close selected pane?");
    deck.send_keys(b"\x1b[B"); // Down → [Close] (arrows DO work inside the modal)
    deck.send_keys(b"\r"); // confirm
}

/// Scenario: Dispatch a single REAL agent (so the daemon owns a worktree for it),
/// with a `git` whose `status` is slow — the state a real dispatched worktree is in
/// once an agent has worked there. Wait until the card shows the live agent, then
/// press Ctrl+W and confirm Close ONCE. The card must disappear on that first
/// confirm, rather than lingering (as "No agent" or otherwise) and needing a second.
#[spec("dispatch/close/001")]
#[test]
fn dispatch_close_001_first_confirm_removes_the_dispatched_card() {
    // Decision 26 runtime-skip: missing CLI / credentials is environmental.
    skip_unless!(common::check_claude_available());

    const UNIT: &str = "close-probe";
    /// The single-agent dispatch labels its card with the task name.
    const CARD: &str = "dispatch-close-probe";

    let scratch = common::race_safe_tempdir();
    // 8s: comfortably past the TUI's 5s `CTRL_W_STOP_TIMEOUT`, so the symptom is
    // deterministic rather than a race with a fast machine.
    let stub_bin = install_slow_git(scratch.path(), 8);
    // The dispatched unit is a REAL agent, because that is what a user dispatches.
    // This test previously used `cat` here, reasoning that the close path is the
    // same for every agent — it is NOT, and that stand-in is exactly what let the
    // reported bug survive a green run. A `cat` pane never emits a `SessionStart`,
    // so it has ONE session (the daemon's synthetic surface event) and the card is
    // whatever that says. A real agent emits its own, so the pane can carry a
    // second session — and a close that removes only one of them leaves a card
    // behind, which is the report ("the card stays, showing No agent").
    //
    // Pinned to Haiku, and never prompted: it only has to BE there when the close
    // lands, so the run costs a cold boot and no turns.
    //
    // And it is launched through a WRAPPER SCRIPT, not as a bare `claude`. This
    // mirrors the reported configuration, where every role runs `devbox run
    // agent-<role>`: the deck cannot infer an agent type from such a command, so
    // the card is badged `No agent` even though a real agent is running inside it.
    // That is the exact shape the report describes ("the status in the card says
    // no agent"), and a bare `claude` — which the deck DOES recognise — takes a
    // different path through the card/session machinery.
    let wrapper = stub_bin.join("agent-wrapper");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nexec claude --model claude-haiku-4-5-20251001 --allowedTools Bash \"$@\"\n",
    )
    .expect("write the agent wrapper");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
            .expect("make the agent wrapper executable");
    }
    let cfg = scratch.path().join("config.toml");
    std::fs::write(&cfg, "default_command = \"agent-wrapper\"\n").expect("write the deck config");

    let deck = TuiDeck::builder()
        // Roomy: at the default width a card title is ellipsized
        // (`dispatch-clo…`), so the selection check below could never match the
        // full name on the title row.
        .with_pty_size(200, 50)
        .with_env(
            "PATH",
            format!("{}:{}", stub_bin.display(), path_with_binary_dir()),
        )
        .with_env("DOT_AGENT_DECK_CONFIG", cfg.to_string_lossy())
        .with_imported_claude_credentials()
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");
    commit_fixture_repo(deck.workdir());

    let expected_worktree = dispatch_worktree_of(&deck, UNIT);
    // Trust the dispatched WORKTREE (where the agent runs) so claude's first-run
    // gates clear without a keystroke and it reaches a live state to be closed.
    common::seed_claude_trust_in_home(
        deck.home_dir(),
        &[expected_worktree.to_string_lossy().into_owned()],
    )
    .expect("seed Claude onboarding and project trust");

    let caller_pane = open_cat_caller_pane(&deck);
    let _guard = SiblingWorktreeGuard(expected_worktree.clone());

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["dispatch", UNIT, "--task", "Wait quietly.", "--single"])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", &caller_pane)
        .output()
        .expect("the dispatch CLI should run");
    assert!(
        out.status.success(),
        "`dispatch --single` failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Wait until the agent is REALLY RUNNING, not merely spawned. The report is
    // about closing an agent that is up; closing one mid-boot tests something else.
    //
    // Gated on the agent's OWN PTY output (the Claude Code banner), NOT on the
    // card's `ClaudeCode` badge: that badge is inferred from the COMMAND at spawn
    // time, so it is on the card before claude has executed a single instruction.
    // Gating on it let this test close a still-booting pane and pass.
    const SURFACE_WAIT: Duration = Duration::from_secs(120);
    let banner = common::search_key("Claude Code");
    let dispatched_agent_id = || {
        common::agent_records_on(deck.attach_socket_path())
            .into_iter()
            .find(|r| r.display_name.as_deref() == Some(CARD))
            .map(|r| r.id)
    };
    assert!(
        common::wait_until(SURFACE_WAIT, || {
            dispatched_agent_id().is_some_and(|id| {
                common::pane_search_key_on(deck.attach_socket_path(), &id).contains(&banner)
            })
        }),
        "the dispatched agent never actually started within {}s — its pane never \
         printed the Claude Code banner, so a close here would not be closing a \
         running agent.\nGrid:\n{}",
        SURFACE_WAIT.as_secs(),
        deck.snapshot_grid()
    );
    assert!(
        common::wait_until(Duration::from_secs(30), || deck
            .snapshot_grid()
            .contains(CARD)),
        "the dispatched agent is running but never surfaced a card.\nGrid:\n{}",
        deck.snapshot_grid()
    );

    // Command mode. The CALLER card is the selected one, so close it first — it
    // owns no worktree, so this close is the control: it must succeed on the
    // first confirm, and it leaves the dispatched card as the only one.
    deck.send_keys(b"\x04"); // Ctrl+D → command mode
    deck.wait_for_string("COMMAND");
    confirm_close_selected(&deck);
    assert!(
        common::wait_until(Duration::from_secs(30), || {
            let g = deck.snapshot_grid();
            !g.contains("caller") && g.contains(CARD)
        }),
        "the CALLER card (no worktree, nothing to clean up) did not close on the first \
         confirm — so this test cannot attribute a later failure to the worktree \
         cleanup.\nGrid:\n{}",
        deck.snapshot_grid()
    );

    // Now the dispatched card is the only one. Close it — ONCE.
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            let g = deck.snapshot_grid();
            g.lines().any(|l| l.contains('▸') && l.contains(CARD))
        }),
        "the dispatched card is not the selected one after the caller closed, so \
         Ctrl+W would not target it.\nGrid:\n{}",
        deck.snapshot_grid()
    );
    confirm_close_selected(&deck);

    // The assertion. Generous, so a merely slow close still passes — what this
    // pins is a card that is never removed at all.
    // NO card may remain for the closed pane — not the one that was closed, and not
    // a ghost left over from a second session on the same pane. Matched on the
    // dispatched worktree's basename, which every such card carries on its `Dir:`
    // line whatever its title says: the ghost is titled `pane-sched-…`, so a needle
    // bound to the card's NAME would miss it entirely.
    let dir_marker = expected_worktree
        .file_name()
        .expect("worktree has a name")
        .to_string_lossy()
        .into_owned();
    const CLOSE_WAIT: Duration = Duration::from_secs(30);
    assert!(
        common::wait_until(CLOSE_WAIT, || !deck.snapshot_grid().contains(&dir_marker)),
        "the dispatched card {CARD:?} was still on the deck {}s after ONE confirmed \
         close, while the SAME close removed the caller card immediately.\n\
         Two independent causes produce exactly this, and the daemon records below \
         tell them apart:\n\
         (a) records NON-EMPTY — the agent was never stopped. A daemon-spawned card \
         has no local pane in this TUI until it is focused, so `close_pane` answered \
         `Pane <id> not found` and the F4 policy preserved the card. Focusing it \
         attaches it, which is why a second Ctrl+W appears to work.\n\
         (b) records EMPTY — the agent IS stopped and only the card survived. The \
         daemon held the close response until it finished removing the worktree, past \
         the TUI's 5s stop-agent budget, so the client timed out and retained the pane.\n\
         Daemon records after the close: {:?}\n\
         Grid:\n{}",
        CLOSE_WAIT.as_secs(),
        common::agent_records_on(deck.attach_socket_path())
            .iter()
            .map(|r| (r.id.clone(), r.display_name.clone()))
            .collect::<Vec<_>>(),
        deck.snapshot_grid()
    );
}
