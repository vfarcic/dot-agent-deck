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
use dot_agent_deck::event::SendResult;
use dot_agent_deck::state::{DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS, SessionStatus};
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
    // Issue #663: an empty registry is AMBIGUOUS, and the ambiguity misdirected a
    // whole investigation. `role_states` reads `ListAgents` over the attach
    // socket, so a daemon that is gone answers exactly like a daemon that spawned
    // nothing — and every role then prints `NO PANE — never spawned at all`
    // beside a rendered grid showing all three panes alive. Say which it is
    // before enumerating roles, so nobody reads a dead daemon as a spawn failure.
    if common::agent_records_on(socket).is_empty() {
        out.push_str(
            "\n- NOTE: the daemon returned NO agent records at all. Every `NO PANE` line \
             below may mean the daemon is unreachable (e.g. its \
             DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS backstop fired) rather than that \
             anything failed to spawn — check the grid and the deck log before \
             concluding the dispatch never started a pane.\n",
        );
    }
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

/// The single-line pointer `handle_delegate` writes into the TARGET worker's PTY
/// (`state::resolve_delegate_task_body`). Role-qualified, and — the load-bearing
/// property — DAEMON-authored: these bytes exist only if the daemon resolved the
/// sender as an orchestrator AND the target as one of its workers, so their
/// arrival in the worker's scrollback *is* the routing decision, observed at the
/// only place the worker could ever act on it.
///
/// This is what makes a `cat` role enough here. `cat` cannot *initiate* a
/// delegate — which is why `orchestration/dispatch/001` disclaimed the round
/// trip — but the initiating half is a plain CLI invocation the test can make
/// itself (see [`run_delegate`]), and `cat` echoes whatever is written to it. The
/// receiving half is therefore fully observable without spending a token.
fn worker_pointer(role: &str) -> String {
    format!("Read .dot-agent-deck/worker-task-{role}.md for your task.")
}

/// Every live role pane of orchestration `orch` whose cwd's basename satisfies
/// `dir_is`, as `role name → DOT_AGENT_DECK_PANE_ID`.
///
/// Scoping by CWD is what keeps the DISPATCHED orchestration and the
/// normally-started CONTROL apart. Both are `demo-orch` with byte-identical role
/// names, and `role_states` (which keys on role name alone) would silently
/// collapse them into one entry — so the two tabs differ only in where their
/// roles run: the dispatched one in the sibling worktree, the control in the
/// fixture dir itself. Matching on the basename rather than the full path avoids
/// depending on whether the daemon recorded a canonicalized cwd.
fn role_panes_in(
    socket: &Path,
    orch: &str,
    dir_is: impl Fn(&str) -> bool,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for record in common::agent_records_on(socket) {
        let Some(TabMembership::Orchestration {
            name, role_name, ..
        }) = record.tab_membership.clone()
        else {
            continue;
        };
        let (Some(cwd), Some(pane_id)) = (record.cwd.clone(), record.pane_id_env.clone()) else {
            continue;
        };
        let basename = Path::new(&cwd)
            .file_name()
            .map(|b| b.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name == orch && dir_is(&basename) {
            out.insert(role_name, pane_id);
        }
    }
    out
}

/// Poll until orchestration `orch` has a pane for every one of `roles` under a
/// cwd satisfying `dir_is`, and return them. Panics with the full record set on
/// timeout, so a failure names which role never showed up.
fn wait_for_role_panes(
    deck: &TuiDeck,
    orch: &str,
    label: &str,
    roles: &[&str],
    dir_is: impl Fn(&str) -> bool + Copy,
    timeout: Duration,
) -> BTreeMap<String, String> {
    let socket = deck.attach_socket_path();
    common::wait_until(timeout, || {
        let found = role_panes_in(socket, orch, dir_is);
        roles.iter().all(|r| found.contains_key(*r))
    });
    let found = role_panes_in(socket, orch, dir_is);
    assert!(
        roles.iter().all(|r| found.contains_key(*r)),
        "the {label} `{orch}` never had a pane for every role {roles:?} within {}s — got {found:?}.\n\
         Records: {:?}\nFinal grid:\n{}",
        timeout.as_secs(),
        common::agent_records_on(socket)
            .iter()
            .map(|r| (
                r.pane_id_env.clone(),
                r.cwd.clone(),
                r.tab_membership.clone()
            ))
            .collect::<Vec<_>>(),
        deck.snapshot_grid()
    );
    found
}

/// Run the REAL `dot-agent-deck delegate` CLI as the agent inside `pane_id`
/// would: same binary, same hook socket, same `DOT_AGENT_DECK_PANE_ID` env the
/// deck exports into every pane it spawns.
///
/// Invoking the CLI directly rather than prompting an LLM to invoke it is
/// deliberate and faithful — this IS the command the orchestrator's Bash tool
/// runs, and it removes model variance from a test about daemon-side routing.
/// (`orchestration/dispatch/002` covers the same path with a real orchestrator
/// deciding to run it.)
fn run_delegate(deck: &TuiDeck, pane_id: &str, role: &str, task: &str) -> std::process::Output {
    run_delegate_to(deck, pane_id, &[role], task)
}

/// [`run_delegate`] with the repeatable `--to` in its general form, for the
/// fan-out cases — in particular the partially-resolvable one, where some roles
/// have a worker pane and some do not.
fn run_delegate_to(
    deck: &TuiDeck,
    pane_id: &str,
    roles: &[&str],
    task: &str,
) -> std::process::Output {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
    cmd.arg("delegate");
    for role in roles {
        cmd.args(["--to", role]);
    }
    cmd.args(["--task", task])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", pane_id)
        .output()
        .expect("the delegate CLI should run")
}

/// Wait for the daemon's delegate pointer for `role` to appear in that role's
/// pane, re-resolving the pane's registry agent id(s) on every poll.
///
/// Two properties here are load-bearing rather than defensive, and both were
/// found by running this helper against the WORKING control path, where it
/// failed once and then passed:
///
///  1. **Re-resolve, don't cache.** `clear` defaults to `true`, so a delegate
///     RESPAWNS the worker: the agent id that existed when the delegate was sent
///     is dead by the time the pointer is written ~31s later (the 30s
///     `SessionStart` wait a `cat` role can never satisfy, plus the readiness
///     buffer). A cached id reads an empty snapshot forever.
///  2. **Check EVERY record carrying the pane id, not the first.** Across a
///     respawn the registry briefly holds both the old and the new agent for one
///     pane, and `ListAgents` order is not specified — so `.find()` can hand back
///     the dead one, whose scrollback will never contain the pointer. That is a
///     coin flip between a pass and a spurious failure, which is exactly what it
///     did on the control.
fn wait_for_delegate_pointer(
    deck: &TuiDeck,
    orch: &str,
    dir_is: impl Fn(&str) -> bool + Copy,
    role: &str,
    timeout: Duration,
) -> bool {
    let socket = deck.attach_socket_path();
    let needle = common::search_key(&worker_pointer(role));
    common::wait_until(timeout, || {
        let Some(pane_id) = role_panes_in(socket, orch, dir_is).get(role).cloned() else {
            return false;
        };
        common::agent_records_on(socket)
            .into_iter()
            .filter(|r| r.pane_id_env.as_deref() == Some(pane_id.as_str()))
            .any(|r| common::pane_search_key_on(socket, &r.id).contains(&needle))
    })
}

/// Drive the production `Ctrl+N` new-pane flow to open the fixture's single
/// orchestration against the deck's CURRENT directory — the "normal" way a user
/// starts one, and the CONTROL for the dispatched path.
///
/// With no `[[modes]]` in the `orch-deck` fixture the Mode chip row is
/// `[No mode] [Orch: demo-orch] [schedule]`, so ONE Right selects the
/// orchestration; selecting one HIDES the Command field, so the second Enter
/// submits. Mirrors `e2e_orchestration_route_isolation::open_orchestration_tab`.
fn open_orchestration_tab(deck: &TuiDeck, orch: &str) {
    deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
    deck.send_keys(b" "); // Space → confirm the current dir → new-pane form
    deck.wait_for_string("No mode"); // form up, Mode field focused
    deck.send_keys(b"\x1b[C"); // Right → [Orch: <orch>]
    deck.wait_for_string(orch);
    deck.send_keys(b"\r"); // Mode → Name
    deck.send_keys(b"\r"); // submit (Command is hidden for an orchestration)
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
/// and the orchestrator's delegation context on disk. Then open the SAME orchestration
/// the normal way with Ctrl+N as a control, and run the real `dot-agent-deck delegate`
/// CLI from each orchestrator: both workers must receive the daemon's task pointer in
/// their panes. Finally, delegate twice more in ways that cannot resolve — from a pane
/// with no role, and to a role that does not exist — and require the CLI to exit
/// non-zero naming what it could not resolve.
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

    // ===== Does the dispatched orchestrator's `delegate` REACH its worker? ====
    //
    // Everything above this line was GREEN while a user's dispatched orchestration
    // could not delegate at all: the tab was there, the worktree was there, the
    // orchestrator had its context file telling it how to delegate — and every
    // `dot-agent-deck delegate --to worker` it ran exited 0 having done nothing,
    // because the daemon had no idea that pane was an orchestrator
    // (`delegate from unknown pane`, `~/.local/state/dot-agent-deck/deck.log`).
    //
    // The dispatched roles are told apart from the control's identically-named
    // ones by the directory they run in.
    let worktree_dir = expected_worktree
        .file_name()
        .expect("the dispatch worktree has a basename")
        .to_string_lossy()
        .into_owned();
    let is_worktree = |basename: &str| basename == worktree_dir;
    let is_fixture = |basename: &str| basename != worktree_dir;

    const ROLES: [&str; 2] = ["orchestrator", "worker"];
    const DELEGATE_WAIT: Duration = Duration::from_secs(90);

    // --- CONTROL first: the SAME orchestration, started the NORMAL way -------
    //
    // Run before the dispatched case on purpose. It is the same fixture, the same
    // two roles, the same delegate CLI and the same daemon — the ONLY thing that
    // differs is which code path spawned the panes. So if the control passes and
    // the dispatched case fails, the failure is attributable to the dispatch spawn
    // path specifically and not to delegation being broken generally; and if the
    // control ITSELF fails, the harness is wrong and the dispatched result below
    // proves nothing (the `reproduce-first` skill's "add a control" step).
    open_orchestration_tab(&deck, "demo-orch");
    let control = wait_for_role_panes(
        &deck,
        "demo-orch",
        "normally-started (Ctrl+N) orchestration",
        &ROLES,
        is_fixture,
        DELEGATE_WAIT,
    );

    let out = run_delegate(
        &deck,
        &control["orchestrator"],
        "worker",
        "Control delegation.",
    );
    assert!(
        out.status.success(),
        "CONTROL FAILED: `delegate` from a normally-started orchestration's \
         orchestrator exited {:?}. The control must pass for the dispatched result \
         below to mean anything.\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_for_delegate_pointer(&deck, "demo-orch", is_fixture, "worker", DELEGATE_WAIT),
        "CONTROL FAILED: the worker of a NORMALLY-STARTED `demo-orch` never received \
         the delegate pointer {:?} within {}s. This path is known to work (it is the \
         one a user falls back to), so a failure here means the harness is wrong — \
         fix it before reading anything into the dispatched case.\nFinal grid:\n{}",
        worker_pointer("worker"),
        DELEGATE_WAIT.as_secs(),
        deck.snapshot_grid()
    );

    // --- The reported case: the DISPATCHED orchestration --------------------
    let dispatched = wait_for_role_panes(
        &deck,
        "demo-orch",
        "dispatched orchestration",
        &ROLES,
        is_worktree,
        DELEGATE_WAIT,
    );

    let out = run_delegate(
        &deck,
        &dispatched["orchestrator"],
        "worker",
        "Dispatched delegation.",
    );
    assert!(
        out.status.success(),
        "`delegate` from a DISPATCHED orchestration's orchestrator exited {:?}.\n\
         stdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_for_delegate_pointer(&deck, "demo-orch", is_worktree, "worker", DELEGATE_WAIT),
        "THE REPORTED BUG: the worker of a DISPATCHED `demo-orch` never received the \
         delegate pointer {:?} within {}s, while the identical delegation in the \
         control (same fixture, same roles, same CLI, started with Ctrl+N) DID \
         arrive. An orchestration started by `dispatch --orchestration` cannot \
         delegate to its own workers: the daemon's role maps are populated only by \
         the `StartAgent` handler, so panes spawned daemon-side are unknown to \
         `handle_delegate` and it drops the signal.\nDispatched panes: {dispatched:?}\n\
         Final grid:\n{}",
        worker_pointer("worker"),
        DELEGATE_WAIT.as_secs(),
        deck.snapshot_grid()
    );

    // ===== …and when it CANNOT resolve, it must say so ======================
    //
    // The second half of the report: `delegate` was fire-and-forget, so every
    // failure above was invisible to the caller. An orchestrator that cannot tell
    // success from failure announces the worker is working and then waits forever
    // for a `work-done` that can never arrive — the silent failure is what turned
    // a routing bug into a hung orchestration.
    //
    // Two ways to be unresolvable, both exercised from the REAL CLI:
    //   1. a sender the daemon holds no role for (the `cat` caller pane) — the
    //      literal shape of the reported log line, `delegate from unknown pane`;
    //   2. a resolvable orchestrator naming a role its orchestration does not have.
    let unknown = run_delegate(&deck, &caller_pane, "worker", "From a non-orchestrator.");
    let unknown_err = String::from_utf8_lossy(&unknown.stderr).into_owned();
    assert!(
        !unknown.status.success(),
        "`delegate` from a pane the daemon holds no role for exited 0. It must FAIL \
         LOUDLY: the orchestrator has no other way to learn its delegation went \
         nowhere, and a silent success is what makes it report phantom progress and \
         then wait forever.\nstdout: {}\nstderr: {unknown_err}",
        String::from_utf8_lossy(&unknown.stdout)
    );
    assert!(
        unknown_err.contains(&caller_pane),
        "a failed `delegate` must name the pane id it could not resolve ({caller_pane}) \
         on stderr, so the agent reading it can say what broke.\nstderr: {unknown_err}"
    );

    let bad_role = run_delegate(
        &deck,
        &dispatched["orchestrator"],
        "nonexistent-role",
        "To nobody.",
    );
    let bad_role_err = String::from_utf8_lossy(&bad_role.stderr).into_owned();
    assert!(
        !bad_role.status.success(),
        "`delegate --to nonexistent-role` from a VALID orchestrator exited 0. A role \
         with no pane is a delegation that reached nobody and must be reported as \
         one.\nstdout: {}\nstderr: {bad_role_err}",
        String::from_utf8_lossy(&bad_role.stdout)
    );
    assert!(
        bad_role_err.contains("nonexistent-role"),
        "a failed `delegate` must name the ROLE it could not resolve on stderr.\n\
         stderr: {bad_role_err}"
    );

    // ===== …and a HALF-landed delegate is not a failure =====================
    //
    // PR #466 review's blocker, from the real CLI. `--to worker
    // --to nonexistent-role` fans out to the worker for real — the task is in
    // its PTY and its idle-worker record is armed — so reporting failure would
    // invite the orchestrator to retry under this command's own contract
    // ("non-zero ⇒ it did not land") and dispatch the worker a second time,
    // arming two records for one pane. Exit 0, and name BOTH sides so a retry
    // can be aimed at just the role that missed.
    let partial = run_delegate_to(
        &deck,
        &dispatched["orchestrator"],
        &["worker", "nonexistent-role"],
        "Half-landed delegation.",
    );
    let partial_err = String::from_utf8_lossy(&partial.stderr).into_owned();
    assert!(
        partial.status.success(),
        "`delegate --to worker --to nonexistent-role` exited {:?}. The worker DID \
         receive it, so a non-zero exit tells the orchestrator to retry a \
         delegation that half landed — and the worker gets the task twice.\n\
         stdout: {}\nstderr: {partial_err}",
        partial.status.code(),
        String::from_utf8_lossy(&partial.stdout)
    );
    assert!(
        partial_err.contains("nonexistent-role") && partial_err.contains("worker"),
        "a half-landed `delegate` must name BOTH the role that missed and the \
         role that received it, or a retry cannot be aimed safely.\n\
         stderr: {partial_err}"
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
/// pane spawned for it, and every role must be named on its own card. Then ask the
/// real orchestrator to delegate a sentinel-file task to its `coder`: the coder
/// agent must receive it and actually create the file in the dispatched worktree.
/// The `coder` is `clear = true`, so that delegation respawns it; once the sentinel
/// appears, every role must STILL be named on its own card, rather than the
/// replacement agent's session UUID.
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
        // Issue #663: run the PRODUCTION post-respawn readiness buffer. The
        // harness pins it to `0` for the many e2e scenarios whose workers are
        // stand-ins that accept input the instant they exist; PRD #249's own
        // measurement is that a real agent's task pointer is LOST at `0` and
        // delivered-and-submitted at `1000`, because `SessionStart` means "a
        // session exists", not "the TUI interprets `\r` as submit".
        //
        // This became a real-agent respawn scenario when the fixture's `coder`
        // flipped to `clear = true` (#584/#606, PR #646), and it inherited the
        // pin — so the delegate reached the right worker, respawned it, and
        // wrote the pointer into a Claude that had emitted `SessionStart` ~400ms
        // earlier and was still booting. The bytes were dropped, the coder sat
        // idle for the full 300s budget, and the test was deterministically red.
        // The other two real `clear = true` scenarios
        // (`orchestration/delegate/014`, `/015`) already opt back in the same way.
        .with_env(DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS, "1000")
        // The daemon must outlive this test's OWN budget, or its failure
        // diagnostics lie. The harness's leaked-daemon backstop is 300s and
        // `WORK_WAIT` below is also 300s, so on a red run the daemon self-exited
        // ~25s BEFORE the assertion fired: `role_diagnostics` then queried a
        // dead socket, got no records, and reported `NO PANE — never spawned at
        // all` for all three roles while the same dump rendered them alive. That
        // false diagnostic is what issue #663 was filed on. 900s still bounds a
        // leak (the test itself runs ~320s).
        .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "900")
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
    let events = deck.subscribe_events();

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

    // ===== …and the orchestration can actually ORCHESTRATE ==================
    //
    // Everything above passed while a dispatched orchestration was completely
    // inert: three real agents up, every card correctly named, and an orchestrator
    // holding a delegation protocol it could not use, because the daemon had no
    // role map for panes it spawned itself. `orchestration/dispatch/001` pins the
    // ROUTING half cheaply (`cat` roles, daemon-authored pointer, plus the
    // normally-started control that makes the failure attributable to the dispatch
    // path). This is the half only real agents can show: a real orchestrator
    // DECIDING to shell `dot-agent-deck delegate`, and a real worker receiving the
    // task and doing the work — the user's actual altitude, "I dispatched an
    // orchestration and the team got something done".
    //
    // The observable is a uniquely-named sentinel FILE in the dispatched worktree
    // (rule 4): it survives LLM phrasing and tool variance, and — unlike anything
    // on the grid — it cannot be produced by an agent merely echoing the task back.
    const SENTINEL: &str = "dispatch_delegate_a41f.txt";
    let worker_task = format!(
        "Create a file named {SENTINEL} in the current directory containing the single \
         word DONE. Do not ask what to do, offer numbered choices, or wait for a task to \
         be defined - this message IS the task and you have everything you need. If a \
         file you were pointed at looks empty or missing, create {SENTINEL} anyway \
         instead of asking about it."
    );
    let role_panes = role_panes_in(deck.attach_socket_path(), ORCH, |_| true);
    let coder_pane = role_panes
        .get("coder")
        .cloned()
        .expect("the dispatched orchestration has a `coder` role pane (asserted above)");
    let orchestrator_pane = role_panes
        .get("orchestrator")
        .cloned()
        .expect("the dispatched orchestration has an `orchestrator` role pane");

    // Wait for every role pane to STOP painting before injecting anything.
    //
    // Not defensive — load-bearing, and learned the expensive way. Without it this
    // test passed in ~17s run alone and failed at its full 300s budget inside a
    // full `cargo test-e2e`: on a saturated machine the boot is slower, the
    // directive lands during a claude TUI's mid-init lull, and bytes injected in
    // that lull are simply dropped. The orchestrator then never delegates, so
    // nothing distinguishes it from the product bug this test exists to catch.
    // `wait_until_panes_settled`'s `min_alive` floor is the part that matters:
    // "quiet" alone is also true of a pane that has not started painting yet.
    // Mirrors `orchestration/route/001`, which injects into real agents the same
    // way. Non-fatal on timeout (a busy agent may still accept input) but logged,
    // so a later failure is diagnosable.
    let settle_ids: Vec<String> = {
        let states = role_states(deck.attach_socket_path(), ORCH);
        ROLES
            .iter()
            .filter_map(|r| states.get(*r).map(|s| s.agent_id.clone()))
            .collect()
    };
    if !common::wait_until_panes_settled(
        deck.attach_socket_path(),
        &settle_ids,
        Duration::from_millis(1500),
        Duration::from_secs(8),
        Duration::from_secs(180),
    ) {
        eprintln!("warning: not every role pane settled within 180s; proceeding anyway");
    }

    // Delivered through the daemon's production prompt-delivery RPC — the same
    // guarded write-and-submit the deck uses for a seed or orchestrator prompt.
    // What the orchestrator does with it (shell `dot-agent-deck delegate`) is the
    // AGENT's own doing, which is the point: this is the decision path, not a
    // test-issued CLI call.
    let directive = format!(
        "Use the Bash tool to run exactly this one command, then stop and say nothing \
         else: dot-agent-deck delegate --to coder --task \"{worker_task}\""
    );
    let orchestrator_agent_id = role_states(deck.attach_socket_path(), ORCH)
        .remove("orchestrator")
        .expect("the dispatched orchestration has an `orchestrator` role state")
        .agent_id;
    let orchestrator_session_id = events.wait_for_session_start_on_pane(
        &orchestrator_pane,
        &orchestrator_agent_id,
        Duration::from_secs(10),
    );
    let resp = common::write_and_submit_with_identity_on(
        deck.attach_socket_path(),
        &orchestrator_pane,
        &directive,
        &orchestrator_agent_id,
        Some(&orchestrator_session_id),
    )
    .expect("WriteAndSubmit to the dispatched orchestrator pane over the attach socket");
    assert_eq!(
        resp.send_result,
        Some(SendResult::Applied),
        "the daemon refused to deliver the delegate directive to the dispatched \
         orchestrator pane {orchestrator_pane}: error={:?}, send_result={:?}",
        resp.error,
        resp.send_result
    );

    // Generous: an orchestrator turn, a `clear = true` worker RESPAWN (issue
    // #584 — a fresh Haiku cold boot, its `SessionStart`, and the readiness
    // buffer) and then a worker turn — three real Haiku round trips plus a cold
    // start, on a machine already running three agents. A slow chain must not
    // read as a broken one.
    const WORK_WAIT: Duration = Duration::from_secs(300);
    let sentinel_path = expected_worktree.join(SENTINEL);
    assert!(
        common::wait_for_path(&sentinel_path, WORK_WAIT),
        "the dispatched orchestration's `coder` never did the delegated work within {}s — \
         no {SENTINEL} at {}. A dispatched orchestration whose orchestrator cannot reach \
         its own workers looks completely healthy (panes up, cards named, tab live) and \
         gets nothing done: the orchestrator reports the worker is working and then waits \
         forever for a work-done that cannot arrive.{}\n\
         Orchestrator pane {orchestrator_pane}, coder pane {coder_pane}.\n\
         Final grid:\n{}",
        WORK_WAIT.as_secs(),
        sentinel_path.display(),
        role_diagnostics(&deck, ORCH, &ROLES),
        deck.snapshot_grid()
    );

    // ===== …and the cards are STILL named after the work happened ===========
    //
    // Issue #663. The card assertion above runs before the delegation; this one
    // runs after it, and only this one can see what a `clear = true` delegate
    // does to the card. The respawn SIGTERMs the worker, its `SessionEnd`
    // retires the named session, and the replacement's `SessionStart` is a fresh
    // generation — so the `coder` card reverted to the replacement's session
    // UUID (`ClaudeCode · c70493f1-13…`) the moment the orchestration did its
    // first piece of work. Every other orchestration path masks this with the
    // TUI-side `ui.pane_display_names` mirror; the live orchestration surface a
    // dispatch builds does not seed it, so the dispatched deck is where the user
    // sees it. Asserted here rather than only in the failure dump, because a
    // dispatched team whose cards stop saying which agent is which is precisely
    // the "looks healthy, tells you nothing" shape this test exists to catch.
    assert!(
        common::wait_until(CARD_WAIT, || {
            let grid = deck.snapshot_grid();
            ROLES.iter().all(|role| card_titled(&grid, role))
        }),
        "the dispatched orchestration's cards lost their role names after the \
         delegation. Every role must STILL appear as `{}` once its agent has been \
         respawned by a `clear = true` delegate — a card that reverts to the \
         replacement's session UUID leaves the user unable to tell the orchestrator \
         from a worker on a team that is actively working. Missing: {:?}{}\n\
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

/// Scenario: Dispatch a unit so the daemon owns a `KeepIfDirty` worktree for it,
/// leave an uncommitted file in that worktree, then close the dispatched card
/// through the real Ctrl+W → confirm path. The confirmation dialog must say, BEFORE
/// the destructive keystroke, that the uncommitted work is kept and where; the
/// control close of a caller pane that owns no worktree must say nothing of the
/// sort; and after confirming, the status line must repeat the path and the
/// directory must still be on disk.
#[spec("dispatch/close/002")]
#[test]
fn dispatch_close_002_a_kept_dirty_worktree_is_announced_before_and_after_the_close() {
    const UNIT: &str = "keep-probe";
    /// The single-agent dispatch labels its card with the task name.
    const CARD: &str = "dispatch-keep-probe";
    /// Uniquely named so the assertion cannot pass on some other stray file.
    const WORK: &str = "uncommitted-work-717.txt";

    let scratch = common::race_safe_tempdir();
    // `cat` throughout. This test is about what the DECK says at close time, and
    // the sentence it must say is decided by the daemon's worktree registry and a
    // `git status` — neither of which any agent participates in. A real agent
    // would add a cold boot and API cost while proving nothing extra here;
    // `dispatch/close/001` next door owns the real-agent close path.
    let cfg = scratch.path().join("config.toml");
    std::fs::write(&cfg, "default_command = \"cat\"\n").expect("write the deck config");

    let deck = TuiDeck::builder()
        // Roomy, so neither the card title nor the dialog's path line is
        // ellipsized by the terminal — the path IS the assertion.
        .with_pty_size(200, 50)
        .with_env("PATH", path_with_binary_dir())
        .with_env("DOT_AGENT_DECK_CONFIG", cfg.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");
    commit_fixture_repo(deck.workdir());

    let expected_worktree = dispatch_worktree_of(&deck, UNIT);
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

    const SURFACE_WAIT: Duration = Duration::from_secs(60);
    assert!(
        common::wait_until(SURFACE_WAIT, || expected_worktree.is_dir()
            && deck.snapshot_grid().contains(CARD)),
        "the dispatch never produced a worktree at {} with a surfaced card.\nGrid:\n{}",
        expected_worktree.display(),
        deck.snapshot_grid()
    );

    // The premise: the user has uncommitted work in the dispatched worktree.
    // This is what `RemovalPolicy::KeepIfDirty` protects, and what closing the
    // tab used to discard from view without discarding from disk.
    std::fs::write(
        expected_worktree.join(WORK),
        "work the user has not committed yet",
    )
    .expect("dirty the dispatched worktree");

    deck.send_keys(b"\x04"); // Ctrl+D → command mode
    deck.wait_for_string("COMMAND");

    // ===== control: the CALLER card owns no worktree ========================
    //
    // Closing it must read exactly as it always has. Without this, a dialog that
    // warned on every close would pass every assertion below while being useless.
    deck.send_keys(b"\x17");
    deck.wait_for_string("Close selected pane?");
    let control = deck.snapshot_grid();
    assert!(
        !control.contains("Uncommitted work"),
        "a pane the deck removes no directory for must not warn about keeping one\n{control}"
    );
    deck.send_keys(b"\x1b[B");
    deck.send_keys(b"\r");
    assert!(
        common::wait_until(Duration::from_secs(30), || {
            let g = deck.snapshot_grid();
            !g.contains("caller") && g.contains(CARD)
        }),
        "the caller card did not close, so the dispatched card is not the armed \
         target below.\nGrid:\n{}",
        deck.snapshot_grid()
    );

    // ===== the dispatched card: warn BEFORE the keystroke ===================
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            let g = deck.snapshot_grid();
            g.lines()
                .any(|l| l.contains('\u{25b8}') && l.contains(CARD))
        }),
        "the dispatched card is not the selected one, so Ctrl+W would not target \
         it.\nGrid:\n{}",
        deck.snapshot_grid()
    );
    deck.send_keys(b"\x17");
    deck.wait_for_string("Close selected pane?");
    let armed = deck.snapshot_grid();
    assert!(
        armed.contains("Uncommitted work here is KEPT, not deleted:"),
        "closing a dispatched card whose worktree holds uncommitted work must say \
         so BEFORE the user answers — this is the whole of issue #717.\n{armed}"
    );
    assert!(
        armed.contains(&expected_worktree.to_string_lossy().into_owned()),
        "the warning must carry the path, because recovering the work means going \
         to it. Expected {}\n{armed}",
        expected_worktree.display()
    );

    // ===== …and again AFTER, because the dialog is gone the instant it is answered
    deck.send_keys(b"\x1b[B");
    deck.send_keys(b"\r");
    const CLOSE_WAIT: Duration = Duration::from_secs(30);
    assert!(
        common::wait_until(CLOSE_WAIT, || deck
            .snapshot_grid()
            .contains("KEPT, not deleted")),
        "after the close the status line must repeat where the work was kept — \
         otherwise the one fact the user needs vanished with the modal.\nGrid:\n{}",
        deck.snapshot_grid()
    );

    // And the claim must be TRUE: the deck said it kept the tree, so the tree is
    // there, with the uncommitted file still in it.
    assert!(
        common::wait_until(CLOSE_WAIT, || common::agent_records_on(
            deck.attach_socket_path()
        )
        .iter()
        .all(|r| r.display_name.as_deref() != Some(CARD))),
        "the dispatched agent was never stopped, so the removal path this test is \
         about never ran.\nGrid:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        expected_worktree.join(WORK).is_file(),
        "the deck promised the uncommitted work was KEPT at {} — it must still be \
         there. Keeping a dirty worktree is correct behaviour and issue #717 is \
         only about making it visible.",
        expected_worktree.display()
    );
}

/// `git status --porcelain` in `dir`, as the test's own independent reading of
/// what the deck is about to decide from.
fn porcelain(dir: &Path) -> String {
    let out = std::process::Command::new("git")
        .args(["-C", &dir.to_string_lossy(), "status", "--porcelain"])
        .output()
        .expect("git available");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Scenario: Dispatch a unit, dirty its worktree, and open the close confirmation
/// so the dialog warns that the work will be kept — then, WHILE the dialog is still
/// open, make the worktree clean again, exactly as a live agent committing its work
/// would. Confirming must report what actually happened rather than replaying the
/// dialog's now-stale prediction: no "kept" claim, and the worktree really removed.
#[spec("dispatch/close/003")]
#[test]
fn dispatch_close_003_a_worktree_cleaned_while_the_dialog_is_open_is_not_reported_as_kept() {
    const UNIT: &str = "stale-probe";
    const CARD: &str = "dispatch-stale-probe";
    const WORK: &str = "uncommitted-work-717-stale.txt";

    let scratch = common::race_safe_tempdir();
    let cfg = scratch.path().join("config.toml");
    std::fs::write(&cfg, "default_command = \"cat\"\n").expect("write the deck config");

    let deck = TuiDeck::builder()
        .with_pty_size(200, 50)
        .with_env("PATH", path_with_binary_dir())
        .with_env("DOT_AGENT_DECK_CONFIG", cfg.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");
    commit_fixture_repo(deck.workdir());

    let expected_worktree = dispatch_worktree_of(&deck, UNIT);
    let caller_pane = open_cat_caller_pane(&deck);
    let _guard = SiblingWorktreeGuard(expected_worktree.clone());

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["dispatch", UNIT, "--task", "Wait quietly.", "--single"])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", &caller_pane)
        .output()
        .expect("the dispatch CLI should run");
    assert!(out.status.success(), "`dispatch --single` failed: {out:?}");

    assert!(
        common::wait_until(Duration::from_secs(60), || expected_worktree.is_dir()
            && deck.snapshot_grid().contains(CARD)),
        "the dispatch never produced a worktree with a surfaced card.\nGrid:\n{}",
        deck.snapshot_grid()
    );
    // Premise: a freshly dispatched worktree is CLEAN, so the single file below
    // is the only thing making it dirty and removing it makes it clean again.
    // Without this the "cleaned up" half could be silently untestable.
    assert_eq!(
        porcelain(&expected_worktree),
        "",
        "a freshly dispatched worktree must start clean for this test to mean anything"
    );
    std::fs::write(expected_worktree.join(WORK), "work, about to be committed")
        .expect("dirty the dispatched worktree");

    deck.send_keys(b"\x04");
    deck.wait_for_string("COMMAND");
    confirm_close_selected(&deck); // close the caller card first
    assert!(
        common::wait_until(Duration::from_secs(30), || {
            let g = deck.snapshot_grid();
            !g.contains("caller") && g.contains(CARD)
        }),
        "the caller card did not close.\nGrid:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            let g = deck.snapshot_grid();
            g.lines()
                .any(|l| l.contains('\u{25b8}') && l.contains(CARD))
        }),
        "the dispatched card is not the selected one.\nGrid:\n{}",
        deck.snapshot_grid()
    );

    // Arm the confirmation while the tree IS dirty — the dialog is right to warn.
    deck.send_keys(b"\x17");
    deck.wait_for_string("Close selected pane?");
    let armed = deck.snapshot_grid();
    assert!(
        armed.contains("Uncommitted work here is KEPT, not deleted:"),
        "premise: the dialog must warn while the tree is genuinely dirty\n{armed}"
    );

    // …then the world moves under it, exactly as a live agent committing its
    // work does. The dialog's answer is now stale.
    std::fs::remove_file(expected_worktree.join(WORK)).expect("clean the worktree");
    assert_eq!(
        porcelain(&expected_worktree),
        "",
        "premise: the worktree must be clean again before the close is confirmed"
    );

    deck.send_keys(b"\x1b[B");
    deck.send_keys(b"\r");

    // The deck must follow the world, not its own earlier guess: a clean tree is
    // REMOVED, so nothing may claim the work was saved somewhere.
    const CLOSE_WAIT: Duration = Duration::from_secs(60);
    assert!(
        common::wait_until(CLOSE_WAIT, || !expected_worktree.exists()),
        "the worktree was clean when the close landed, so it must have been \
         removed. Still present at {}\nGrid:\n{}",
        expected_worktree.display(),
        deck.snapshot_grid()
    );
    let grid = deck.snapshot_grid();
    assert!(
        !grid.contains("KEPT, not deleted"),
        "the close reported work KEPT at a path it had just deleted — the dialog's \
         arm-time prediction was replayed as if it were what happened. The report \
         after a close must come from the daemon's own post-cleanup verdict, which \
         is measured with the agent already reaped and cannot go stale.\nGrid:\n{grid}"
    );
}

/// Scenario: Launch the deck on the `orch-multi` fixture — two spawnable
/// orchestrations where the SECOND, `gpt-side`, both `extends` the first and
/// declares `default = true` — open one ordinary `cat` pane to dispatch from,
/// then run the REAL `dot-agent-deck dispatch --list-targets` CLI against the
/// deck's own hook socket exactly as an agent in that pane would. The printed
/// listing must offer both orchestrations, must mark `gpt-side` as the default
/// rather than the one that comes first, must report it as having inherited its
/// two roles, and must carry no "chosen because it comes first" note. Then
/// dispatch with the BARE `--orchestration=` form and require the tab that
/// surfaces to be `gpt-side` too.
#[spec("orchestration/dispatch/004")]
#[test]
fn orchestration_dispatch_004_list_targets_marks_the_declared_default() {
    const UNIT: &str = "default-probe";

    let deck = TuiDeck::builder()
        .with_env("PATH", path_with_binary_dir())
        .launch_with_fixture("orch-multi");
    deck.wait_for_string("No active sessions");
    commit_fixture_repo(deck.workdir());
    let caller_pane = open_cat_caller_pane(&deck);

    // The READ-ONLY half: what a dispatcher agent is shown before it chooses.
    let listed = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["dispatch", "--list-targets"])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", &caller_pane)
        .output()
        .expect("the list-targets CLI should run");
    assert!(
        listed.status.success(),
        "`dispatch --list-targets` failed: {}{}",
        String::from_utf8_lossy(&listed.stdout),
        String::from_utf8_lossy(&listed.stderr)
    );
    let rendered = String::from_utf8_lossy(&listed.stdout).into_owned();

    let line_for = |name: &str| {
        rendered
            .lines()
            .find(|l| l.contains(&format!("'{name}'")))
            .unwrap_or_else(|| {
                panic!("`{name}` must appear in the listing:\n{rendered}");
            })
            .to_string()
    };
    let claude_line = line_for("claude-side");
    let gpt_line = line_for("gpt-side");
    assert!(
        gpt_line.contains("[default]") && !claude_line.contains("[default]"),
        "the DECLARED default must be the marked one. Marking the first entry instead would pass \
         a listing that merely echoes file order, which is the state issue #704 is about:\n\
         {rendered}"
    );
    assert!(
        !rendered.contains("comes first in the file"),
        "a config that declares its default must produce no ambiguity note — a permanent one on \
         every listing is noise that trains the reader to skip it:\n{rendered}"
    );
    assert!(
        gpt_line.contains("2 roles"),
        "`gpt-side` restates ONE role and inherits the rest through `extends`, so a count of 2 is \
         the daemon's own config load proving the inheritance resolved (issue #705):\n{gpt_line}"
    );

    // The ACTING half: the same answer, through the spawn. A listing that says
    // one thing while the dispatch does another is the disagreement #704 is
    // about, so the two are asserted in one test rather than two.
    let expected_worktree = dispatch_worktree_of(&deck, UNIT);
    let _guard = SiblingWorktreeGuard(expected_worktree.clone());
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args([
            "dispatch",
            UNIT,
            "--task",
            "Say hello, then stop.",
            // The BARE form: "whatever this repo's default is". `=` with an empty
            // value is how clap expresses an optional-value flag given no value.
            "--orchestration=",
        ])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", &caller_pane)
        .output()
        .expect("the dispatch CLI should run");
    assert!(
        out.status.success(),
        "a bare `--orchestration=` dispatch failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    const TAB_WAIT: Duration = Duration::from_secs(90);
    assert!(
        common::wait_until(TAB_WAIT, || {
            common::agent_records_on(deck.attach_socket_path())
                .iter()
                .any(|r| {
                    matches!(
                        &r.tab_membership,
                        Some(dot_agent_deck::agent_pty::TabMembership::Orchestration { name, .. })
                            if name == "gpt-side"
                    )
                })
        }),
        "the bare dispatch opened something other than the declared default within {}s — the \
         listing and the spawn must not disagree.\nRecords: {:?}\nFinal grid:\n{}",
        TAB_WAIT.as_secs(),
        common::agent_records_on(deck.attach_socket_path())
            .iter()
            .map(|r| (r.id.clone(), r.tab_membership.clone()))
            .collect::<Vec<_>>(),
        deck.snapshot_grid()
    );
}
