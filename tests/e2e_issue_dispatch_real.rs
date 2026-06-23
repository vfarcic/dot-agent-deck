#![cfg(feature = "e2e")]

//! L2 REAL-scenario showcase for GitHub issue-dispatch of an ORCHESTRATION
//! (PRD #120, CLAUDE.md rule 4 real-scenario policy). This is the heaviest,
//! flaky-tolerant pre-PR tier: it drives the genuine `gh` → clone → per-issue
//! worktree → real-agent path against LIVE GitHub with REAL cheap-model (Haiku)
//! agents — NO `gh` stub, NO `cat` stand-in.
//!
//! Where `scheduler/dispatch/011` (in `e2e_issue_dispatch_live.rs`) composes an
//! OFFLINE `gh` stub with a `cat` SINGLE-AGENT card to prove the live-surfacing
//! plumbing deterministically, this test proves the thing a stand-in never can,
//! AND for the multi-agent ORCHESTRATION path: that the daemon really cloned a
//! repo whose `.dot-agent-deck.toml` declares an `[[orchestrations]]` block, and
//! the dispatched ORCHESTRATION surfaces LIVE — as an orchestration TAB with its
//! orchestrator + worker role panes — on the already-attached TUI, without a
//! reconnect or relaunch.
//!
//! ## The production gap this test pins (the RED signal)
//! Today the daemon's spawn path live-surfaces only SINGLE-AGENT cards: the
//! orchestration branch in `spawn::spawn` deliberately does NOT call
//! `surface_spawned_pane` (see the comment at that call site), and an
//! orchestration TAB is only ever rebuilt from the daemon's `tab_membership`
//! records at TUI hydration (startup / reconnect). So when an `issue_dispatch`
//! fire opens an ORCHESTRATION, the orchestrator + worker panes do boot and emit
//! their own `SessionStart` hooks — which the live `apply_event` path paints as
//! FLAT dashboard cards — but NO orchestration tab appears live. The user has to
//! detach and reattach to see the orchestration grouped into its tab. This test
//! HARD-ASSERTS the live orchestration tab (label `issue-work`, from the
//! fixture's `[[orchestrations]] name`) so it is RED until the daemon learns to
//! surface a dispatched orchestration tab to attached TUIs.
//!
//! ## Fixture (permanent, created out-of-band)
//! The public repo `vfarcic/dot-agent-deck-tests` carries: a committed
//! `DISPATCH_E2E_SENTINEL.md`; a `.dot-agent-deck.toml` with an
//! `[[orchestrations]] name = "issue-work"` whose roles are `orchestrator`
//! (`start = true`) and `worker`, BOTH `claude --model
//! claude-haiku-4-5-20251001 --allowedTools Bash`, the orchestrator's
//! `prompt_template` instructing it to delegate the task to the worker via
//! `dot-agent-deck delegate --to worker --task "<TASK>"`; and a PERMANENT open
//! issue #1 labelled `agent-dispatch-test`. The schedule filters on that label
//! with `max_per_run = 1`, so ONLY issue #1 is ever enumerated — the run is
//! deterministic against a fixed remote.
//!
//! Seams it exercises end to end:
//!   - REAL `gh` on the normal PATH (no stub): the lazily-spawned daemon really
//!     enumerates (`gh issue list`), checks for an in-flight PR (`gh pr list`),
//!     and clones (`gh repo clone`) against live GitHub. Auth is threaded via
//!     `GITHUB_TOKEN` (this environment's `gh` auth) — the harness scrubs the
//!     spawned deck's env to a pinned set, so the token is passed explicitly so
//!     the daemon and the `gh` it runs inherit it.
//!   - REAL cheap agents: the clone's `[[orchestrations]]` resolves to two
//!     FULLY INTERACTIVE `claude` role panes pinned to Haiku (NO `-p`), so the
//!     focused orchestration tab shows the genuine live claude TUIs working —
//!     exactly what a user sees. The deck builder seeds project-trust into the
//!     per-test HOME (via `with_claude_project_trust`) for the per-issue worktree
//!     cwd the BOTH role panes share, so claude's first-run onboarding + trust
//!     gates clear with no human keystroke and the injected prompt is not
//!     swallowed.
//!   - The `dot-agent-deck` BINARY on the agents' PATH: the orchestrator role
//!     runs `dot-agent-deck delegate --to worker` to hand the task off, so the
//!     freshly-built binary's dir is prepended to the PATH the deck (→ daemon →
//!     agents) inherits — `with_env("PATH", …)` wins over the harness's scrub.
//!   - The live-fire seam from `scheduler/live/*`: the fire is driven by `RunNow`
//!     over the deck's attach socket (no real cron wait).
//!
//! PRD #120 ships `issue_dispatch` behind the `experimental` flag, so the deck
//! env sets `DOT_AGENT_DECK_EXPERIMENTAL=1`.
//!
//! NO REMOTE WRITES: the dispatch creates the branch `agent/issue-1` only in the
//! local tempdir clone and never pushes; the task is a list-files directive, so
//! neither agent must push or open a PR. The test asserts (best-effort) that the
//! fixture repo has no `agent/issue-1` branch after the run. All on-disk state
//! (clone + worktree) lives under a `tempfile::tempdir()` removed on drop.
//!
//! Decision 23 cost: two short interactive Haiku turns (orchestrator delegates,
//! worker lists a handful of files) — well under the <$0.05/run bound.
//! Local-only (Decision 8 / rule 5): gated on the `e2e` feature so CI's
//! `cargo test-fast` never compiles it; flaky-tolerant (real LLM + real network)
//! per rule 4 — it is NOT looped for flakiness.

mod common;

use std::path::Path;
use std::time::Duration;

use common::TuiDeck;
use spec::spec;

const FIXTURE_REPO: &str = "vfarcic/dot-agent-deck-tests";
const FIXTURE_LABEL: &str = "agent-dispatch-test";
const SCHEDULE_NAME: &str = "github-issues";
const SENTINEL: &str = "DISPATCH_E2E_SENTINEL.md";
/// The fixture's `[[orchestrations]] name` — the label the live orchestration
/// TAB renders with in the tab strip. NOT the schedule/display name
/// (`github-issues`, which titles the flat per-role cards) and NOT the worktree
/// basename (`issue-1`), so its appearance on the grid is an unambiguous signal
/// that the orchestration TAB surfaced — not that a flat role card did.
const ORCH_TAB_NAME: &str = "issue-work";

/// The schedule's `[[scheduled_tasks]]` + `[scheduled_tasks.issue_dispatch]`
/// fixture the lazily-spawned daemon loads via `DOT_AGENT_DECK_SCHEDULES`. The
/// cron never fires on its own (Jan 1 00:00) — the fire is driven by `RunNow`.
/// `label` + `max_per_run = 1` pin enumeration to the single permanent issue #1.
/// The `prompt` is the TASK handed to the orchestrator (which it must delegate
/// to the worker): a directive list-files command robust to LLM phrasing.
fn dispatch_schedule_toml(working_dir: &str) -> String {
    format!(
        "[[scheduled_tasks]]\n\
         name = \"{SCHEDULE_NAME}\"\n\
         cron = \"0 0 1 1 *\"\n\
         working_dir = \"{working_dir}\"\n\
         prompt = \"List every file in the current directory using the Bash tool (ls -a) and print each filename verbatim, one per line, with no other commentary.\"\n\
         enabled = true\n\
         \n\
         [scheduled_tasks.issue_dispatch]\n\
         repo = \"{FIXTURE_REPO}\"\n\
         label = \"{FIXTURE_LABEL}\"\n\
         max_per_run = 1\n\
         \n"
    )
}

/// PATH for the spawned deck (→ daemon → agents) with the freshly-built
/// `dot-agent-deck` binary's dir prepended to the host PATH. The orchestrator
/// role runs `dot-agent-deck delegate --to worker`, so the binary must resolve;
/// the rest of the host PATH is preserved so `gh`, `git`, and `claude` still
/// resolve. `CARGO_BIN_EXE_dot-agent-deck` is set by Cargo at integration-test
/// build time to the binary under test (same value the harness launches).
fn path_with_binary_dir() -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bindir = Path::new(bin)
        .parent()
        .expect("binary path has a parent dir");
    format!(
        "{}:{}",
        bindir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// Fire a registered task on the deck's own daemon via the `RunNow` control
/// message over the attach socket (mirrors `dispatch/011` / `scheduler/live/*`).
fn run_now(deck: &TuiDeck, name: &str) {
    common::attach_request_on(
        deck.attach_socket_path(),
        &dot_agent_deck::daemon_protocol::AttachRequest::RunNow {
            name: name.to_string(),
        },
    )
    .unwrap_or_else(|e| panic!("RunNow {name} over the attach socket failed: {e}"));
}

/// Runtime-skip helper (Decision 26): this environment authenticates `gh` via
/// the `GITHUB_TOKEN` env var. Absent → the daemon's `gh` cannot enumerate/clone,
/// so the test is environmentally inapplicable rather than broken.
fn github_token() -> Result<String, String> {
    match std::env::var("GITHUB_TOKEN") {
        Ok(t) if !t.trim().is_empty() => Ok(t),
        _ => Err("GITHUB_TOKEN not set — real-GitHub issue-dispatch e2e needs gh auth".into()),
    }
}

/// Best-effort probe of the LIVE fixture remote: does the per-issue branch
/// `agent/issue-1` exist on GitHub? `Some(true)` = present (a remote write
/// leaked — a failure), `Some(false)` = confirmed absent, `None` = the `gh`
/// probe itself failed (network/transient) so we can't conclude either way.
/// Runs `gh` from the TEST process, which inherits the host's real env + auth.
fn remote_branch_exists(branch: &str) -> Option<bool> {
    let out = std::process::Command::new("gh")
        .args([
            "api",
            &format!("repos/{FIXTURE_REPO}/branches/{branch}"),
            "--silent",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if out.status.success() {
        return Some(true);
    }
    // gh exits non-zero for a 404 (branch absent) — distinguish that from a
    // network/auth failure by inspecting stderr for the GitHub "Not Found".
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("Not Found") || stderr.contains("404") {
        Some(false)
    } else {
        None
    }
}

/// Scenario: Launch the deck attached to a daemon configured with one enabled
/// `issue_dispatch` schedule that targets the LIVE public fixture repo
/// `vfarcic/dot-agent-deck-tests` (filtered to the permanent label-gated issue
/// #1, `max_per_run = 1`) whose `.dot-agent-deck.toml` declares an
/// `[[orchestrations]] name = "issue-work"` with an `orchestrator` (start) and a
/// `worker` role, both interactive Haiku `claude`, then fire it via `RunNow` over
/// the attach socket WITHOUT detaching. With the REAL `gh` on PATH and
/// `GITHUB_TOKEN` threaded through, the daemon enumerates, clones from GitHub,
/// creates the per-issue worktree `…/.worktrees/issue-1`, and spawns the
/// orchestration's role panes into it (the per-issue worktree cwd, shared by both
/// roles, was pre-trusted in the per-test HOME so claude's first-run gates clear
/// without a keystroke; the built binary's dir is on PATH so the orchestrator's
/// `dot-agent-deck delegate --to worker` resolves). After the dispatched agents
/// register under the schedule name (precondition — proves the live clone +
/// worktree + spawn happened), HARD-ASSERT that the dispatched ORCHESTRATION
/// surfaces LIVE as an orchestration TAB labelled `issue-work` in the
/// already-attached TUI's tab strip, with no reconnect/relaunch. This is RED
/// today: the orchestration spawn branch does not call `surface_spawned_pane` and
/// orchestration tabs are only rebuilt at hydration, so the role panes appear
/// only as flat dashboard cards and no `issue-work` tab ever paints live. Once
/// GREEN, best-effort: switch to the orchestration tab and observe the worker
/// (delegated to by the orchestrator) list the cloned repo's files including the
/// committed sentinel `DISPATCH_E2E_SENTINEL.md` (logged, not asserted — too
/// LLM/timing-dependent to gate on), and assert (best-effort) the fixture repo has
/// no pushed `agent/issue-1` branch. Reel-eligible (PTY-attached, records a
/// `full-stream.cast`); flaky-tolerant (real LLM + real network) — run once, not
/// looped.
#[spec("scheduler/dispatch/013")]
#[test]
fn dispatch_013_orchestration_surfaces_and_delegates() {
    // Decision 26 runtime-skip: a missing CLI / credentials / token is an
    // environmental condition, not a broken test.
    skip_unless!(common::check_claude_available());
    let token = match github_token() {
        Ok(t) => t,
        Err(reason) => {
            // Same shape as `skip_unless!`'s early return (print `SKIP:` +
            // return) for the token precondition.
            eprintln!("SKIP: {reason}");
            return;
        }
    };

    // Workspace root where the daemon provisions the clone
    // (`<work>/github-issues`) and its per-issue worktree
    // (`<work>/github-issues/.worktrees/issue-1`). A scratch tempdir removed on
    // drop, so no clone/worktree leaks past the test.
    let work_td = tempfile::tempdir().expect("workspace tempdir");
    let work = work_td.path().join("ws");
    std::fs::create_dir_all(&work).expect("create workspace root");
    let work_str = work.to_string_lossy().into_owned();

    // The schedule fixture the lazily-spawned daemon loads via
    // `DOT_AGENT_DECK_SCHEDULES` (inherited from the deck's env). NO scratch
    // `default_command` config here (unlike the single-agent `dispatch/011`): the
    // orchestration's role commands come from the CLONED repo's
    // `.dot-agent-deck.toml`, not from `default_command`.
    let sched_td = tempfile::tempdir().expect("schedules tempdir");
    let sched_path = sched_td.path().join("schedules.toml");
    std::fs::write(&sched_path, dispatch_schedule_toml(&work_str)).expect("write schedules.toml");

    // The EXACT per-issue worktree cwd BOTH orchestration role panes (orchestrator
    // + worker) run in — they share the orchestration cwd. Derived the same way
    // the daemon does: `std::path::absolute` of the workspace root (the daemon's
    // `canonical_workspace` — lexical, no symlink resolution) →
    // `derive_issue_paths(.., SCHEDULE_NAME, 1)`. Seeding trust for this one path
    // covers both roles (shared cwd) so each interactive claude clears its
    // first-run trust dialog and the injected prompt isn't swallowed. Seeded
    // BEFORE the dispatch fires (the builder writes it into the per-test HOME at
    // launch).
    let workspace_abs = std::path::absolute(&work).expect("absolutize workspace root");
    let worktree_cwd =
        dot_agent_deck::issue_dispatch::derive_issue_paths(&workspace_abs, SCHEDULE_NAME, 1)
            .worktree_dir
            .to_string_lossy()
            .into_owned();

    let deck = TuiDeck::builder()
        // Real Claude credentials so the daemon-spawned interactive `claude` role
        // panes authenticate.
        .with_imported_claude_credentials()
        // Pre-trust the per-issue worktree cwd (shared by both roles) so each
        // interactive claude clears its first-run onboarding + trust gates and
        // auto-submits its injected prompt.
        .with_claude_project_trust(worktree_cwd)
        .with_env("DOT_AGENT_DECK_SCHEDULES", sched_path.to_string_lossy())
        // The orchestrator role runs `dot-agent-deck delegate --to worker`; put
        // the freshly-built binary's dir on the PATH the deck → daemon → agents
        // inherit (this override wins over the harness's env scrub). Preserves the
        // host PATH so `gh` / `git` / `claude` still resolve.
        .with_env("PATH", path_with_binary_dir())
        // The harness scrubs the spawned deck's env to a pinned set, so thread
        // GITHUB_TOKEN explicitly — the daemon and the real `gh` it runs inherit
        // it for enumerate/clone against live GitHub.
        .with_env("GITHUB_TOKEN", token)
        // PRD #120 ships issue_dispatch behind the experimental flag; turn it ON.
        .with_env("DOT_AGENT_DECK_EXPERIMENTAL", "1")
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    // Fire the dispatch into the SAME daemon this TUI is attached to.
    run_now(&deck, SCHEDULE_NAME);

    // Precondition (daemon side): the dispatch flow really enumerated, cloned
    // from GitHub, created the worktree, and spawned the orchestration's role
    // agents under the schedule's friendly name (every role pane carries it as
    // its display name). A generous timeout absorbs the live clone + `gh`
    // round-trips. This isolates the showcase below to the attached TUI's live
    // surfacing — the registry holds the agents regardless of whether a tab
    // paints.
    assert!(
        common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            SCHEDULE_NAME,
            true,
            Duration::from_secs(120),
        ),
        "the daemon must clone from GitHub + worktree + spawn the dispatched \
         orchestration's role agents under the schedule name '{SCHEDULE_NAME}' \
         (precondition for the showcase)"
    );

    // The load-bearing showcase assertion (RED today): the dispatched
    // ORCHESTRATION must surface LIVE as an orchestration TAB on the
    // already-attached dashboard — its tab-strip label is the fixture's
    // `[[orchestrations]] name`, `issue-work`. The tab strip is only shown once a
    // second tab exists (`TabManager::show_tab_bar` → `tabs.len() > 1`), so seeing
    // `issue-work` on the grid means a real orchestration tab was created live.
    //
    // Scanned over the cumulative byte history (not a single grid frame). Today
    // this FAILS: `spawn::spawn`'s orchestration branch does not call
    // `surface_spawned_pane`, and orchestration tabs are rebuilt only at TUI
    // hydration — so the role panes' own `SessionStart` hooks paint FLAT
    // dashboard cards (titled `github-issues`, dir `issue-1`) but no `issue-work`
    // tab ever appears without a reconnect. The coder's live-orchestration-
    // surfacing change must make this pass.
    let surfaced = deck.wait_for_stream_string_within(ORCH_TAB_NAME, Duration::from_secs(90));
    assert!(
        surfaced,
        "the dispatched orchestration never surfaced a LIVE tab labelled \
         {ORCH_TAB_NAME:?} within 90s — expected the orchestration tab (with its \
         orchestrator + worker role panes) to appear in the attached TUI's tab \
         strip without a reconnect. RED today: the orchestration spawn branch does \
         not live-surface, so only flat per-role dashboard cards appear.\n\
         Final grid:\n{}",
        deck.snapshot_grid()
    );

    // ---- Below here runs only once the feature lands (GREEN). It is the reel
    // narrative — best-effort, NOT gating, so LLM/timing variance can't make the
    // load-bearing assertion above flaky. ----

    // Switch from the Dashboard to the freshly-surfaced orchestration tab (`l` =
    // "move right / next tab") so its role panes render — the orchestrator
    // delegating to the worker, the worker listing the cloned repo's files.
    deck.send_keys(b"l");

    // The committed sentinel filename should appear once the worker (delegated to
    // by the orchestrator) runs `ls` inside the cloned per-issue worktree. Logged,
    // not asserted: the multi-agent delegation chain is too LLM/timing-dependent
    // to hard-gate (per the task's reel-narrative guidance).
    let saw_sentinel = deck.wait_for_stream_string_within(SENTINEL, Duration::from_secs(150));
    eprintln!(
        "reel narrative (soft): sentinel {SENTINEL:?} seen in orchestration panes = {saw_sentinel}"
    );

    // NO REMOTE WRITES: a list-files directive must not push a branch or open a
    // PR. Confirm (best-effort) the fixture remote has no `agent/issue-1` branch.
    // `None` (the gh probe itself failed) is tolerated — only a definitive
    // present branch is a failure.
    if let Some(true) = remote_branch_exists("agent/issue-1") {
        panic!(
            "the fixture repo {FIXTURE_REPO} has a pushed branch 'agent/issue-1' after the run — \
             the dispatch/agents must NOT push (they operate only in the local tempdir clone)"
        );
    }

    drop(work_td);
    drop(sched_td);
}
